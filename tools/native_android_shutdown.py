"""Owned ART shutdown comparison; expected pre-startup timeout is not runtime success."""
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import time
import uuid

from native_android_diagnostics import run_command
from native_android_processes import parse_inventory
from native_clock_control import atomic_json

MODES = ('before-startup', 'startup-complete')


def process_identity(raw):
    pid, _, tail = raw.partition(' (')
    fields = tail.rsplit(')', 1)[1].split()
    return dict(pid=int(pid), start_ticks=int(fields[19]), state=fields[0])


def observe_timeout_target(folder, command, identity, timeout_ns):
    """Observe once after the verdict, before transport termination; never extend its deadline."""
    observation = dict(timeout_monotonic_ns=timeout_ns, started_monotonic_ns=time.monotonic_ns(),
                       timeout_s=1, identity=identity)
    try:
        if identity is None: raise RuntimeError('Target identity unavailable at timeout')
        raw = command('shell', 'cat', f"/proc/{identity['pid']}/stat", timeout=1)
        (folder/'timeout-stat.txt').write_bytes(raw)
    except InterruptedError:
        raise
    except Exception as error:
        observation['error'] = f'{type(error).__name__}: {error}'
    finally:
        observation['finished_monotonic_ns'] = time.monotonic_ns()
        atomic_json(folder/'timeout-observation.json', observation)


