"""Force-stop only the fixture at clock command boundaries, preserving each generation."""
from dataclasses import asdict
import json
from pathlib import Path
import subprocess
import sys
import time

from native_android_control import ClockClient, Request, exchange, read_receipt
from native_clock_control import atomic_json
from native_android_diagnostics import capture_retirement, capture_thread_ids, capture_retiring_threads
from native_android_processes import parse_inventory


PACKAGE = 'dev.frontcap.s18.probe'
PHASES = ('before-advance', 'after-advance', 'after-receipt')


def archive(command, directory, *, package=PACKAGE, state_name="state.json"):
    directory.mkdir(parents=True, exist_ok=True)
    for name in ('clock-control.json', 'clock-checkpoint.json', 'input-control.json', 'input-checkpoint.json', state_name):
        (directory/name).write_bytes(command('shell', 'run-as', package, 'cat', 'files/'+name))


def stop(command, pid, destination, timeout=2, *, package=PACKAGE, observe_zombie=False):
    threads = capture_thread_ids(command, pid, destination.with_name(destination.stem+'-threads-before.json'))
    force_stop_started_ns = time.monotonic_ns()
    command('shell', 'am', 'force-stop', package)
    force_stop_finished_ns = time.monotonic_ns()
    deadline = time.monotonic()+timeout
    observations = []
    observed_zombie = False
    processes = b''
    while True:
        remaining = deadline-time.monotonic()
        if remaining <= 0:
            capture_retirement(command, pid, processes.decode(),
                               destination.with_name(destination.stem+'-retirement.json'), package=package, thread_snapshot=threads)
            raise RuntimeError('Fixture retirement deadline elapsed before absence was observed')
        observation = dict(started_monotonic_ns=time.monotonic_ns(), timeout_s=remaining,
                           deadline_monotonic_s=deadline,
                           force_stop_started_monotonic_ns=force_stop_started_ns,
                           force_stop_finished_monotonic_ns=force_stop_finished_ns)
        try:
            processes = command('shell', 'ps', '-A', '-o', 'PID,PPID,STAT,NAME', timeout=remaining)
        except subprocess.TimeoutExpired as error:
            partial = error.output
            if isinstance(partial, bytes): partial = partial.decode(errors='replace')
            observations.append(dict(observation, monotonic_ns=time.monotonic_ns(),
                                     error=f'{type(error).__name__}: {error}', partial_output=partial))
            atomic_json(destination.with_suffix('.json'), observations)
            capture_retirement(command, pid, processes.decode(),
                               destination.with_name(destination.stem+'-retirement.json'), package=package, thread_snapshot=threads)
            raise
        observed_at = time.monotonic()
        observation['monotonic_ns'] = time.monotonic_ns()
        destination.write_bytes(processes)
        observations.append(dict(observation, processes=processes.decode()))
        atomic_json(destination.with_suffix('.json'), observations)
        try:
            rows = parse_inventory(processes.decode())
        except ValueError as error:
            raise RuntimeError('Fixture retirement inventory unavailable: '+str(error)) from error
        matching = [row['raw'] for row in rows if row['pid'] == pid or row['name'] == package]
        if observed_at > deadline:
            capture_retirement(command, pid, processes.decode(),
                               destination.with_name(destination.stem+'-retirement.json'), package=package, thread_snapshot=threads)
            raise RuntimeError('Fixture retirement observation arrived after deadline')
        if not matching:
            return dict(pid=pid, process_absent=True)
        if (observe_zombie and not observed_zombie and time.monotonic() < deadline
                and any(row['pid'] == pid and row['state'].startswith('Z') for row in rows)):
            observed_zombie = True
            capture_retiring_threads(command, pid,
                destination.with_name(destination.stem+'-first-zombie.json'), package=package,
                thread_snapshot=threads, inventory=processes.decode(),
                inventory_monotonic_ns=observations[-1]['monotonic_ns'], deadline=deadline)
        if time.monotonic() >= deadline:
            capture_retirement(command, pid, processes.decode(),
                               destination.with_name(destination.stem+'-retirement.json'), package=package, thread_snapshot=threads)
            raise RuntimeError('Fixture process still present after force-stop: '+repr(matching))
        # force-stop may return before the killed process is reaped. Require actual
        # PID absence, retaining each observation rather than treating a zombie as absent.
        time.sleep(min(.05, max(0, deadline-time.monotonic())))


