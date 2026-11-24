"""Kill the input CLI before transport launch; distinguish resumable preparation from dispatch intent."""
import json
from pathlib import Path
import sys
import time

from native_android_control import ClockClient
from native_android_input import InputClient, read_receipt
from native_android_lifecycle import PACKAGE, archive, stop
from native_android_tap import host_processes
from native_clock_control import atomic_json
from native_controller_recovery import invoke_cli, kill_controller

PHASES = ('before-pending', 'after-pending', 'after-registration', 'after-dispatch')


def exercise(root, command, launch, observe, node, center, adb):
    report = dict(progress='starting', cases=[])
    def save(progress):
        report['progress'] = progress
        atomic_json(root/'input-preparation.json', report)
    def receipt():
        return command('shell', 'run-as', PACKAGE, 'cat', 'files/input-control.json')
    def inventory(process):
        rows = host_processes()
        return dict(observed_monotonic_ns=time.monotonic_ns(),
                    parents=[p for p in rows if p['pid'] == process.pid],
                    children=[p for p in rows if p['ppid'] == process.pid])
    def cli(directory):
        return [sys.executable, str(Path(__file__).with_name('native_android_input.py')),
                '--adb', adb[0], '--serial', adb[2], '--directory', str(directory), '--record-tap-process']
    try:
        for phase in PHASES:
            directory = root/'input-preparation'/phase
            directory.mkdir(parents=True)
            row = dict(phase=phase)
            report['cases'].append(row)
            save(phase+'-launching')
            row['launch'] = launch().decode()
            tree, row['initial_screen'] = observe('input-preparation-'+phase+'-initial', 0)
            row['before'] = read_receipt(command, PACKAGE)
            client = InputClient(command, directory/'controller')
            x, y = center(node(tree, 'native-increment'))
            arguments = ['tap', '--x', str(x), '--y', str(y)]
            command_line = cli(client.directory)
            kill_controller(directory, client.directory, command_line, arguments, row, receipt,
                            prefix='input', before_kill=inventory)
            row['request'] = row['checkpoint']['request']
            row['transport_before_recovery'] = (client.directory/'input-transport.json').exists()
            if (row['before_kill']['children'] or row['transport_before_recovery']
                    or len(row['before_kill']['parents']) != 1 or not row['lock_held']):
                raise RuntimeError('Input controller did not stop before transport launch')
            archive(command, directory/'at-death')
            _, row['death_screen'] = observe('input-preparation-'+phase+'-death', 0)
            save(phase+'-killed')
            def invoke(name, *args, success=True):
                return invoke_cli(directory, command_line, name, *args, success=success)
            row['recover'] = invoke('recover', 'recover', success=False)
            row['state_after_recover'] = json.loads(client.path.read_text())
            row['receipt_after_recover'] = json.loads(receipt())
            row['resume'] = invoke('resume', 'resume', success=False)
            row['state_after_resume'] = json.loads(client.path.read_text())
            row['receipt_after_resume'] = json.loads(receipt())
            if phase == 'after-dispatch':
                if (row['resume']['exit_code'] == 0 or row['recover']['exit_code'] == 0
                        or row['state_after_recover'] != row['state_at_death']
                        or row['state_after_resume'] != row['state_at_death']):
                    raise RuntimeError('Dispatch intent was repeated or falsely resolved')
                row['old_transport_after_recovery'] = (client.directory/'input-transport.json').exists()
                row['old_stopped'] = stop(command, row['before']['pid'], directory/'old-processes.txt')
                archive(command, directory/'old')
                row['replacement_launch'] = launch().decode()
                tree, row['replacement_screen'] = observe('input-preparation-replacement', 0)
                row['replacement_before'] = json.loads(receipt())
                row['old_resume_after_replacement'] = invoke('old-resume', 'resume', success=False)
                row['replacement_unchanged'] = json.loads(receipt()) == row['replacement_before']
                fresh = InputClient(command, directory/'replacement-controller')
                completed_client = fresh.directory
                x, y = center(node(tree, 'native-increment'))
                row['resolution'] = invoke_cli(directory, cli(completed_client), 'replacement-tap', 'tap', '--x', str(x), '--y', str(y))['reply']
            else:
                completed_client = client.directory
                row['resolution'] = (invoke('fresh-tap', *arguments)['reply'] if phase == 'before-pending' else row['resume']['reply'])
                if row['resolution'] is None:
                    raise RuntimeError('Prepared input could not resume')
            endpoint = receipt()
            row['outcome'] = invoke_cli(directory, cli(completed_client), 'outcome', 'outcome')['reply']
            row['outcome_unchanged'] = receipt() == endpoint
            row['completed_client'] = str(completed_client.relative_to(root))
            row['completed_transport'] = json.loads((completed_client/'input-transport.json').read_text())
            row['client_final'] = json.loads(client.path.read_text())
            row['completed_state'] = json.loads((completed_client/'input-client.json').read_text())
            clock = ClockClient(command, directory/'clock')
            row['clock_before'] = clock.request('read')
            row['advance'] = clock.request('advance', 1000000)
            row['clock_after'] = clock.request('read')
            _, row['capture'] = observe('input-preparation-'+phase+'-after', 1)
            row['stopped'] = stop(command, row['resolution']['pid'], directory/'processes.txt')
            archive(command, directory/'final')
            save(phase+'-finished')
        save('finished')
    except BaseException as error:
        report['error'] = f'{type(error).__name__}: {error}'
        save('failed')
        raise
