"""Run the clock capability probe in a disposable, exclusively owned WebView host."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'tools'))
from native_emulator import OwnedEmulator
from native_host_process import write_json


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--apk', type=Path, required=True)
    parser.add_argument('--component', required=True)
    parser.add_argument('--avd', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--sdk', type=Path, default=Path.home() / 'Library/Android/sdk')
    parser.add_argument('--node', default='node')
    parser.add_argument('--reverse-port', type=int)
    parser.add_argument('--frame-gate', action='store_true')
    args = parser.parse_args()
    apk = args.apk.resolve(strict=True)
    package, separator, activity = args.component.partition('/')
    if not separator or not package or not activity or args.component.startswith('-'):
        parser.error('--component must name PACKAGE/ACTIVITY')
    if args.reverse_port is not None and not 1024 <= args.reverse_port <= 65535:
        parser.error('--reverse-port must be an unprivileged port')
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    lockdir = Path.home() / '.cache/frontcap'
    lockdir.mkdir(parents=True, exist_ok=True)
    fence = lockdir / 'native-session.json'
    emulator = OwnedEmulator(args.sdk, out, os.environ.copy(), avd=args.avd)

    def adb(*command, timeout=30, allow_missing_pid=False):
        result = subprocess.run([str(args.sdk / 'platform-tools/adb'), '-s', emulator.serial,
                                 *command], capture_output=True, text=True, timeout=timeout)
        if result.returncode and not (allow_missing_pid and result.returncode == 1
                                      and not result.stdout.strip()):
            raise RuntimeError(f'ADB {command}: {result.stderr}')
        return result.stdout.strip()

    with (lockdir / 'native-tests.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if fence.exists() and not json.loads(fence.read_text()).get('retired'):
            raise RuntimeError('Previous native owner is not retired')
        write_json(fence, dict(directory=str(out), retired=False))
        forward = None
        try:
            emulator.start()
            emulator.wait_for_boot()
            if args.reverse_port:
                port = f'tcp:{args.reverse_port}'
                adb('reverse', port, port)
            adb('install', '-r', str(apk), timeout=90)
            adb('shell', 'am', 'start', '-n', args.component)
            deadline = time.monotonic() + 60
            while True:
                pid = adb('shell', 'pidof', package, allow_missing_pid=True)
                sockets = adb('shell', 'cat', '/proc/net/unix')
                socket_name = f'webview_devtools_remote_{pid}'
                if pid.isdigit() and any(row.split()[-1] == '@'+socket_name
                                         for row in sockets.splitlines() if row.split()):
                    break
                if time.monotonic() > deadline:
                    raise TimeoutError('Owned host WebView debugger not available')
                time.sleep(.25)
            forward = adb('forward', 'tcp:0', 'localabstract:'+socket_name)
            write_json(out / 'attachment.json', dict(package=package, pid=pid,
                       socket=socket_name, forward_port=forward))
            with (out / 'probe.log').open('w') as log:
                subprocess.run([args.node, str(Path(__file__).with_name('probe.mjs')),
                                'http://127.0.0.1:'+forward, str(out / 'clock.json'),
                                *([str(Path(__file__).with_name('frame-gate.js'))]
                                  if args.frame_gate else [])],
                               stdout=log, stderr=subprocess.STDOUT, timeout=45, check=True)
        finally:
            try:
                if forward:
                    adb('forward', '--remove', 'tcp:'+forward)
            finally:
                cleanup = emulator.retire()
                write_json(out / 'retirement.json', cleanup)
                if cleanup['passed']:
                    write_json(fence, dict(directory=str(out), retired=True))


if __name__ == '__main__':
    main()