def lifecycle(out, command, launch, observe, tap, adb):
    report = dict(progress='starting', cases=[])
    def save(progress):
        report['progress'] = progress
        atomic_json(out/'clock-lifecycle.json', report)
    try:
        for phase in PHASES:
            row = dict(phase=phase)
            report['cases'].append(row)
            directory = out/'clock-lifecycle'/phase
            directory.mkdir(parents=True)
            save(phase)
            row['launch'] = launch(phase).decode()
            tree, row['initial_screen'] = observe('death-'+phase+'-initial', 0)
            old = ClockClient(command, directory/'old-controller')
            row['input'] = tap(tree, 'native-increment')
            row['before'] = old.request('read')
            pending = Request(row['before']['generation'], 2, 'advance', 1000000)
            row['request'] = asdict(pending)
            cli = [sys.executable, str(Path(__file__).with_name('native_android_control.py')),
                   '--adb', adb[0], '--serial', adb[2], '--directory', str(old.directory)]
            with (directory/'advance.stdout').open('wb') as stdout, (directory/'advance.stderr').open('wb') as stderr:
                process = subprocess.Popen(cli+['advance', '--delta-ns', '1000000'], stdout=stdout, stderr=stderr)
                row['controller_pid'] = process.pid
                try:
                    deadline = time.monotonic()+4
                    while True:
                        marker = json.loads(command('shell', 'run-as', PACKAGE, 'cat', 'files/clock-checkpoint.json'))
                        if (marker.get('status') == 'waiting' and marker.get('phase') == phase
                                and marker.get('request') == asdict(pending) and marker.get('pid') == row['before']['pid']):
                            break
                        if process.poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError(f'Clock checkpoint unavailable: {phase}, child={process.poll()}')
                        time.sleep(.02)
                    row['checkpoint'] = marker
                    save(phase+'-waiting')
                    row['stopped'] = stop(command, marker['pid'], directory/'old-processes.txt')
                    row['controller_exit'] = process.wait(timeout=6)
                finally:
                    if process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait(timeout=5)
            archive(command, directory/'old')
            row['checkpoint_after'] = json.loads((directory/'old/clock-checkpoint.json').read_text())
            row['receipt'] = read_receipt(command, PACKAGE)
            row['pending_before'] = json.loads(old.path.read_text())
            if not row['controller_exit'] or row['checkpoint_after'] != marker:
                raise RuntimeError('App did not terminate at the requested checkpoint')
            recovered = subprocess.run(cli+['recover'], capture_output=True, text=True, timeout=10)
            (directory/'recover.stdout').write_text(recovered.stdout)
            (directory/'recover.stderr').write_text(recovered.stderr)
            row['recovery'] = dict(exit_code=recovered.returncode,
                                   reply=json.loads(recovered.stdout) if recovered.returncode == 0 else None)
            row['pending_after'] = json.loads(old.path.read_text())
            if (recovered.returncode == 0) != (phase == 'after-receipt'):
                raise RuntimeError('Unexpected recorded-outcome recovery: '+phase)
            save(phase+'-recovered')
            row['replacement_launch'] = launch().decode()
            tree, row['replacement_screen_before'] = observe('death-'+phase+'-replacement', 0)
            fresh = ClockClient(command, directory/'new-controller')
            row['replacement_before'] = fresh.request('read')
            before = command('shell', 'run-as', PACKAGE, 'cat', 'files/clock-control.json')
            try:
                ClockClient(command, old.directory).resume()
            except RuntimeError as error:
                row['resume_error'] = str(error)
            else:
                raise RuntimeError('Old controller resumed into replacement')
            row['replacement_receipt_unchanged'] = before == command('shell', 'run-as', PACKAGE, 'cat', 'files/clock-control.json')
            row['pending_final'] = json.loads(old.path.read_text())
            # Separate negative backend control; the recovered controller never sends this.
            row['foreign_request'] = exchange(command, PACKAGE, pending, directory/'foreign-command.jsonl')
            row['replacement_input'] = tap(tree, 'native-increment')
            row['replacement_advance'] = fresh.request('advance', 1000000)
            row['replacement_after'] = fresh.request('read')
            _, row['capture'] = observe('death-'+phase+'-after', 1)
            row['replacement_stopped'] = stop(command, row['replacement_after']['pid'], directory/'new-processes.txt')
            archive(command, directory/'new')
            save(phase+'-finished')
        save('finished')
    except BaseException as error:
        report['error'] = f'{type(error).__name__}: {error}'
        try:
            save('failed')
        except OSError as evidence_error:
            print(f'Cannot save lifecycle failure: {evidence_error}', file=sys.stderr)
        raise
