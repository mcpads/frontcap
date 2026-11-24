"""Kill input CLI children around existing Android lifecycle taps."""
import json
from dataclasses import asdict, replace
from pathlib import Path
import sys
import uuid

from native_android_input import InputRequest, read_receipt, register_request
from native_clock_control import atomic_json
from native_controller_recovery import PHASES, invoke_cli, kill_controller


def registration_controls(root, client, reply):
    request = InputRequest(**reply['request'])
    rows = []
    for name, candidate, error in (
            ('replay', request, None),
            ('conflict', replace(request, x=request.x+1), 'request_conflict'),
            ('owner', replace(request, controller_id=str(uuid.uuid4())), 'controller_mismatch'),
            ('generation', replace(request, generation=str(uuid.uuid4())), 'generation_mismatch'),
            ('gap', replace(request, request_id=request.request_id+2), 'sequence_gap')):
        row = dict(name=name, request=asdict(candidate))
        rows.append(row)
        try:
            row['broadcast'] = register_request(client.command, client.package, candidate)
            row['receipt'] = read_receipt(client.command, client.package)
            expected = (f'Broadcast completed: result=1, data="{error}"' if error else
                        f'Broadcast completed: result=-1, data="{request.generation}:{request.request_id}"')
            if expected not in row['broadcast'].splitlines() or row['receipt'] != reply:
                raise RuntimeError('Input registration changed its accepted outcome: '+name)
        finally:
            atomic_json(root/'input-registration.json', rows)


def recover_tap(root, client, adb, phase, target, x, y):
    directory = root/'input-recovery'
    if phase == PHASES[0]:
        directory.mkdir()
        report = dict(progress='starting', cases=[])
    else:
        report = json.loads((directory/'report.json').read_text())
    if [r['phase'] for r in report['cases']] != list(PHASES[:PHASES.index(phase)]):
        raise RuntimeError('Input recovery phases must run once in order')
    command = [sys.executable, str(Path(__file__).with_name('native_android_input.py')),
               '--adb', adb[0], '--serial', adb[2], '--directory', str(client.directory)]
    def read_target():
        return client.command('shell', 'run-as', client.package, 'cat', 'files/input-control.json')
    row = dict(phase=phase, before=json.loads(read_target()), client=str(client.directory.relative_to(root)))
    report['cases'].append(row)
    def save(progress):
        report['progress'] = progress
        atomic_json(directory/'report.json', report)
    def invoke(name, *args, success=True):
        return invoke_cli(directory, command, phase+'-'+name, *args, success=success)
    tap = ['tap', '--target', target, '--x', str(x), '--y', str(y)]
    try:
        save(phase)
        kill_controller(directory, client.directory, command, tap, row, read_target, prefix='input')
        save(phase+'-killed')
        row['resume'] = invoke('resume', 'resume', success=False)
        if phase in ('before-pending', 'after-commit'):
            if row['resume']['exit_code'] == 0 or 'No pending input command' not in row['resume']['error']:
                raise RuntimeError('Unsaved/committed input was unexpectedly resumed')
            row['resolution'] = (invoke('tap', *tap)['reply'] if phase == 'before-pending'
                                 else invoke('committed', 'outcome')['reply'])
        else:
            if row['resume']['exit_code']:
                raise RuntimeError('Saved input could not resume: '+row['resume']['error'])
            row['resolution'] = row['resume']['reply']
        endpoint = read_target()
        row['local_outcome'] = invoke('outcome', 'outcome')['reply']
        row['outcome_without_target_change'] = read_target() == endpoint
        row['state_resolved'] = json.loads(client.path.read_text())
        save('finished' if phase == PHASES[-1] else phase+'-finished')
        return row['resolution']
    except BaseException as error:
        report['error'] = f'{type(error).__name__}: {error}'
        try:
            save('failed')
        except OSError as evidence_error:
            print(f'Cannot save input recovery failure: {evidence_error}', file=sys.stderr)
        raise
