"""Kill the owned XCTest bridge at tap boundaries and verify a usable replacement."""
import argparse
from contextlib import contextmanager
from dataclasses import asdict
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time

from native_clock_control import atomic_json
from native_controller_recovery import invoke_cli, controller_environment
from native_ios_app import find_ui
from native_ios_control import ClockClient
from native_ios_discovery import app_container
from native_ios_lifecycle import archive, stopped
from native_ios_transport import exchange
from native_ios_ui import UIClient, UIRequest


PHASES = ('before-tap', 'after-tap', 'after-receipt')


@contextmanager
def session(command, directory):
    directory.mkdir(parents=True, exist_ok=True)
    command = list(command)
    command[command.index('-resultBundlePath') + 1] = str(directory/'test.xcresult')
    with (directory/'test.log').open('wb') as log:
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
        state = dict(command=command, pid=process.pid, started_monotonic_ns=time.monotonic_ns())
        atomic_json(directory/'session.json', state)
        try:
            yield process
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
                state['forced_stop'] = True
            state.update(exit_code=process.returncode, finished_monotonic_ns=time.monotonic_ns())
            atomic_json(directory/'session.json', state)


def kill_bridge(simulator, descriptor, request, cli, directory, row):
    marker_path = descriptor.with_name('ui-checkpoint.json')
    with (directory/'tap.stdout').open('wb') as stdout, (directory/'tap.stderr').open('wb') as stderr:
        process = subprocess.Popen([*cli, 'tap', '--arguments', json.dumps(request.arguments)],
                                   env=controller_environment(), stdout=stdout, stderr=stderr)
        row['controller_pid'] = process.pid
        try:
            deadline = time.monotonic() + 15
            while True:
                if marker_path.exists():
                    marker = json.loads(marker_path.read_text())
                    if (marker.get('request') == asdict(request) and marker.get('phase') == row['phase']
                            and marker.get('status') == 'waiting' and marker.get('pid') == row['launch']['pid']):
                        break
                if process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(f'XCTest checkpoint unavailable: {row["phase"]}, child={process.poll()}')
                time.sleep(.02)
            row['checkpoint'] = marker
            # A receipt alone is not authority to signal a process. Bind it to this owned
            # simulator's installed XCTest executable immediately before SIGKILL.
            identity = subprocess.run(['ps', '-p', str(marker['pid']), '-o', 'uid=', '-o', 'lstart=', '-o', 'command='],
                                      check=True, capture_output=True, text=True, timeout=3).stdout.strip()
            row['identity'] = identity
            if (not identity.startswith(str(os.getuid())+' ') or '/'+simulator+'/' not in identity
                    or '/NativeProbeTests-Runner.app/NativeProbeTests-Runner' not in identity):
                raise RuntimeError('Checkpoint PID is not the owned XCTest executable: '+identity)
            if json.loads(marker_path.read_text()) != marker:
                raise RuntimeError('XCTest checkpoint expired before interruption')
            atomic_json(directory/'before-kill.json', dict(request=asdict(request), checkpoint=marker, identity=identity))
            os.kill(marker['pid'], signal.SIGKILL)
            row['signal'] = signal.SIGKILL
            row['controller_exit'] = process.wait(timeout=5)
            row['checkpoint_after'] = json.loads(marker_path.read_text())
            row['receipt'] = json.loads(descriptor.read_text())
            row['stopped'] = stopped(row['launch'])
            if (row['controller_exit'] == 0 or marker != row['checkpoint_after']
                    or not row['stopped']['process_absent'] or not row['stopped']['port_closed']):
                raise RuntimeError('XCTest did not die at the requested checkpoint')
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)


