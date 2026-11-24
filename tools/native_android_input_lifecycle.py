"""Terminate the fixture during a real input; retain uncertainty and test a fresh generation."""
import json
from pathlib import Path
import signal
import subprocess
import sys
import time

from native_android_control import ClockClient
from native_android_input import InputClient, InputRequest, read_receipt, register_request
from native_android_input_interruption import PHASES
from native_android_lifecycle import PACKAGE, archive, stop
from native_android_tap import host_processes, matching_processes, retire_process, wait_for_exit
from native_clock_control import atomic_json
from native_controller_recovery import controller_environment, invoke_cli, lock_available


def pause_controller(process):
    """Freeze the owned parent before identifying children it might otherwise keep spawning."""
    if process.poll() is not None:
        raise RuntimeError('Input controller exited before interruption')
    process.send_signal(signal.SIGSTOP)
    deadline = time.monotonic()+2
    while True:
        rows = host_processes()
        parents = [p for p in rows if p['pid'] == process.pid]
        if len(parents) == 1 and 'T' in parents[0]['state']:
            return dict(parent=parents[0], children=[p for p in rows if p['ppid'] == process.pid],
                        observed_monotonic_ns=time.monotonic_ns(), signal=int(signal.SIGSTOP))
        if process.poll() is not None or time.monotonic() >= deadline:
            raise RuntimeError('Owned input controller did not stop for child inventory')
        time.sleep(.01)


