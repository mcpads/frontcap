"""Configured ordinary UIKit/XCTest baseline on a newly owned simulator.

Source-owned test behavior is separate from native clock and external-input qualification.
The shared host lock and retirement fence are the same as the native/Flutter runners.
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
import uuid

from flutter_capture_artifacts import png_dimensions
from native_cleanup import inspect_cleanup, require_retired_native_session, retire_simulator
from native_host_process import execute, now, WorkerInterrupted, write_json


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('config', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--runtime', default='com.apple.CoreSimulator.SimRuntime.iOS-26-1')
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    source = args.config.resolve().parent
    project = (source/config['project']).resolve(strict=True)
    if not project.is_relative_to(source):
        raise ValueError('Xcode project must remain within configured source')
    for bundle, paths in config['artifacts'].items():
        if not re.fullmatch(r'[A-Za-z0-9_.-]+', bundle):
            raise ValueError('Invalid bundle identity')
        for path in paths:
            if not re.fullmatch(r'[A-Za-z0-9_./-]+', path) or Path(path).is_absolute() or '..' in Path(path).parts:
                raise ValueError('Invalid artifact path')
    tests = config.get('tests', [])
    if not isinstance(tests, list) or any(not isinstance(test, str) or
            not re.fullmatch(r'[A-Za-z0-9_]+/[A-Za-z0-9_]+/[A-Za-z0-9_]+', test) for test in tests):
        raise ValueError('Invalid XCTest selection')
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    env = dict(os.environ, PATH='/opt/homebrew/bin:'+os.environ['PATH'])
    state = dict(state='running', pid=os.getpid(), started=now(), steps=[], config=config)
    status = output/'status.json'
    def run(name, command, timeout):
        state['active'] = name
        write_json(status, state)
        code = execute(command, output/(name+'.log'), env, timeout, source)
        state['steps'].append(dict(name=name, exit_code=code))
        if code:
            raise RuntimeError(name+' failed; retain original Run')
    def interrupted(signum, frame):
        raise WorkerInterrupted('Interrupted by signal '+str(signum))
    for signum in (signal.SIGTERM, signal.SIGINT):
        signal.signal(signum, interrupted)
    lock_dir = Path.home()/'.cache/frontcap'
    lock_dir.mkdir(parents=True, exist_ok=True)
    owned = False
    device = None
    with (lock_dir/'native-tests.lock').open('a') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            require_retired_native_session(lock_dir)
            write_json(lock_dir/'native-session.json', dict(directory=str(output), retired=False))
            owned = True
            run('create', ['xcrun', 'simctl', 'create', 'frontcap-xctest-'+output.name, 'iPhone 16', args.runtime], 60)
            device = str(uuid.UUID((output/'create.log').read_text().strip())).upper()
            (output/'simulator-id').write_text(device+'\n')
            run('boot', ['xcrun', 'simctl', 'boot', device], 60)
            run('boot-status', ['xcrun', 'simctl', 'bootstatus', device, '-b'], 180)
            run('test', ['xcodebuild', 'test', '-project', str(project), '-scheme', config['scheme'],
                '-destination', 'platform=iOS Simulator,id='+device, '-derivedDataPath', str(output/'build'),
                '-resultBundlePath', str(output/'test.xcresult'), '-parallel-testing-enabled', 'NO',
                'CODE_SIGNING_ALLOWED=NO'] + ['-only-testing:'+test for test in tests], 600)
            state['state'] = 'passed'
        except BaseException as error:
            state.update(state='failed', error=f'{type(error).__name__}: {error}', traceback=traceback.format_exc())
        finally:
            if device:
                artifacts, unavailable = {}, []
                for index, (bundle, paths) in enumerate(config['artifacts'].items()):
                    try:
                        run('container-'+str(index), ['xcrun', 'simctl', 'get_app_container', device, bundle, 'data'], 15)
                        container = Path((output/('container-'+str(index)+'.log')).read_text().strip())
                        for path in paths:
                            try:
                                data = (container/path).read_bytes()
                                if path.endswith('.png'):
                                    png_dimensions(data)
                                elif path.endswith('.json'):
                                    json.loads(data)
                                relative = Path('app-artifacts')/bundle/path
                                target = output/relative
                                target.parent.mkdir(parents=True, exist_ok=True)
                                target.write_bytes(data)
                                artifacts[str(relative)] = hashlib.sha256(data).hexdigest()
                            except Exception as error:
                                unavailable.append(dict(bundle=bundle, path=path, error=str(error)))
                    except Exception as error:
                        unavailable.append(dict(bundle=bundle, error=str(error)))
                state.update(artifacts=artifacts, unavailable_artifacts=unavailable)
                if unavailable and state['state'] == 'passed':
                    state.update(state='failed', error='Required installed evidence unavailable')
            if owned:
                try:
                    if device is None:
                        raise RuntimeError('Simulator create outcome unresolved; preserve ownership fence')
                    (output/'retirement').mkdir(exist_ok=True)
                    actions = retire_simulator(device, output/'retirement')
                    cleanup = inspect_cleanup('ios', output, env)
                    cleanup['actions'] = actions
                    write_json(output/'cleanup.json', cleanup)
                    if not cleanup['passed']:
                        raise RuntimeError('Owned simulator retirement unverified')
                    write_json(lock_dir/'native-session.json', dict(directory=str(output), retired=True))
                except BaseException as error:
                    state.update(state='failed', cleanup_error=str(error))
            state.update(active=None, finished=now())
            write_json(status, state)
    print(json.dumps(state, indent=2))
    return 0 if state['state']=='passed' else 1


if __name__ == '__main__':
    sys.exit(main())
