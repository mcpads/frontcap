"""Execute the existing SystemUI XCTest body on the already-owned relocated device."""
from concurrent.futures import ThreadPoolExecutor
from dataclasses import asdict
import hashlib
import json
import os
from pathlib import Path
import plistlib
import time

from native_ios_generic_ui import UIKitUIRequest
from native_ios_transport import exchange
from native_ios_ui import validate_reply
from native_ios_ui_bridge import copy_observation_artifacts
from native_host_process import execute


def exercise(output, uid, test, container, call):
    directory = output / 'system-input'
    directory.mkdir()

    def save(name, value):
        (directory / name).write_text(json.dumps(value, indent=2) + '\n')

    app = test.parent / 'Debug-iphonesimulator/SystemUI.app'
    info = plistlib.loads((app / 'Info.plist').read_bytes())
    bundle = info['CFBundleIdentifier']
    save('app.json', dict(path=str(app), bundle=bundle,
         executable_sha256=hashlib.sha256((app / info['CFBundleExecutable']).read_bytes()).hexdigest()))
    call(['/usr/bin/xcrun', 'simctl', 'install', uid, str(app)])
    descriptor = container / 'Documents/system-ui/ui-control.json'
    assert not descriptor.exists(), 'fresh XCTest endpoint required'
    command = ['/usr/bin/xcodebuild', 'test-without-building', '-xctestrun', str(test),
               '-destination', 'platform=iOS Simulator,id=' + uid, '-destination-timeout', '5',
               '-parallel-testing-enabled', 'NO', '-only-testing:SystemUITests/SystemUITests/testSystemBridge',
               '-resultBundlePath', str(directory / 'test.xcresult')]
    save('command.json', dict(command=command, timeout=120))
    endpoint = None
    number = 0
    can_finish = True

    def request(operation, arguments):
        nonlocal number, can_finish
        assert can_finish, 'uncertain request fences further input'
        number += 1
        value = UIKitUIRequest(endpoint['generation'], number, operation, arguments)
        save(f'request-{number}.json', asdict(value))
        can_finish = False
        reply = exchange(endpoint, value, timeout=20, max_response_bytes=1024 * 1024)
        validate_reply(reply, value)
        assert reply['pid'] == endpoint['pid']
        save(f'reply-{number}.json', reply)
        can_finish = True
        return reply['result']

    with ThreadPoolExecutor(max_workers=1) as executor:
        job = executor.submit(execute, command, directory / 'xctest.log', os.environ.copy(), 120, output)
        try:
            deadline = time.monotonic() + 60
            while not descriptor.exists():
                # XCTest may reinstall its runner and replace the enumeration container.
                # Rediscover through CoreSimulator, and keep the relocated-root constraint.
                current = call(['/usr/bin/xcrun', 'simctl', 'get_app_container', uid,
                                'dev.frontcap.system-ui.tests.xctrunner', 'data'],
                               timeout=5, allow_failure=True).decode().strip()
                if current:
                    candidate = Path(current)
                    relocated = Path(json.loads((output / 'relocation.json').read_text())['target'])
                    assert candidate.is_absolute() and candidate.resolve().is_relative_to(relocated.resolve())
                    container = candidate
                    descriptor = container / 'Documents/system-ui/ui-control.json'
                    if descriptor.exists():
                        break
                if job.done():
                    job.result()
                    raise RuntimeError('XCTest exited without publishing its endpoint')
                if time.monotonic() >= deadline:
                    raise TimeoutError('XCTest endpoint not ready within 60 seconds')
                time.sleep(.5)
            save('runner-container.json', dict(container=str(container), descriptor=str(descriptor),
                 filesystem_device=container.stat().st_dev))
            endpoint = json.loads(descriptor.read_text())
            assert endpoint['status'] == 'ready' and endpoint['last_id'] == 0
            assert endpoint['system_only'] and endpoint['system_alerts']
            save('endpoint.json', endpoint)
            observed = request('observe_system_alerts', {})
            copy_observation_artifacts(observed['artifacts'], descriptor.parent, directory)
            launch = call(['/usr/bin/xcrun', 'simctl', 'launch', uid, bundle]).decode().strip()
            pid = int(launch.rsplit(':', 1)[1].strip())
            save('launched-app.json', dict(pid=pid, bundle=bundle))
            background = request('set_host_app_state', dict(bundle=bundle, pid=pid, state='background'))
            assert background['change']['before_state'] == 4 and background['change']['app_state'] in (2, 3)
            foreground = request('set_host_app_state', dict(bundle=bundle, pid=pid, state='foreground'))
            assert foreground['change']['before_state'] in (2, 3) and foreground['change']['app_state'] == 4
            observed = request('observe_system_alerts', {})
            copy_observation_artifacts(observed['artifacts'], descriptor.parent, directory)
            data = Path(call(['/usr/bin/xcrun', 'simctl', 'get_app_container', uid, bundle, 'data']).decode().strip())
            assert data.stat().st_dev == container.stat().st_dev
            save('result.json', dict(state='passed', app_pid=pid, background=background['change'],
                 foreground=foreground['change'], data_container=str(data), filesystem_device=data.stat().st_dev))
        finally:
            if endpoint is not None and can_finish and not job.done():
                request('finish', {})
            job.result(timeout=180)
    log = (directory / 'xctest.log').read_text()
    assert '** TEST EXECUTE SUCCEEDED **' in log or '** TEST SUCCEEDED **' in log, 'XCTest body did not pass'
