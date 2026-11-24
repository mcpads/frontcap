"""Opt-in installed AVSpeechSynthesizer failure observation, separate from consumer flows."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import sys
import time
import traceback
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--runtime', default='com.apple.CoreSimulator.SimRuntime.iOS-26-1')
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    sys.path.insert(0, str(root/'tools'))
    from native_cleanup import retire_simulator
    out = args.output.resolve(); out.mkdir()
    sha = lambda path: hashlib.sha256(path.read_bytes()).hexdigest()
    inputs = [Path(__file__).resolve(), root/'spikes/ios-speech-host/failure-fixture.m',
              *[root/'spikes/webview-timeline'/name for name in ('speech-observer.m', 'speech-delegate.m', 'speech-flow.h')]]
    hashes = {str(path): sha(path) for path in inputs}
    (out/'inputs.json').write_text(json.dumps(hashes, indent=2))
    state = dict(state='running', pid=os.getpid()); device = None; creation_attempted = False
    commands = []

    def save(): (out/'state.json').write_text(json.dumps(state, indent=2))
    def run(argv, *, env=None, timeout=120):
        number = len(commands); commands.append(argv)
        (out/'commands.json').write_text(json.dumps(commands, indent=2))
        stdout, stderr = out/f'command-{number}.stdout', out/f'command-{number}.stderr'
        with stdout.open('xb') as output, stderr.open('xb') as error:
            result = subprocess.run(argv, stdout=output, stderr=error, env=env, timeout=timeout)
        if result.returncode: raise RuntimeError(f'Command {number} failed ({result.returncode}); see {stderr}')
        return stdout.read_text()

    save()
    try:
        sdk = run(['xcrun', '--sdk', 'iphonesimulator', '--show-sdk-path']).strip()
        app = out/'SpeechFailure.app'; app.mkdir()
        plist = dict(CFBundleIdentifier='dev.frontcap.speechfailure', CFBundleExecutable='SpeechFailure',
                     CFBundleName='SpeechFailure', CFBundlePackageType='APPL', CFBundleVersion='1',
                     CFBundleShortVersionString='1.0', MinimumOSVersion='17.0', UIDeviceFamily=[1, 2],
                     UILaunchScreen={})
        (app/'Info.plist').write_bytes(plistlib.dumps(plist))
        common = ['xcrun', 'clang', '-fobjc-arc', '-target', 'arm64-apple-ios17.0-simulator', '-isysroot', sdk,
                  '-framework', 'Foundation', '-framework', 'AVFoundation']
        run(common+['-dynamiclib', str(inputs[2]), str(inputs[3]), '-o', str(app/'SpeechObserver.dylib')])
        run(common+['-framework', 'UIKit', str(inputs[1]), '-o', str(app/'SpeechFailure')])
        run(['codesign', '--force', '--sign', '-', str(app/'SpeechObserver.dylib')])
        run(['codesign', '--force', '--sign', '-', str(app)])
        lockdir = Path.home()/'.cache/frontcap'; fence = lockdir/'native-session.json'
        with (lockdir/'native-tests.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            assert not fence.exists() or json.loads(fence.read_text()).get('retired')
            fence.write_text(json.dumps(dict(directory=str(out), retired=False)))
            try:
                creation_attempted = True
                device = str(uuid.UUID(run(['xcrun', 'simctl', 'create', 'FrontcapSpeechFailure-'+uuid.uuid4().hex[:8],
                    'com.apple.CoreSimulator.SimDeviceType.iPhone-16', args.runtime]).strip())).upper()
                state['device'] = device; save()
                run(['xcrun', 'simctl', 'boot', device])
                run(['xcrun', 'simctl', 'bootstatus', device, '-b'])
                run(['xcrun', 'simctl', 'install', device, str(app)])
                bundle = plist['CFBundleIdentifier']
                installed = run(['xcrun', 'simctl', 'get_app_container', device, bundle, 'app']).strip()
                env = dict(os.environ, SIMCTL_CHILD_DYLD_INSERT_LIBRARIES=str(Path(installed)/'SpeechObserver.dylib'),
                           SIMCTL_CHILD_FRONTCAP_WEBVIEW_CONTROL='1', SIMCTL_CHILD_FRONTCAP_MUTE_AUDIO='1')
                launched = run(['xcrun', 'simctl', 'launch', device, bundle], env=env)
                state['app_pid'] = int(launched.strip().rsplit(':', 1)[1]); save()
                container = Path(run(['xcrun', 'simctl', 'get_app_container', device, bundle, 'data']).strip())
                deadline = time.monotonic()+30
                while not (container/'Documents/speech-failure-done.json').exists():
                    if time.monotonic() > deadline: raise TimeoutError('native failure fixture completion')
                    time.sleep(.2)
                for path in (container/'Documents').iterdir():
                    if path.is_file() and (path.name.startswith('speech-failure-') or path.name=='native-speech-api.jsonl'):
                        shutil.copy2(path, out/path.name)
                oracle = json.loads((out/'speech-failure-oracle.json').read_text())
                observed = json.loads((out/'speech-failure-observation.json').read_text())
                assert [row['outcome'] for row in oracle] == ['returned', 'threw'], oracle
                assert observed['apiObserved'] and observed['delegateObserved'] and observed['faults']==0
                assert len(observed['utterances'])==1
                utterance = observed['utterances'][0]
                assert utterance['requests']==2 and utterance['failures']==1
                assert sum(row['kind']=='failed' for row in observed['events'])==1
                assert all(sha(Path(name))==digest for name,digest in hashes.items())
                state.update(state='passed', exception=oracle[1]['exception'], observer_process=observed['process'])
            finally:
                cleanup = retire_simulator(device, out) if device else []
                absent = False
                if device:
                    devices = json.loads(run(['xcrun', 'simctl', 'list', 'devices', '--json']))['devices']
                    absent = all(row['udid']!=device for rows in devices.values() for row in rows)
                retired = absent if creation_attempted else True
                state.update(retirement=cleanup, retired=retired)
                fence.write_text(json.dumps(dict(directory=str(out), retired=retired)))
                if not retired: state['state']='failed'
    except BaseException:
        state.update(state='failed', error=traceback.format_exc())
    finally: save()
    return 0 if state['state']=='passed' else 1


if __name__ == '__main__': raise SystemExit(main())
