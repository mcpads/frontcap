"""Kill and recreate the actual clock CLI while retaining a live application generation."""
import fcntl
import json
import os
import subprocess
import time

from native_clock_control import atomic_json


PHASES = ('before-pending', 'after-pending', 'after-reply', 'after-commit')


def lock_available(directory, prefix='clock'):
    with (directory/(prefix+'-client.lock')).open('a') as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False
        return True


def controller_environment():
    env = dict(os.environ)
    env.pop('FC_CONTROLLER_STOP_AT', None)
    return env


def invoke_cli(directory, command, name, *args, success=True):
    result = subprocess.run([*command, *args], env=controller_environment(), capture_output=True, text=True, timeout=15)
    (directory/(name+'.stdout')).write_text(result.stdout)
    (directory/(name+'.stderr')).write_text(result.stderr)
    if success and result.returncode:
        raise RuntimeError(f'Controller CLI failed ({name}): '+result.stderr)
    return dict(exit_code=result.returncode, reply=json.loads(result.stdout) if not result.returncode else None,
                error=result.stderr if result.returncode else None)


def kill_controller(directory, client, command, arguments, row, read_target, *, prefix='clock', before_kill=None):
    phase = row['phase']
    marker_path = client/'controller-checkpoint.json'
    marker_path.unlink(missing_ok=True)
    with (directory/(phase+'.stdout')).open('wb') as stdout, (directory/(phase+'.stderr')).open('wb') as stderr:
        process = subprocess.Popen([*command, *arguments],
            env={**controller_environment(), 'FC_CONTROLLER_STOP_AT':phase}, stdout=stdout, stderr=stderr)
        row['controller_pid'] = process.pid
        try:
            deadline = time.monotonic()+5
            while True:
                if marker_path.exists():
                    marker = json.loads(marker_path.read_text())
                    if marker.get('pid') == process.pid and marker.get('phase') == phase and marker.get('status') == 'waiting':
                        break
                if process.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError(f'Controller checkpoint unavailable: {phase}, child={process.poll()}')
                time.sleep(.01)
            row['checkpoint'] = marker
            row['lock_held'] = not lock_available(client, prefix)
            if before_kill is not None:
                row['before_kill'] = before_kill(process)
            process.kill()
            row['exit_code'] = process.wait(timeout=5)
            row['checkpoint_after'] = json.loads(marker_path.read_text())
            row['lock_released'] = lock_available(client, prefix)
            row['state_at_death'] = json.loads((client/(prefix+'-client.json')).read_text())
            row['target_at_death'] = json.loads(read_target())
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)


def exercise(directory, command, read_target):
    directory.mkdir(parents=True)
    client = directory/'client'
    command = [*command, '--directory', str(client)]
    report = dict(progress='starting', cases=[])
    def save(progress):
        report['progress'] = progress
        atomic_json(directory/'report.json', report)
    def invoke(name, *args, success=True):
        return invoke_cli(directory, command, name, *args, success=success)
    try:
        report['initial'] = before = invoke('initial', 'read')['reply']
        for phase in PHASES:
            row = dict(phase=phase, before=before)
            report['cases'].append(row)
            save(phase)
            kill_controller(directory, client, command, ['advance', '--delta-ns', '1000000'], row, read_target)
            save(phase+'-killed')
            row['resume'] = invoke(phase+'-resume', 'resume', success=False)
            if phase in ('before-pending', 'after-commit'):
                if row['resume']['exit_code'] == 0 or 'No pending clock command' not in row['resume']['error']:
                    raise RuntimeError('Resolved/unsaved request was unexpectedly resumed')
                row['resolution'] = (invoke(phase+'-advance', 'advance', '--delta-ns', '1000000')['reply']
                                     if phase == 'before-pending' else invoke(phase+'-committed', 'outcome')['reply'])
            else:
                if row['resume']['exit_code']:
                    raise RuntimeError('Persisted request could not resume: '+row['resume']['error'])
                row['resolution'] = row['resume']['reply']
            endpoint = read_target()
            row['local_outcome'] = invoke(phase+'-outcome', 'outcome')['reply']
            row['outcome_without_target_change'] = read_target() == endpoint
            row['state_resolved'] = json.loads((client/'clock-client.json').read_text())
            row['after'] = before = invoke(phase+'-read', 'read')['reply']
            save(phase+'-finished')
        save('finished')
        return report
    except BaseException as error:
        report['error'] = f'{type(error).__name__}: {error}'
        try:
            save('failed')
        except OSError as evidence_error:
            print(f'Cannot save controller recovery failure: {evidence_error}')
        raise