def lifecycle(root, command, launch, observe, node, center, adb, *, mode='app'):
    if mode not in ('app', 'app-controller'):
        raise ValueError('Invalid app input interruption mode')
    joint = mode == 'app-controller'
    report = dict(mode=mode, progress='starting', cases=[], observe_retiring_threads=joint)
    def save(progress):
        report['progress'] = progress
        atomic_json(root/'input-lifecycle.json', report)
    def app_file(name):
        return command('shell', 'run-as', PACKAGE, 'cat', 'files/'+name)
    try:
        for phase in PHASES:
            directory = root/'input-lifecycle'/phase
            directory.mkdir(parents=True)
            row = dict(phase=phase)
            report['cases'].append(row)
            save(phase+'-launching')
            row['launch'] = launch(phase).decode()
            tree, row['initial_screen'] = observe('input-death-'+phase+'-initial', 0)
            row['before'] = read_receipt(command, PACKAGE)
            client = InputClient(command, directory/'old-controller')
            x, y = center(node(tree, 'native-increment'))
            cli = [sys.executable, str(Path(__file__).with_name('native_android_input.py')),
                   '--adb', adb[0], '--serial', adb[2], '--directory', str(client.directory), '--record-tap-process']
            process = None
            child = None
            owned = []
            host_marker_path = client.directory/'controller-checkpoint.json'
            environment = controller_environment()
            if joint and phase == 'after-receipt':
                environment['FC_CONTROLLER_STOP_AT'] = 'after-reply'
            try:
                with (directory/'tap.stdout').open('wb') as stdout, (directory/'tap.stderr').open('wb') as stderr:
                    started = time.monotonic()
                    process = subprocess.Popen([*cli, 'tap', '--x', str(x), '--y', str(y)],
                        env=environment, stdout=stdout, stderr=stderr)
                    row['controller_pid'] = process.pid
                    deadline = started+5
                    while True:
                        marker = json.loads(app_file('input-checkpoint.json'))
                        if (marker.get('status') == 'waiting' and marker.get('phase') == phase
                                and marker.get('generation') == row['before']['generation']
                                and marker.get('pid') == row['before']['pid']):
                            if not joint or phase != 'after-receipt':
                                break
                            if host_marker_path.exists():
                                host_marker = json.loads(host_marker_path.read_text())
                                if (host_marker.get('pid') == process.pid and host_marker.get('phase') == 'after-reply'
                                        and host_marker.get('status') == 'waiting' and host_marker.get('request') == marker['request']):
                                    row['controller_checkpoint'] = host_marker
                                    break
                        if process.poll() is not None or time.monotonic() >= deadline:
                            raise RuntimeError(f'Input app checkpoint unavailable: {phase}, child={process.poll()}')
                        time.sleep(.02)
                    row['checkpoint'] = marker
                    row['request'] = marker['request']
                    row['controller_exit_at_checkpoint'] = process.poll()
                    row['client_at_checkpoint'] = json.loads(client.path.read_text())
                    transport = json.loads((client.directory/'input-transport.json').read_text())
                    row['transport_at_checkpoint'] = transport
                    child = transport['identity']
                    if (transport['parent_pid'] != process.pid or transport['request'] != marker['request']
                            or transport['command'] != [*adb, 'shell', 'input', 'tap', str(x), str(y)]):
                        raise RuntimeError('Tap transport does not belong to this request')
                    row['host_at_checkpoint'] = [p for p in host_processes() if p['pid'] in (process.pid, child['pid'])]
                    row['receipt_at_checkpoint'] = read_receipt(command, PACKAGE)
                    row['state_at_checkpoint'] = json.loads(app_file('state.json'))
                    save(phase+'-waiting')
                    if joint:
                        row['controller_pause'] = pause_controller(process)
                        owned = row['controller_pause']['children']
                        row['lock_held'] = not lock_available(client.directory, 'input')
                        row['client_before_kill'] = json.loads(client.path.read_text())
                        row['tap_before_kill'] = matching_processes(host_processes(), child)
                        if (not row['lock_held'] or row['client_before_kill']['pending'] != marker['request']
                                or row['client_before_kill']['delivery_phase'] != 'dispatched'
                                or (phase == 'before-touch-up' and (len(row['tap_before_kill']) != 1
                                    or 'Z' in row['tap_before_kill'][0]['state']))):
                            raise RuntimeError('Pending input/controller or actual tap child unavailable before kill')
                        row['kill_started_monotonic_ns'] = time.monotonic_ns()
                        process.kill()
                        row['controller_exit'] = process.wait(timeout=5)
                        row['controller_finished_monotonic_ns'] = time.monotonic_ns()
                        row['tap_after_controller_death'] = matching_processes(host_processes(), child)
                        if phase == 'after-receipt':
                            row['controller_checkpoint_after'] = json.loads(host_marker_path.read_text())
                    row['stop_started_monotonic_ns'] = time.monotonic_ns()
                    row['stopped'] = stop(command, marker['pid'], directory/'old-processes.txt', observe_zombie=joint)
                    row['stop_finished_monotonic_ns'] = time.monotonic_ns()
                    if not joint:
                        row['controller_exit'] = process.wait(timeout=max(0, 15-(time.monotonic()-started)))
                        row['controller_finished_monotonic_ns'] = time.monotonic_ns()
                row['lock_released'] = lock_available(client.directory, 'input')
                row['transport_after'] = json.loads((client.directory/'input-transport.json').read_text())
                row['child_retirement'] = []
                if not wait_for_exit(child, row['child_retirement']):
                    raise RuntimeError('Tap child did not retire after app death')
                archive(command, directory/'old')
                row['checkpoint_after'] = json.loads((directory/'old/input-checkpoint.json').read_text())
                row['receipt'] = json.loads((directory/'old/input-control.json').read_text())
                row['pending_before'] = json.loads(client.path.read_text())
                if row['checkpoint_after'] != marker:
                    raise RuntimeError('App was not stopped at the requested input checkpoint')
                operation = 'recover' if row['pending_before']['pending'] else 'outcome'
                row['recovery_operation'] = operation
                row['recovery'] = invoke_cli(directory, cli, 'recover', operation, success=False)
                row['pending_after'] = json.loads(client.path.read_text())
                if (row['recovery']['exit_code'] == 0) != (phase == 'after-receipt'):
                    raise RuntimeError('Unexpected input resolution after app death: '+phase)
                save(phase+'-recovered')
            finally:
                actions = []
                if process is not None and process.poll() is None:
                    process.kill()
                    process.wait(timeout=5)
                    actions.append('killed owned controller after failure')
                row['cleanup_actions'] = actions
                if child:
                    row['child_cleanup'] = retire_process(child)
                    if not row['child_cleanup']['absent']:
                        raise RuntimeError('Owned tap child cleanup incomplete')
                if joint:
                    row['other_children_cleanup'] = [retire_process(p) for p in owned if child is None or p['pid'] != child['pid']]
                    if any(not p['absent'] for p in row['other_children_cleanup']):
                        raise RuntimeError('Other owned input child cleanup incomplete')
                save(phase+'-retired')
            row['replacement_launch'] = launch().decode()
            tree, row['replacement_screen_before'] = observe('input-death-'+phase+'-replacement', 0)
            fresh = InputClient(command, directory/'new-controller')
            clock = ClockClient(command, directory/'new-clock')
            row['replacement_before'] = read_receipt(command, PACKAGE)
            row['replacement_clock_before'] = clock.request('read')
            before = app_file('input-control.json')
            row['resume'] = invoke_cli(directory, cli, 'resume', 'resume', success=False)
            row['outcome_after_replacement'] = invoke_cli(directory, cli, 'outcome', 'outcome', success=False)
            row['pending_final'] = json.loads(client.path.read_text())
            row['replacement_receipt_unchanged'] = before == app_file('input-control.json')
            # Explicit backend rejection control, separate from the old client's receipt-only attempt.
            row['foreign_registration'] = register_request(command, PACKAGE, InputRequest(**row['request']))
            row['foreign_receipt_unchanged'] = before == app_file('input-control.json')
            x, y = center(node(tree, 'native-increment'))
            row['replacement_input'] = fresh.tap('native-increment', x, y)
            row['replacement_advance'] = clock.request('advance', 1000000)
            row['replacement_after'] = clock.request('read')
            _, row['capture'] = observe('input-death-'+phase+'-after', 1)
            row['replacement_stopped'] = stop(command, row['replacement_before']['pid'], directory/'new-processes.txt', observe_zombie=joint)
            archive(command, directory/'new')
            save(phase+'-finished')
        save('finished')
    except BaseException as error:
        report['error'] = f'{type(error).__name__}: {error}'
        save('failed')
        raise
