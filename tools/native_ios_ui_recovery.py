"""Recover a killed tap CLI while XCTest and the current app remain alive."""
import json
from pathlib import Path
import sys

from native_clock_control import atomic_json
from native_controller_recovery import PHASES, invoke_cli, kill_controller


def recover_tap(root, ui, phase, launch, app_archive):
    directory = root/'ui-recovery'
    if phase == PHASES[0]:
        directory.mkdir()
        report = dict(progress='starting', cases=[])
    else:
        report = json.loads((directory/'report.json').read_text())
    if [row['phase'] for row in report['cases']] != list(PHASES[:PHASES.index(phase)]):
        raise RuntimeError('UI recovery phases must run once in order')
    command = [sys.executable, str(Path(__file__).with_name('native_ios_ui.py')),
               '--descriptor', str(ui.descriptor), '--directory', str(ui.directory)]
    state = json.loads(ui.path.read_text())
    row = dict(phase=phase, before=state['last_reply'], launch=launch, app_archive=app_archive)
    report['cases'].append(row)
    def save(progress):
        report['progress'] = progress
        atomic_json(directory/'report.json', report)
    def invoke(name, *args, success=True):
        return invoke_cli(directory, command, phase+'-'+name, *args, success=success)
    tap = ['tap', '--arguments', json.dumps(dict(target='native-increment'))]
    try:
        save(phase)
        kill_controller(directory, ui.directory, command, tap, row, ui.descriptor.read_bytes, prefix='ui')
        save(phase+'-killed')
        row['resume'] = invoke('resume', 'resume', success=False)
        if phase in ('before-pending', 'after-commit'):
            if row['resume']['exit_code'] == 0 or 'No pending UI command' not in row['resume']['error']:
                raise RuntimeError('Unsaved/committed UI request was unexpectedly resumed')
            row['resolution'] = (invoke('tap', *tap)['reply'] if phase == 'before-pending'
                                 else invoke('committed', 'outcome')['reply'])
        else:
            if row['resume']['exit_code']:
                raise RuntimeError('Saved UI request could not resume: '+row['resume']['error'])
            row['resolution'] = row['resume']['reply']
        endpoint = ui.descriptor.read_bytes()
        row['local_outcome'] = invoke('outcome', 'outcome')['reply']
        row['outcome_without_target_change'] = ui.descriptor.read_bytes() == endpoint
        row['state_resolved'] = json.loads(ui.path.read_text())
        row['capture'] = invoke('capture', 'capture', '--arguments', json.dumps(
            dict(name='ui-death-'+phase, clock='Time: 0', text='Pending')))['reply']
        save('finished' if phase == PHASES[-1] else phase+'-finished')
        return row['resolution']
    except BaseException as error:
        report['error'] = f'{type(error).__name__}: {error}'
        try:
            save('failed')
        except OSError as evidence_error:
            print(f'Cannot save UI recovery failure: {evidence_error}', file=sys.stderr)
        raise
