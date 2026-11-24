"""Run a configured ordinary Android instrumentation baseline on an owned emulator.

This checks installed app behavior; it does not qualify external input or clock control.
Build inputs must come from the caller's frozen source Run. Existing device ownership is never adopted.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import sys
import traceback

from flutter_capture_artifacts import png_dimensions
from native_cleanup import require_retired_native_session
from native_emulator import OwnedEmulator
from native_host_process import execute, now, WorkerInterrupted, write_json


def baseline_result(log, marker):
    proofs = re.findall(re.escape(marker)+r' (\{[^\n]*\})', log)
    if len(proofs) != 1 or not re.search(r'^INSTRUMENTATION_CODE: -1\s*$', log, re.MULTILINE):
        return None
    try:
        return json.loads(proofs[0])
    except ValueError:
        return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('config', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--sdk', type=Path, default=Path.home()/'Library/Android/sdk')
    parser.add_argument('--avd', default='Medium_Phone_API_35')
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    source = args.config.resolve().parent
    build = (source/config['build_script']).resolve(strict=True)
    if not build.is_relative_to(source):
        raise ValueError('Build script must remain in the configured source directory')
    for name in config['apks']+config['artifacts']+config.get('optional_artifacts', []):
        if not re.fullmatch(r'[A-Za-z0-9_./-]+', name) or Path(name).is_absolute() or '..' in Path(name).parts:
            raise ValueError('Invalid APK or app artifact path')
    if not re.fullmatch(r'[a-zA-Z0-9_.]+', config['package']) or not re.fullmatch(r'[a-zA-Z0-9_.]+/[a-zA-Z0-9_.]+', config['instrumentation']):
        raise ValueError('Invalid Android package/component')
    marker = config['success_marker']
    if not re.fullmatch(r'[A-Z_]+', marker):
        raise ValueError('Invalid baseline success marker')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, ANDROID_HOME=str(args.sdk.resolve()),
               PATH='/opt/homebrew/opt/openjdk@17/bin:/opt/homebrew/bin:'+os.environ['PATH'])
    state = dict(state='running', pid=os.getpid(), started=now(), steps=[], config=config)
    status = output/'status.json'
    def run(name, command, timeout):
        state['active'] = name
        write_json(status, state)
        code = execute(command, output/(name+'.log'), env, timeout, source)
        state['steps'].append(dict(name=name, exit_code=code))
        if code:
            raise RuntimeError(name+' failed; preserve original Run')
    def interrupted(signum, frame):
        raise WorkerInterrupted('Interrupted by signal '+str(signum))
    for signum in (signal.SIGTERM, signal.SIGINT):
        signal.signal(signum, interrupted)
    lock_dir = Path.home()/'.cache/frontcap'
    lock_dir.mkdir(parents=True, exist_ok=True)
    emulator = None
    owns_device = False
    with (lock_dir/'native-tests.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            require_retired_native_session(lock_dir)
            run('build', ['bash', str(build), str(output/'build')], 180)
            state['apks'] = {name:hashlib.sha256((output/'build'/name).read_bytes()).hexdigest() for name in config['apks']}
            write_json(lock_dir/'native-session.json', dict(directory=str(output), retired=False))
            owns_device = True
            emulator = OwnedEmulator(args.sdk, output, env, avd=args.avd)
            emulator.start()
            emulator.wait_for_boot()
            adb = [str(args.sdk/'platform-tools/adb'), '-s', emulator.serial]
            for index, name in enumerate(config['apks']):
                run('install-'+str(index), adb+['install', str(output/'build'/name)], 90)
            run('baseline', adb+['shell', 'am', 'instrument', '-w', '-r', config['instrumentation']], 180)
            log = (output/'baseline.log').read_text()
            proof = baseline_result(log, marker)
            passed = proof is not None
            if passed:
                state['baseline'] = proof
            collected = output/'app-artifacts'
            collected.mkdir()
            hashes = {}
            missing = []
            for index, name in enumerate(config['artifacts']+config.get('optional_artifacts', [])):
                try:
                    run('artifact-'+str(index), adb+['exec-out', 'run-as', config['package'], 'cat', name], 30)
                except RuntimeError:
                    missing.append(name)
                    continue
                data = (output/('artifact-'+str(index)+'.log')).read_bytes()
                try:
                    if name.endswith('.png'):
                        png_dimensions(data)
                    elif name.endswith('.json'):
                        json.loads(data)
                except (ValueError, UnicodeError) as error:
                    missing.append(name)
                    state.setdefault('artifact_errors', {})[name] = str(error)
                    continue
                target = collected/name
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(data)
                hashes[name] = hashlib.sha256(data).hexdigest()
            state.update(artifacts=hashes, unavailable_artifacts=missing)
            if not passed:
                raise RuntimeError('Instrumentation did not publish one successful baseline result')
            if any(name in config['artifacts'] for name in missing):
                raise RuntimeError('Required baseline evidence missing')
            state['state'] = 'passed'
        except BaseException as error:
            state.update(state='failed', error=f'{type(error).__name__}: {error}', traceback=traceback.format_exc())
        finally:
            if owns_device:
                try:
                    cleanup = emulator.retire() if emulator is not None else dict(passed=True, checks=[])
                    write_json(output/'cleanup.json', cleanup)
                    if not cleanup['passed']:
                        raise RuntimeError('Owned emulator retirement unverified')
                    write_json(lock_dir/'native-session.json', dict(directory=str(output), retired=True))
                except BaseException as error:
                    state.update(state='failed', cleanup_error=str(error))
            state.update(active=None, finished=now())
            write_json(status, state)
    print(json.dumps(state, indent=2))
    return 0 if state['state'] == 'passed' else 1


if __name__ == '__main__':
    sys.exit(main())