def acquire(root, adb, hold_us):
    root = Path(root)
    report = dict(state='running', hold_us=hold_us, cases=[])
    build = root/'shutdown-build'
    report['binaries'] = {name: hashlib.sha256((build/name).read_bytes()).hexdigest()
                          for name in ('dex/classes.dex', 'libvclock.so')}
    def save(): atomic_json(root/'shutdown.json', report)
    def call(*args, timeout=5): return run_command([*adb, *args], root, timeout=timeout)
    save()
    try:
        for mode in MODES:
            folder = root/mode
            folder.mkdir()
            remote = '/data/local/tmp/frontcap-shutdown-'+uuid.uuid4().hex
            row = dict(mode=mode, state='preparing', remote=remote, timeout_s=15, cleanup_signal_sent=False)
            report['cases'].append(row)
            save()
            call('shell', 'mkdir', remote)
            for name in ('dex/classes.dex', 'libvclock.so'):
                call('push', str(build/name), remote+'/'+Path(name).name)
            call('shell', 'touch', remote+'/current.prof', remote+'/reference.prof')
            command = [*adb, 'shell', 'env', 'FC_MANUAL=1', 'FC_HOST_RENDER=1',
                       'LD_PRELOAD='+remote+'/libvclock.so', 'dalvikvm', '-Xusejit:true',
                       '-Xjitsaveprofilinginfo', '-cp', remote+'/classes.dex',
                       'dev.frontcap.s18.ShutdownControl', mode, remote+'/libvclock.so',
                       remote+'/current.prof', remote+'/reference.prof', remote+'/classes.dex', str(hold_us//1000)]
            row.update(command=command, state='running')
            child = None
            identity = None
            try:
                with (folder/'stdout.txt').open('wb') as stream:
                    began_ns = time.monotonic_ns()
                    deadline_ns = began_ns+15_000_000_000
                    row['command_started_monotonic_ns'] = began_ns
                    row['deadline_monotonic_ns'] = deadline_ns
                    child = subprocess.Popen(command, stdout=stream, stderr=subprocess.STDOUT)
                    row['transport_pid'] = child.pid
                    save()
                    while child.poll() is None and time.monotonic_ns() < deadline_ns:
                        match = re.search(r'^FC_PID (\d+)$', (folder/'stdout.txt').read_text(), re.M)
                        if identity is None and match:
                            pid = int(match[1])
                            try:
                                raw = call('shell', 'cat', f'/proc/{pid}/stat',
                                           timeout=min(1, max(.01, (deadline_ns-time.monotonic_ns())/1e9)))
                                identity = process_identity(raw.decode())
                                if identity['pid'] != pid: raise RuntimeError('Target identity differs')
                                (folder/'live-stat.txt').write_bytes(raw)
                                row['identity'] = identity
                                save()
                            except subprocess.CalledProcessError:
                                pass  # A quick successful exit can precede observation; no destructive action follows.
                        time.sleep(.02)
                    row['timed_out'] = child.poll() is None
                    if row['timed_out']:
                        row['timeout_monotonic_ns'] = time.monotonic_ns()
                        save()  # Preserve the original timeout before the separate read-only observation.
                        observe_timeout_target(folder, call, identity, row['timeout_monotonic_ns'])
                        child.kill()
                        child.wait(timeout=3)
                    row.update(exit_code=child.returncode, duration_s=(time.monotonic_ns()-began_ns)/1e9)
            finally:
                if child is not None and child.poll() is None:
                    child.kill()
                    child.wait(timeout=3)
                save()
            output = (folder/'stdout.txt').read_text()
            pids = re.findall(r'^FC_PID (\d+)$', output, re.M)
            if len(pids) != 1: raise RuntimeError('Unique target PID unavailable; retire owned emulator')
            pid = int(pids[0])
            row['target_pid'] = pid
            raw = call('shell', 'ps', '-A', '-o', 'PID,PPID,STAT,NAME')
            (folder/'processes-before-cleanup.txt').write_bytes(raw)
            present = any(p['pid'] == pid for p in parse_inventory(raw.decode()))
            row['target_present_after_command'] = present
            if present:
                try:
                    raw = call('shell', 'cat', f'/proc/{pid}/stat')
                    current = process_identity(raw.decode())
                    (folder/'cleanup-stat.txt').write_bytes(raw)
                    if identity is None or any(identity[k] != current[k] for k in ('pid', 'start_ticks')):
                        raise RuntimeError('Target identity unavailable/changed; retire owned emulator')
                    if current['state'] != 'Z':
                        row['cleanup_signal_sent'] = True
                        save()
                        call('shell', 'kill', '-9', str(pid))
                except subprocess.CalledProcessError:
                    # A disappearing /proc entry must be resolved by the independent inventory below.
                    pass
            deadline = time.monotonic()+2
            while True:
                remaining = deadline-time.monotonic()
                if remaining <= 0: raise TimeoutError('Owned shutdown target did not retire')
                raw = call('shell', 'ps', '-A', '-o', 'PID,PPID,STAT,NAME', timeout=remaining)
                (folder/'processes-after-cleanup.txt').write_bytes(raw)
                if not any(p['pid'] == pid for p in parse_inventory(raw.decode())): break
                time.sleep(.05)
            row.update(state='finished', target_absent=True)
            save()
            call('shell', 'rm', '-r', remote)
        report['state'] = 'finished'
    except BaseException as error:
        report.update(state='failed', error=f'{type(error).__name__}: {error}')
        raise
    finally:
        save()


def check_shutdown(directory, expected_hold_us):
    root = Path(directory)
    report = json.loads((root/'shutdown.json').read_text())
    checks = []
    def record(name, ok, detail): checks.append(dict(name='shutdown.'+name, passed=bool(ok), detail=detail))
    record('cases', report.get('state') == 'finished' and report.get('hold_us') == expected_hold_us
           and [c.get('mode') for c in report['cases']] == list(MODES), report.get('state'))
    for name, digest in report['binaries'].items():
        record('binary.'+name, name in ('dex/classes.dex', 'libvclock.so') and
               hashlib.sha256((root/'shutdown-build'/name).read_bytes()).hexdigest() == digest, digest)
    record('binary-set', set(report['binaries']) == {'dex/classes.dex', 'libvclock.so'}, list(report['binaries']))
    for row in report['cases']:
        mode = row['mode']
        if mode not in MODES: continue
        folder = root/mode
        text = (folder/'stdout.txt').read_text()
        def one(prefix, decode=json.loads):
            values = [decode(line[len(prefix):]) for line in text.splitlines() if line.startswith(prefix)]
            if len(values) != 1: raise ValueError('Expected one '+prefix)
            return values[0]
        prefixes = ['FC_PID ', 'FC_INITIAL_READY ']
        if mode == 'startup-complete': prefixes += ['FC_STARTUP_NOTIFIED', 'FC_STARTUP_READY ']
        prefixes += ['FC_HOLD ', 'FC_MAIN_RETURN ']
        markers = [prefix for line in text.splitlines() for prefix in prefixes if line.startswith(prefix)]
        record(mode+'.ordering', markers == prefixes, markers)
        pid = one('FC_PID ', int)
        initial, final, hold = one('FC_INITIAL_READY '), one('FC_MAIN_RETURN '), one('FC_HOLD ')
        record(mode+'.identity', pid == row['target_pid'] and pid > 0, pid)
        record(mode+'.initial', initial['active_timed'] == 1 and initial['active_untimed'] == 0
               and initial['wait_clock_ns'] == 0, initial)
        record(mode+'.hold', hold['requested_ms']*1000 == expected_hold_us
               and hold['elapsed_ns'] >= expected_hold_us*1000, hold)
        record(mode+'.clock', final['wait_clock_ns'] == 0, final)
        before = parse_inventory((folder/'processes-before-cleanup.txt').read_text())
        after = parse_inventory((folder/'processes-after-cleanup.txt').read_text())
        record(mode+'.retired', row['state'] == 'finished' and row.get('target_absent') is True
               and not any(p['pid'] == pid for p in after), row['state'])
        if mode == 'before-startup':
            try:
                observation = json.loads((folder/'timeout-observation.json').read_text())
                initial_identity = process_identity((folder/'live-stat.txt').read_text())
                timeout_identity = process_identity((folder/'timeout-stat.txt').read_text())
                bound = (not observation.get('error') and observation['identity'] == row['identity']
                         and initial_identity == row['identity']
                         and all(timeout_identity[k] == initial_identity[k] for k in ('pid', 'start_ticks'))
                         and timeout_identity['pid'] == pid and timeout_identity['state'] not in ('Z', 'X', 'x')
                         and row['deadline_monotonic_ns']-row['command_started_monotonic_ns'] == 15_000_000_000
                         and row['timeout_monotonic_ns'] >= row['deadline_monotonic_ns']
                         and observation['timeout_monotonic_ns'] == row['timeout_monotonic_ns']
                         and observation['started_monotonic_ns'] >= row['timeout_monotonic_ns']
                         and observation['finished_monotonic_ns'] >= observation['started_monotonic_ns']
                         and observation['timeout_s'] == 1)
                detail = dict(identity=timeout_identity, observation=observation)
            except (OSError, ValueError, KeyError, IndexError, TypeError) as error:
                bound, detail = False, str(error)
            record(mode+'.timeout-target', bound, detail)
            record(mode+'.counterexample', row['timeout_s'] == 15 and row['timed_out'] is True
                   and row['exit_code'] != 0 and row['duration_s'] >= 15
                   and final['active_timed'] == 1 and final['active_untimed'] == 0
                   and 'FC_STARTUP_' not in text, dict(exit_code=row['exit_code'], duration_s=row['duration_s']))
        else:
            ready = one('FC_STARTUP_READY ')
            record(mode+'.startup', text.count('FC_STARTUP_NOTIFIED\n') == 1
                   and ready['active_timed'] == 0 and ready['active_untimed'] == 1
                   and ready['wait_clock_ns'] == 0 and final['active_timed'] == 0
                   and final['active_untimed'] == 1, ready)
            record(mode+'.graceful', row['timeout_s'] == 15 and row['timed_out'] is False
                   and row['exit_code'] == 0 and 0 < row['duration_s'] <= 15
                   and row['cleanup_signal_sent'] is False and row['target_present_after_command'] is False
                   and not any(p['pid'] == pid for p in before), row['duration_s'])
    return dict(passed=all(c['passed'] for c in checks), checks=checks)


if __name__ == '__main__':
    root, sdk, port = sys.argv[1:]
    def interrupted(signum, frame): raise InterruptedError('Shutdown control interrupted')
    signal.signal(signal.SIGTERM, interrupted)
    acquire(root, [str(Path(sdk)/'platform-tools/adb'), '-s', 'emulator-'+port], int(os.environ['FC_HOLD_US']))
