"""Terminate the real app at bounded clock checkpoints; retain each generation's evidence."""
from dataclasses import asdict
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time

from native_clock_control import Request, atomic_json
from native_ios_control import ClockClient
from native_ios_transport import exchange
from native_ios_ui_recovery import recover_tap


PHASES = ('before-advance', 'after-advance', 'after-receipt')


def archive(documents, destination):
    destination.mkdir(parents=True, exist_ok=True)
    for path in documents.iterdir():
        if path.is_file():
            shutil.copy2(path, destination/path.name)


def stopped(endpoint):
    try:
        os.kill(endpoint['pid'], 0)
        absent = False
    except ProcessLookupError:
        absent = True
    try:
        with socket.create_connection(('127.0.0.1', endpoint['port']), timeout=.2):
            closed = False
    except ConnectionRefusedError:
        closed = True
    return dict(pid=endpoint['pid'], port=endpoint['port'], process_absent=absent, port_closed=closed)


def lifecycle(simulator, out, ui, get_container):
    report = dict(progress='starting', cases=[])
    def save(phase):
        report['progress'] = phase
        atomic_json(out/'clock-lifecycle-state.json', report)
    for phase in PHASES:
        row = dict(phase=phase)
        report['cases'].append(row)
        directory = out/'clock-lifecycle'/phase
        directory.mkdir(parents=True)
        save(phase)
        row['launch'] = ui.request('launch', clock_stop_at=phase)
        documents = get_container(simulator, 'dev.frontcap.s18.probe')/'Documents'
        endpoint = documents/'clock-control.json'
        client_directory = directory/'old-controller'
        old = ClockClient(endpoint, client_directory)
        row['input'] = (recover_tap(out, ui, 'before-pending' if phase == 'before-advance' else 'after-reply',
                                   row['launch'], f'clock-lifecycle/{phase}/old')
                        if phase != 'after-receipt' else ui.request('tap', target='native-increment'))
        row['before'] = old.request('read')
        pending = Request(row['before']['generation'], 2, 'advance', 1000000)
        row['request'] = asdict(pending)
        cli = [sys.executable, str(Path(__file__).with_name('native_ios_control.py')),
               '--descriptor', str(endpoint), '--directory', str(client_directory)]
        with (directory/'advance.stdout').open('wb') as stdout, (directory/'advance.stderr').open('wb') as stderr:
            process = subprocess.Popen(cli + ['advance', '--delta-ns', '1000000'], stdout=stdout, stderr=stderr)
            row['controller_pid'] = process.pid
            try:
                deadline = time.monotonic() + 8
                while True:
                    marker_path = documents/'clock-checkpoint.json'
                    if marker_path.exists():
                        marker = json.loads(marker_path.read_text())
                        if (marker.get('request') == asdict(pending) and marker.get('phase') == phase
                                and marker.get('status') == 'waiting' and marker.get('pid') == row['before']['pid']):
                            break
                    if process.poll() is not None or time.monotonic() >= deadline:
                        raise RuntimeError(f'Clock checkpoint unavailable: {phase}, child={process.poll()}')
                    time.sleep(.02)
                row['checkpoint'] = marker
                save(phase+'-waiting')
                row['terminate'] = ui.request('terminate')
                row['controller_exit'] = process.wait(timeout=5)
                row['stopped'] = stopped(row['before'])
                row['checkpoint_after'] = json.loads(marker_path.read_text())
                row['receipt'] = json.loads(endpoint.read_text())
                row['pending_before'] = json.loads(old.path.read_text())
                if (not row['controller_exit'] or not row['stopped']['process_absent']
                        or not row['stopped']['port_closed'] or row['checkpoint_after'] != marker):
                    raise RuntimeError('App did not terminate at the requested checkpoint')
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                archive(documents, directory/'old')
                save(phase+'-terminated')
        recovered = subprocess.run(cli + ['recover'], capture_output=True, text=True, timeout=10)
        (directory/'recover.stdout').write_text(recovered.stdout)
        (directory/'recover.stderr').write_text(recovered.stderr)
        row['recovery'] = dict(exit_code=recovered.returncode,
                               reply=json.loads(recovered.stdout) if recovered.returncode == 0 else None)
        row['pending_after'] = json.loads(old.path.read_text())
        if (recovered.returncode == 0) != (phase == 'after-receipt'):
            raise RuntimeError('Unexpected recorded-outcome recovery: '+phase)
        row['replacement_launch'] = ui.request('launch')
        fresh = ClockClient(endpoint, directory/'new-controller')
        row['replacement_before'] = fresh.request('read')
        before = endpoint.read_bytes()
        try:
            ClockClient(endpoint, client_directory).resume()
        except RuntimeError as error:
            row['resume_error'] = str(error)
        else:
            raise RuntimeError('Old controller unexpectedly resumed into replacement')
        row['replacement_receipt_unchanged'] = endpoint.read_bytes() == before
        row['pending_final'] = json.loads(old.path.read_text())
        # Deliberately bypass the controller as a separate negative backend control.
        row['foreign_request'] = exchange(json.loads(endpoint.read_text()), pending)
        row['replacement_input'] = (recover_tap(out, ui, 'after-pending' if phase == 'before-advance' else 'after-commit',
                                               row['replacement_launch'], f'clock-lifecycle/{phase}/new')
                                    if phase != 'after-receipt' else ui.request('tap', target='native-increment'))
        row['replacement_advance'] = fresh.request('advance', 1000000)
        row['replacement_after'] = fresh.request('read')
        row['capture'] = ui.request('capture', name='death-'+phase, clock='Time: 1000000', text='Pending')
        row['replacement_terminate'] = ui.request('terminate')
        row['replacement_stopped'] = stopped(row['replacement_after'])
        archive(documents, directory/'new')
        save(phase+'-finished')
    save('finished')
