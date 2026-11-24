"""Opt-in macOS ADB server isolation prerequisite; never acquires a device.

Start one foreground server with USB, mDNS and emulator scanning disabled. Query
its smart socket directly (no client auto-start), then retire that exact child.
This is not product integration, a volume quota, or installed device qualification.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import time
import traceback

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'tools'))
from native_output_log import OutputLog


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def listeners(port):
    result = subprocess.run(['/usr/sbin/lsof', '-nP', f'-iTCP:{port}',
                             '-sTCP:LISTEN', '-Fp'], capture_output=True, text=True)
    assert result.returncode in (0, 1), result.stderr
    return sorted({int(line[1:]) for line in result.stdout.splitlines() if line.startswith('p')})


def listener_owners(port):
    return [dict(pid=pid, started=subprocess.check_output(
        ['ps', '-p', str(pid), '-o', 'lstart='], text=True).strip()) for pid in listeners(port)]


def receive(stream, count):
    data = b''
    while len(data) < count:
        part = stream.recv(count - len(data))
        if not part:
            raise RuntimeError('ADB response closed before completion')
        data += part
    return data


def query(port, service):
    data = service.encode('ascii')
    with socket.create_connection(('127.0.0.1', port), timeout=3) as stream:
        stream.sendall(f'{len(data):04x}'.encode('ascii') + data)
        assert receive(stream, 4) == b'OKAY'
        size = int(receive(stream, 4), 16)
        assert size <= 4096, 'ADB response exceeds probe bound'
        return receive(stream, size).decode('utf-8')


def probe(adb, output, port):
    assert sys.platform == 'darwin'
    assert 1024 <= port <= 65535 and port != 5037
    adb = adb.resolve(strict=True)
    output.mkdir(parents=True, exist_ok=False)
    output = output.resolve()
    scratch = output / 'scratch'
    scratch.mkdir()
    home, temporary = scratch / 'home', scratch / 'tmp'
    home.mkdir()
    temporary.mkdir()
    before = listener_owners(5037)
    assert not listeners(port), 'Dedicated server port already occupied'
    state = dict(state='running', pid=os.getpid(), started=time.time())
    save(output / 'state.json', state)
    policy = ('(version 1)\n(allow default)\n(deny file-write*)\n'
              '(allow file-write* (subpath ' + json.dumps(str(scratch)) + '))\n'
              '(allow file-write* (literal "/dev/null"))\n')
    env = {key: value for key, value in os.environ.items()
           if not key.startswith(('ADB_', 'ANDROID_'))}
    overrides = dict(HOME=str(home), ANDROID_USER_HOME=str(home), TMPDIR=str(temporary),
                     TMP=str(temporary), TEMP=str(temporary), ADB_USB='0', ADB_MDNS='0',
                     ADB_LOCAL_TRANSPORT_MAX_PORT='0')
    env.update(overrides)
    # An explicit numeric host is treated as remote by ADB: failed connection
    # must not auto-start a replacement daemon. Verify the installed behavior below.
    client_env = dict(env, ADB_SERVER_SOCKET=f'tcp:127.0.0.1:{port}')
    command = ['/usr/bin/sandbox-exec', '-p', policy, str(adb), '-L',
               f'tcp:{port}', 'server', 'nodaemon']
    save(output / 'inputs.json', dict(command=command, environment_overrides=overrides,
         adb_sha256=hashlib.sha256(adb.read_bytes()).hexdigest(),
         source_sha256=hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
         shared_listener_owners=before))
    child = None
    collector = None
    with (output / 'server.log').open('xb') as log:
        try:
            child = subprocess.Popen(command, env=env, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                     close_fds=True)
            save(output / 'process.json', dict(pid=child.pid, command=command))
            collector = OutputLog(child.stdout, log, limit=1024 * 1024)
            deadline = time.monotonic() + 10
            while listeners(port) != [child.pid]:
                collector.check()
                assert child.poll() is None, 'Foreground server exited before readiness'
                assert time.monotonic() < deadline, 'Dedicated listener not ready'
                time.sleep(.05)
            version = query(port, 'host:version')
            devices = query(port, 'host:devices-l')
            assert int(version, 16) > 0
            assert devices == '', 'Isolated server unexpectedly acquired a transport'
            sockets = subprocess.check_output(['/usr/sbin/lsof', '-nP', '-a', '-p',
                str(child.pid), '-iTCP', '-sTCP:LISTEN', '-Fn'], text=True)
            addresses = [line[1:] for line in sockets.splitlines() if line.startswith('n')]
            assert addresses == [f'127.0.0.1:{port}'], addresses
            client = subprocess.run([str(adb), 'devices', '-l'], env=client_env,
                                    capture_output=True, text=True, timeout=5)
            save(output / 'client-live.json', dict(returncode=client.returncode,
                 stdout=client.stdout, stderr=client.stderr))
            assert client.returncode == 0
            assert client.stdout.strip() == 'List of devices attached'
            save(output / 'queries.json', dict(version=version, devices=devices,
                                             listener_pid=child.pid, addresses=addresses))
        except BaseException:
            state.update(state='failed', error=traceback.format_exc())
            raise
        finally:
            if child is not None:
                if child.poll() is None:
                    child.terminate()
                try:
                    child.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait(timeout=5)
                save(output / 'closure.json', dict(pid=child.pid, returncode=child.returncode,
                                                  reaped=True))
            if collector is not None:
                collector.finish(output / 'output-report.json')
            state['finished'] = time.time()
            save(output / 'state.json', state)
    try:
        assert not listeners(port), 'Owned listener survived child retirement'
        missing = subprocess.run([str(adb), 'devices', '-l'], env=client_env,
                                 capture_output=True, text=True, timeout=5)
        save(output / 'client-after-retirement.json', dict(returncode=missing.returncode,
             stdout=missing.stdout, stderr=missing.stderr))
        assert missing.returncode != 0
        assert 'cannot start server on remote host' in missing.stderr
        assert not listeners(port), 'Client resurrected the retired server'
        assert listener_owners(5037) == before, 'Shared ADB listener changed during probe'
        collector.check()
        assert collector.report['eof']
    except BaseException:
        state.update(state='failed', error=traceback.format_exc(), finished=time.time())
        save(output / 'state.json', state)
        raise
    state.update(state='passed', finished=time.time())
    save(output / 'state.json', state)
    # Never print or retain key contents generated by ADB.
    save(output / 'scratch-inventory.json', [dict(path=str(p.relative_to(scratch)),
         bytes=p.stat().st_size) for p in scratch.rglob('*') if p.is_file()])


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--adb', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--port', required=True, type=int)
    args = parser.parse_args()
    probe(args.adb, args.output, args.port)
