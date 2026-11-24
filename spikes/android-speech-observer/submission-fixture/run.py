"""Check submission results from the installed Android speech framework."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import shlex
import subprocess
import sys
import time
import traceback
import uuid


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ('apk', 'library', 'avd-home', 'tools-directory', 'output'):
        p.add_argument('--'+name, type=Path, required=True)
    p.add_argument('--avd', required=True)
    a = p.parse_args()
    sys.path.insert(0, str(a.tools_directory.resolve(strict=True)))
    from native_emulator import OwnedEmulator
    from native_host_process import write_json
    out = a.output.resolve(); out.mkdir()
    sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
    inputs = {str(path.resolve()): sha(path) for path in (a.apk, a.library, Path(__file__))}
    write_json(out/'inputs.json', inputs)
    source = a.avd_home.resolve(strict=True)
    original = {str(path.relative_to(source)): sha(path) for path in source.rglob('*') if path.is_file()}
    write_json(out/'avd-inputs.json', original)
    shutil.copytree(source, out/'avd', ignore=shutil.ignore_patterns('snapshot.lock.lock'))
    (out/'avd'/f'{a.avd}.ini').write_text('avd.ini.encoding=UTF-8\npath='+str(out/'avd'/f'{a.avd}.avd')+'\ntarget=android-35\n')
    sdk = Path.home()/'Library/Android/sdk'
    emulator = OwnedEmulator(sdk, out, dict(os.environ, ANDROID_AVD_HOME=str(out/'avd')), avd=a.avd)
    state = dict(state='running', pid=os.getpid())
    write_json(out/'state.json', state)
    package = 'dev.frontcap.speechsubmission'
    commands = []

    def adb(*args, required=True):
        commands.append(list(args)); write_json(out/'commands.json', commands)
        result = subprocess.run([str(sdk/'platform-tools/adb'), '-s', emulator.serial, *args], capture_output=True, timeout=60)
        if len(result.stdout) > 262144 or len(result.stderr) > 16384:
            raise RuntimeError('diagnostic command output exceeds bound')
        if required and result.returncode:
            raise RuntimeError(result.stderr.decode(errors='replace'))
        return result.stdout

    def records(name):
        raw = adb('exec-out', 'run-as', package, 'cat', 'files/'+name, required=False)
        try:
            rows = [json.loads(line) for line in raw.splitlines()]
        except (ValueError, UnicodeError):
            return []
        (out/name).write_bytes(raw)
        return rows

    lockdir = Path.home()/'.cache/frontcap'; fence = lockdir/'native-session.json'
    with (lockdir/'native-tests.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        assert not fence.exists() or json.loads(fence.read_text()).get('retired')
        write_json(fence, dict(directory=str(out), retired=False))
        try:
            emulator.start(); emulator.wait_for_boot()
            adb('install', '-r', str(a.apk.resolve()))
            adb('shell', 'am', 'start', '-n', package+'/.SubmissionActivity')
            deadline = time.monotonic()+45
            while not records('submission-oracle.jsonl'):
                if time.monotonic() > deadline: raise TimeoutError('TTS initialization')
                time.sleep(.5)
            assert records('submission-oracle.jsonl')[0]['returned'] == 0
            pid = int(adb('shell', 'pidof', package).strip())
            stat = adb('shell', 'cat', f'/proc/{pid}/stat').decode()
            ticks = int(stat.rsplit(')', 1)[1].split()[19]); attachment = str(uuid.uuid4())
            state.update(app_pid=pid, start_ticks=ticks, attachment=attachment)
            adb('push', str(a.library.resolve()), '/data/local/tmp/submission-observer.so')
            adb('shell', 'run-as', package, 'cp', '/data/local/tmp/submission-observer.so', 'files/observer.so')
            options = f'/data/data/{package}/files|{pid}|{ticks}|{attachment}'
            adb('shell', 'am', 'attach-agent', package, shlex.quote(f'/data/data/{package}/files/observer.so={options}'))
            rows = records('speech-observer.jsonl')
            ready = next(row for row in rows if row['event'] == 'ready')
            state['submission_observed'] = ready.get('submission_observed', False)
            write_json(out/'state.json', state)
            adb('shell', 'am', 'start', '-n', package+'/.SubmissionActivity', '--es', 'action', 'submit')
            deadline = time.monotonic()+20
            while not any(row.get('operation') == 'after-shutdown' for row in records('submission-oracle.jsonl')):
                if time.monotonic() > deadline: raise TimeoutError('framework returns')
                time.sleep(.2)
            rows = records('speech-observer.jsonl')
            oracle = [row for row in records('submission-oracle.jsonl') if row['operation'] != 'initialization']
            assert [row['operation'] for row in oracle] == ['ordinary', 'oversized', 'after-shutdown']
            observed = [row for row in rows if row['event'] == 'submission']
            assert state['submission_observed'], 'ART method-exit capability unavailable'
            assert [row['returned'] for row in observed] == [row['returned'] for row in oracle]
            assert [row['returned'] for row in oracle] == [0, -1, -1], oracle
            assert all(not row['threw'] and row['observer_error'] == 0 for row in observed)
            state.update(state='passed',returns=[row['returned'] for row in observed])
        except BaseException:
            state.update(state='failed', error=traceback.format_exc())
        finally:
            cleanup = emulator.retire(); state['retirement'] = cleanup
            write_json(out/'retirement.json', cleanup)
            write_json(fence, dict(directory=str(out), retired=cleanup['passed']))
            if not cleanup['passed']: state['state'] = 'failed'
            state['inputs_unchanged'] = all(sha(Path(name)) == digest for name,digest in inputs.items())
            state['source_avd_unchanged'] = all(sha(source/name) == digest for name,digest in original.items())
            if not state['inputs_unchanged'] or not state['source_avd_unchanged']: state['state'] = 'failed'
            if cleanup['passed']: shutil.rmtree(out/'avd')
            write_json(out/'state.json', state)
    return 0 if state['state'] == 'passed' else 1


if __name__ == '__main__':
    raise SystemExit(main())
