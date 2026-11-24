"""Opt-in installed ADB startup on a full, fresh owned volume; no device acquisition.

Records observed readiness separately from actual SDK capacity diagnostics. Evidence and
bounded output stay outside the 256 MiB image. Never resumes or adopts a prior volume.
"""
import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import traceback

from probe import listeners, listener_owners, query, save
from native_owned_volume import OwnedVolume
from native_output_log import OutputLog


def probe(adb, output, port):
    assert sys.platform == 'darwin' and 1024 <= port <= 65535 and port != 5037
    adb = adb.resolve(strict=True)
    output.mkdir(parents=True, exist_ok=False)
    output = output.resolve()
    assert not listeners(port)
    shared = listener_owners(5037)
    state = dict(state='running', pid=os.getpid(), started=time.time())
    save(output / 'state.json', state)
    owner = child = collector = None
    number = 0

    def command(args):
        nonlocal number
        number += 1
        prefix = output / f'volume-command-{number}'
        save(prefix.with_suffix('.json'), dict(command=args))
        result = subprocess.run(args, capture_output=True, timeout=90)
        prefix.with_suffix('.stdout').write_bytes(result.stdout)
        prefix.with_suffix('.stderr').write_bytes(result.stderr)
        assert result.returncode == 0, f'volume command {number} failed'
        return result.stdout

    try:
        owner = OwnedVolume.create(output / 'volume', 256, command)
        owner.attach()
        owner.reserve_retirement(8)
        scratch = owner.mount
        assert scratch.stat().st_dev != output.stat().st_dev
        home, temporary = scratch / 'home', scratch / 'tmp'
        home.mkdir(); temporary.mkdir(); (home / '.android').mkdir()
        # Finish with single allocation blocks so a large failed write does not leave
        # enough spare capacity for ADB's small key files. Both loops are finite.
        written = 0
        failures = []
        with (scratch / 'fill.bin').open('xb', buffering=0) as stream:
            for size, count in [(1024 * 1024, 256), (4096, 1024)]:
                block = os.urandom(size)
                try:
                    for _ in range(count):
                        offset = 0
                        while offset < size:
                            n = stream.write(block[offset:])
                            assert n, 'capacity writer made no progress'
                            written += n; offset += n
                        os.fsync(stream.fileno())
                except OSError as error:
                    assert error.errno in (errno.ENOSPC, errno.EDQUOT)
                    failures.append(dict(block_bytes=size, errno=error.errno))
                else:
                    raise RuntimeError('capacity writer reached its finite bound without rejection')
        save(output / 'capacity.json', dict(written=written, failures=failures,
             available=os.statvfs(scratch).f_bavail * os.statvfs(scratch).f_frsize))
        policy = ('(version 1)\n(allow default)\n(deny file-write*)\n'
                  '(allow file-write* (subpath ' + json.dumps(str(scratch)) + '))\n'
                  '(allow file-write* (literal "/dev/null"))\n')
        overrides = dict(HOME=str(home), ANDROID_USER_HOME=str(home), TMPDIR=str(temporary),
                         TMP=str(temporary), TEMP=str(temporary), ADB_USB='0', ADB_MDNS='0',
                         ADB_LOCAL_TRANSPORT_MAX_PORT='0')
        env = {k: v for k, v in os.environ.items() if not k.startswith(('ADB_', 'ANDROID_'))}
        env.update(overrides)
        args = ['/usr/bin/sandbox-exec', '-p', policy, str(adb), '-L', f'tcp:{port}', 'server', 'nodaemon']
        save(output / 'inputs.json', dict(command=args, environment=overrides,
             adb_sha256=hashlib.sha256(adb.read_bytes()).hexdigest(),
             source_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), shared=shared))
        with (output / 'server.log').open('xb') as log:
            try:
                child = subprocess.Popen(args, env=env, stdin=subprocess.DEVNULL,
                                         stdout=subprocess.PIPE, stderr=subprocess.STDOUT, close_fds=True)
                save(output / 'process.json', dict(pid=child.pid, command=args))
                collector = OutputLog(child.stdout, log, limit=1024 * 1024)
                end = time.monotonic() + 10
                ready = False
                while child.poll() is None and time.monotonic() < end:
                    collector.check()
                    if listeners(port) == [child.pid]:
                        assert query(port, 'host:devices-l') == ''
                        ready = True
                        break
                    time.sleep(.05)
                save(output / 'observation.json', dict(listener_ready=ready,
                     exit_before_retirement=child.poll(), home_files=[dict(path=str(p.relative_to(home)),
                     bytes=p.stat().st_size) for p in home.rglob('*') if p.is_file()]))
            finally:
                if child is not None:
                    if child.poll() is None: child.terminate()
                    try: child.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        child.kill(); child.wait(timeout=5)
                    save(output / 'closure.json', dict(pid=child.pid, reaped=True, returncode=child.returncode))
                if collector is not None: collector.finish(output / 'output-report.json')
        keygen = subprocess.run(['/usr/bin/sandbox-exec', '-p', policy, str(adb),
                                 'keygen', str(home / '.android/adbkey')], env=env,
                                capture_output=True, text=True, timeout=10)
        save(output / 'key-generation.json', dict(returncode=keygen.returncode,
             stdout=keygen.stdout, stderr=keygen.stderr))
        assert keygen.returncode != 0 and 'No space left on device' in keygen.stderr
        assert not listeners(port)
        assert listener_owners(5037) == shared
        collector.check()
        assert collector.report['eof']
        state.update(state='observed', finished=time.time())
    except BaseException:
        state.update(state='failed', error=traceback.format_exc(), finished=time.time())
        raise
    finally:
        if owner is not None and (child is None or child.poll() is not None):
            if owner.attached is not None:
                if owner.reserve and owner.reserve['state'] == 'reserved':
                    owner.release_retirement(writers_retired=True)
                owner.detach(writers_retired=True)
            owner.remove()
            save(output / 'volume-closure.json', dict(detached=True, image_removed=not owner.image.exists()))
        save(output / 'state.json', state)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--adb', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--port', required=True, type=int)
    args = parser.parse_args()
    probe(args.adb, args.output, args.port)