def run(simulator, out, command, hold_us):
    root = out/'xctest-recovery'
    root.mkdir()
    report = dict(simulator=simulator, hold_us=hold_us, cases=[], progress='starting')
    previous = None
    def save(progress):
        report['progress'] = progress
        atomic_json(root/'report.json', report)
    try:
        for phase in PHASES:
            row = dict(phase=phase)
            report['cases'].append(row)
            directory = root/phase
            directory.mkdir()
            save(phase+'-old-launch')
            with session(command, directory/'old-session') as process:
                descriptor = find_ui(simulator, process, directory, previous)
                old = UIClient(descriptor, directory/'old-controller')
                row['launch'] = old.request('launch')
                previous = row['launch']['generation']
                documents = app_container(simulator, 'dev.frontcap.s18.probe', directory)/'Documents'
                endpoint = documents/'clock-control.json'
                row['old_clock'] = ClockClient(endpoint, directory/'old-clock').request('read')
                request = UIRequest(previous, 2, 'tap', dict(target='native-increment', stop_at=phase))
                row['request'] = asdict(request)
                cli = [sys.executable, str(Path(__file__).with_name('native_ios_ui.py')),
                       '--descriptor', str(descriptor), '--directory', str(directory/'old-controller')]
                kill_bridge(simulator, descriptor, request, cli, directory, row)
                archive(descriptor.parent, directory/'old-ui')
                archive(documents, directory/'old-app')
                row['pending_before'] = json.loads(old.path.read_text())
                row['recovery'] = invoke_cli(directory, cli, 'recover', 'recover', success=False)
                row['pending_after'] = json.loads(old.path.read_text())
                if (row['recovery']['exit_code'] == 0) != (phase == 'after-receipt'):
                    raise RuntimeError('Unexpected XCTest receipt recovery: '+phase)
                if phase == 'after-receipt':
                    offline = [*cli]; offline[offline.index('--descriptor')+1] = str(directory/'absent-descriptor')
                    row['outcome'] = invoke_cli(directory, offline, 'outcome', 'outcome')
                else:
                    row['blocked_input'] = invoke_cli(directory, cli, 'blocked', 'tap', '--arguments',
                                                     '{"target":"native-increment"}', success=False)
                    if row['blocked_input']['exit_code'] == 0:
                        raise RuntimeError('An unresolved tap admitted new input')
                save(phase+'-killed')
                row['xcodebuild_exit'] = process.wait(timeout=60)
                if not row['xcodebuild_exit'] or json.loads(descriptor.read_text()) != row['receipt']:
                    raise RuntimeError('XCTest death was masked by success or a restarted bridge')
            termination = subprocess.run(['xcrun', 'simctl', 'terminate', simulator, 'dev.frontcap.s18.probe'],
                                         capture_output=True, text=True, timeout=10)
            row['app_termination'] = dict(exit_code=termination.returncode, stdout=termination.stdout, stderr=termination.stderr)
            row['old_app_stopped'] = stopped(row['old_clock'])
            if not row['old_app_stopped']['process_absent'] or not row['old_app_stopped']['port_closed']:
                raise RuntimeError('Old fixture survived retirement')
            save(phase+'-replacement-launch')
            with session(command, directory/'new-session') as process:
                replacement = find_ui(simulator, process, directory, previous)
                fresh = UIClient(replacement, directory/'new-controller')
                row['replacement_launch'] = fresh.request('launch')
                previous = row['replacement_launch']['generation']
                row['replacement_descriptor'] = str(replacement)
                # Resolve the new installation independently; never use the old app's data path.
                documents = app_container(simulator, 'dev.frontcap.s18.probe', directory)/'Documents'
                clock = ClockClient(documents/'clock-control.json', directory/'new-clock')
                row['replacement_before'] = clock.request('read')
                receipt_before = replacement.read_bytes()
                old.descriptor = replacement
                try:
                    old.resume()
                except RuntimeError as error:
                    row['resume_error'] = str(error)
                else:
                    raise RuntimeError('Old UI request entered a replacement generation')
                row['replacement_receipt_unchanged'] = replacement.read_bytes() == receipt_before
                row['pending_final'] = json.loads(old.path.read_text())
                row['foreign_request'] = exchange(json.loads(replacement.read_text()), request)
                row['replacement_input'] = fresh.request('tap', target='native-increment')
                start = time.monotonic()
                time.sleep(hold_us/1e6)
                row['hold_s'] = time.monotonic() - start
                row['held'] = clock.request('read')
                row['captures'] = [fresh.request('capture', name=phase+'-frozen', clock='Time: 0', text='Pending')]
                row['advances'] = []
                for delta, name, instant, label in [(4999000000, 'before', 4999000000, 'Pending'),
                                                    (1000000, 'due', 5000000000, 'Count: 1')]:
                    row['advances'].append(clock.request('advance', delta))
                    row['captures'].append(fresh.request('capture', name=phase+'-'+name,
                                                         clock='Time: '+str(instant), text=label))
                row['finish'] = fresh.request('finish')
                row['replacement_app_stopped'] = stopped(row['replacement_before'])
                row['replacement_xcodebuild_exit'] = process.wait(timeout=40)
                row['replacement_bridge_stopped'] = stopped(row['replacement_launch'])
                archive(documents, directory/'new-app')
                archive(replacement.parent, directory/'new-ui')
                if row['replacement_xcodebuild_exit']:
                    raise RuntimeError('Replacement XCTest session failed')
            save(phase+'-finished')
        save('finished')
    except BaseException as error:
        report['error'] = f'{type(error).__name__}: {error}'
        save('failed')
        raise


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--simulator', required=True)
    parser.add_argument('--directory', required=True, type=Path)
    parser.add_argument('--hold-us', required=True, type=int)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command or args.hold_us <= 0:
        parser.error('A positive hold and xcodebuild command are required')
    run(args.simulator, args.directory, command, args.hold_us)
