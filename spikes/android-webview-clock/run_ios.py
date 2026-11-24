"""Measure frame/timeline control in a disposable WKWebView host.

Build the supplied test host to load http://127.0.0.1:PORT/ on launch.
"""
import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time
from urllib.parse import urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'tools'))
from native_cleanup import retire_simulator
from native_host_process import write_json
from fixture_server import create_server


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', type=Path, required=True)
    parser.add_argument('--bundle-id', required=True)
    parser.add_argument('--runtime', required=True)
    parser.add_argument('--device-type', required=True)
    parser.add_argument('--port', type=int, default=18764)
    parser.add_argument('--fixture', type=Path)
    parser.add_argument('--timeline-bundle', type=Path)
    parser.add_argument('--inject-library', type=Path)
    parser.add_argument('--inject-script', type=Path)
    parser.add_argument('--controller', type=Path)
    parser.add_argument('--page-registry', action='store_true',
                        help='Use independent per-WebView channels (requires a registry-aware controller)')
    parser.add_argument('--input-build', type=Path)
    parser.add_argument('--review-actions', action='store_true')
    parser.add_argument('--startup-steps', type=int, default=80,
                        help='Explicit 50ms startup advances for the timeline controller')
    parser.add_argument('--mute-audio', action='store_true',
                        help='Request injected HTML media mute and native speech volume zero')
    parser.add_argument('--redirect', help='Loopback URL loaded by the existing fixture host')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if args.review_actions and (not args.controller or not args.input_build):
        parser.error('--review-actions requires --controller and --input-build')
    if not 1 <= args.startup_steps <= 600 or (args.startup_steps != 80 and not args.controller):
        parser.error('--startup-steps requires a controller and must be between 1 and 600')
    app = args.app.resolve(strict=True)
    if args.redirect:
        url = urlsplit(args.redirect)
        if url.scheme != 'http' or url.hostname != '127.0.0.1' or '\r' in args.redirect or '\n' in args.redirect:
            parser.error('--redirect must be an HTTP loopback URL')
    if args.page_registry and not args.controller:
        parser.error('--page-registry requires a registry-aware controller')
    if args.controller:
        args.controller = args.controller.resolve(strict=True)
        if not args.inject_library:
            parser.error('--controller requires injected control')
    if bool(args.inject_library) != bool(args.inject_script):
        parser.error('--inject-library and --inject-script must be supplied together')
    if args.mute_audio and not args.controller:
        parser.error('--mute-audio requires injected control and a controller')
    launch_env = dict(os.environ)
    launch_env['SIMCTL_CHILD_FRONTCAP_MUTE_AUDIO'] = '1' if args.mute_audio else '0'
    if args.inject_library:
        launch_env.update(
            SIMCTL_CHILD_DYLD_INSERT_LIBRARIES=str(args.inject_library.resolve(strict=True)),
            SIMCTL_CHILD_FRONTCAP_WEBVIEW_SCRIPT=str(args.inject_script.resolve(strict=True)))
    if args.controller:
        launch_env['SIMCTL_CHILD_FRONTCAP_WEBVIEW_CONTROL'] = 'pages' if args.page_registry else '1'
    if not 1024 <= args.port <= 65535:
        parser.error('--port must be an unprivileged port')
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    write_json(out / 'audio-policy.json', dict(muted_requested=args.mute_audio,
               scope=['html_media', 'native_speech'], verified=False))
    lockdir = Path.home() / '.cache/frontcap'
    lockdir.mkdir(parents=True, exist_ok=True)
    fence = lockdir / 'native-session.json'

    def simctl(*command, timeout=90):
        result = subprocess.run(['xcrun', 'simctl', *command], capture_output=True,
                                text=True, timeout=timeout)
        if result.returncode:
            raise RuntimeError(result.stdout + result.stderr)
        return result.stdout.strip()

    with (lockdir / 'native-tests.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if fence.exists() and not json.loads(fence.read_text()).get('retired'):
            raise RuntimeError('Previous native owner is not retired')
        device = console = server = server_thread = log = None
        write_json(fence, dict(directory=str(out), retired=False))
        try:
            server = create_server(args.port, out / 'frame.json',
                                   fixture=args.fixture, timeline=args.timeline_bundle, redirect=args.redirect)
            server_thread = threading.Thread(target=server.serve_forever, daemon=True)
            server_thread.start()
            device = simctl('create', 'WebView frame probe', args.device_type, args.runtime)
            (out / 'simulator-udid').write_text(device+'\n')
            simctl('boot', device)
            simctl('bootstatus', device, '-b', timeout=180)
            simctl('install', device, str(app))
            log = (out / 'app.log').open('w')
            console = subprocess.Popen(['xcrun', 'simctl', 'launch', '--console', device,
                                        args.bundle_id], stdout=log, stderr=subprocess.STDOUT,
                                       start_new_session=True, env=launch_env)
            if args.controller:
                command = [sys.executable, str(args.controller), '--device', device,
                           '--bundle-id', args.bundle_id, '--output', str(out)]
                if args.page_registry:
                    command += ['--page-registry']
                if args.startup_steps != 80:
                    command += ['--startup-steps', str(args.startup_steps)]
                if args.input_build:
                    command += ['--input-build', str(args.input_build.resolve(strict=True))]
                if args.review_actions:
                    command += ['--review-actions']
                controller_environment = dict(os.environ)
                if args.redirect:
                    controller_environment['FRONTCAP_CONSUMER_ORIGIN'] = f'{url.scheme}://{url.netloc}'
                subprocess.run(command, check=True, timeout=900 if args.review_actions else 300,
                               env=controller_environment)
            deadline = time.monotonic() + 90
            while True:
                try:
                    report = json.loads((out / 'frame.json').read_text())
                    break
                except (FileNotFoundError, json.JSONDecodeError):
                    pass
                if time.monotonic() > deadline:
                    raise TimeoutError('No WebView frame report')
                time.sleep(.25)
            simctl('io', device, 'screenshot', str(out / 'frame.png'))
            if not report['passed']:
                raise RuntimeError('Frame fixture failed; inspect frame.json')
        finally:
            try:
                if server:
                    server.shutdown()
                    server.server_close()
                    server_thread.join(timeout=5)
            finally:
                actions = retire_simulator(device, out) if device else []
                if console:
                    try:
                        console.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        console.terminate()
                        console.wait(timeout=5)
                if log:
                    log.close()
                devices = json.loads(simctl('list', 'devices', '--json'))['devices']
                absent = device is None or all(item['udid'] != device
                                              for group in devices.values() for item in group)
                write_json(out / 'retirement.json', dict(passed=absent, actions=actions))
                if absent:
                    write_json(fence, dict(directory=str(out), retired=True))


if __name__ == '__main__':
    main()
