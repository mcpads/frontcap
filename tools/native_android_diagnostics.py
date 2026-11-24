"""Retain guest diagnostics without changing native probe acceptance."""
import argparse
import hashlib
import json
import os
import re
import shlex
from pathlib import Path
import subprocess
import sys
import time


def run_command(command, directory, *, timeout=20, input=None):
    """Retain command boundaries; observer failures never replace the command outcome."""
    started = time.monotonic_ns()
    identity = dict(command_id=f'{os.getpid()}-{started}', command=command, pid=os.getpid(),
                    started_monotonic_ns=started, host_time_ns=time.time_ns(), timeout_s=timeout)

    if input is not None:identity.update(input_bytes=len(input),input_sha256=hashlib.sha256(input).hexdigest())

    def record(**fields):
        try:
            with (Path(directory)/'adb-commands.jsonl').open('a') as stream:
                stream.write(json.dumps({**identity, **fields})+'\n')
        except InterruptedError:
            raise
        except OSError as error:
            print(f'Cannot save adb command evidence: {error}', file=sys.stderr)

    traced_inventory = list(command[-5:]) == ['shell', 'ps', '-A', '-o', 'PID,PPID,STAT,NAME']
    executed = command
    if traced_inventory:
        script = ('printf "FRONTCAP_INVENTORY shell-entered pid=%s\\n" "$$" >&2; '
                  'ps -A -o PID,PPID,STAT,NAME; result=$?; '
                  'printf "FRONTCAP_INVENTORY ps-exited status=%s\\n" "$result" >&2; exit "$result"')
        executed = [*command[:-5], 'shell', script]
        identity.update(adb_trace='adb,shell', executed_command=executed)
    record(state='started')
    stderr = b''
    try:
        if traced_inventory:
            result = subprocess.run(executed, capture_output=True, check=True, timeout=timeout,
                                    env={**os.environ, 'ADB_TRACE': 'adb,shell'})
            output, stderr = result.stdout, result.stderr
        else:
            output = subprocess.check_output(command, timeout=timeout, **({'input':input} if input is not None else {}))
    except BaseException as error:
        record(state='failed', finished_monotonic_ns=time.monotonic_ns(),
               exit_code=getattr(error, 'returncode', None), error=f'{type(error).__name__}: {error}',
               **({'stderr': (getattr(error, 'stderr', None) or b'').decode(errors='replace')}
                  if traced_inventory else {}))
        raise
    record(state='finished', finished_monotonic_ns=time.monotonic_ns(), exit_code=0, output_bytes=len(output),
           **({'stderr': stderr.decode(errors='replace')} if traced_inventory else {}))
    return output



def clear_app_data(command, package, destination):
    """Keep a failed clear authoritative while observing guest waits before teardown."""
    if not re.fullmatch(r'[A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)+', package):
        raise ValueError('Invalid diagnostic package')
    try:
        return command('shell', 'pm', 'clear', package, timeout=5)
    except InterruptedError:
        raise
    except (OSError, subprocess.SubprocessError) as failure:
        report = dict(package=package, phase='after-clear-failure', timeout_s=3,
                      operation_error=f'{type(failure).__name__}: {failure}',
                      started_monotonic_ns=time.monotonic_ns())
        # One read-only command shares one deadline. Access denials and disappearing
        # threads remain raw evidence; neither is interpreted as an empty inventory.
        script = "date +%s%N; cat /proc/uptime /proc/loadavg; "
        script += "inventory=$(ps -AT -o PID,TID,PPID,STAT,WCHAN,NAME,ARGS 2>&1); rc=$?; "
        script += "cat <<FRONTCAP_INVENTORY\n$inventory\nFRONTCAP_INVENTORY\n"
        script += "printf '\\nINVENTORY_EXIT=%s\\n' \"$rc\"; "
        # pidof excludes observed zombie cmd leaders. Select candidates from the
        # retained inventory; parent first, and no claim that every cmd is our request.
        script += "candidates=$(awk '$6 == \"cmd\" || $6 == \"[cmd]\" {print $3; print $1}' <<FRONTCAP_CANDIDATES\n"
        script += "$inventory\nFRONTCAP_CANDIDATES\n"
        script += " ); candidates=$(echo \"$candidates\" | awk '$1 > 0 && !seen[$1]++'); "
        script += 'for p in $candidates; do '
        script += 'for f in stat wchan syscall fd status schedstat task/*/stat; do '
        script += "printf '\\nCOMMAND_CANDIDATE_PID=%s FILE=%s\\n' \"$p\" \"$f\"; "
        script += 'if [ "$f" = fd ]; then ls -l /proc/$p/fd; else cat /proc/$p/$f; fi 2>&1; '
        script += "printf '\\nREAD_EXIT=%s\\n' \"$?\"; done; "
        script += "printf '\\nCOMMAND_CANDIDATE_PID=%s FILE=stat-after\\n' \"$p\"; "
        script += "cat /proc/$p/stat 2>&1; printf '\\nREAD_EXIT=%s\\n' \"$?\"; done; "
        script += "for name in system_server installd artd cmd dex2oat64; do "
        script += "printf '\\nSERVICE=%s\\n' \"$name\"; "
        script += "pids=$(pidof $name 2>&1); rc=$?; "
        script += "printf '\\nPIDOF_EXIT=%s OUTPUT=%s\\n' \"$rc\" \"$pids\"; "
        script += '[ "$rc" = 0 ] || continue; for p in $pids; do '
        script += "printf '\\nPID=%s THREAD_STATS\\n' \"$p\"; "
        script += "cat /proc/$p/task/*/stat 2>&1; "
        script += "printf '\\nSTATS_EXIT=%s\\n' \"$?\"; done; done"
        # These enqueue completion markers; they do not clear/retry app data.
        # Compare queues within the same observer deadline, never the default 60s wait.
        script += "; for handler in wait-for-handler wait-for-background-handler; do "
        script += "printf '\\nHANDLER=%s\\n' \"$handler\"; "
        script += 'cmd package "$handler" --timeout 100 2>&1; '
        script += "printf '\\nHANDLER_EXIT=%s\\n' \"$?\"; done"
        report['command'] = ['shell', script]
        try:
            report['output'] = command(*report['command'], timeout=3).decode(errors='replace')
            if '\nINVENTORY_EXIT=' not in report['output']:
                raise ValueError('Clear diagnostic inventory output is missing')
        except InterruptedError:
            raise
        except Exception as error:
            report['error'] = f'{type(error).__name__}: {error}'
            partial = getattr(error, 'output', None)
            if partial is not None:
                report['output'] = partial.decode(errors='replace') if isinstance(partial, bytes) else partial
        report['finished_monotonic_ns'] = time.monotonic_ns()
        try:
            from native_clock_control import atomic_json
            atomic_json(destination, report)
        except InterruptedError:
            raise
        except Exception as error:
            print(f'Cannot save clear-data diagnostic: {error}', file=sys.stderr)
            print(json.dumps(report), file=sys.stderr)
        raise


def capture_thread_ids(command, pid, destination):
    """Retain TIDs before force-stop; a dead leader can hide its task directory."""
    report = dict(pid=pid, phase='before-force-stop', started_monotonic_ns=time.monotonic_ns(),
                  timeout_s=3, threads=[])
    try:
        if type(pid) is not int or pid <= 0:
            raise ValueError('Invalid process identity')
        report['output'] = command('shell', 'cat', f'/proc/{pid}/task/*/stat', timeout=3).decode()
        threads = []
        for line in report['output'].splitlines():
            fields = line[line.rfind(')')+2:].split()
            tid = int(line.split(' ',1)[0])
            start_ticks = int(fields[19])
            if tid <= 0 or start_ticks < 0:
                raise ValueError('Invalid thread identity')
            threads.append(dict(tid=tid, start_ticks=start_ticks))
        if not threads or pid not in [row['tid'] for row in threads]:
            raise ValueError('Thread inventory has no leader')
        if len({row['tid'] for row in threads}) != len(threads):
            raise ValueError('Duplicate thread identity')
        report['threads'] = threads
    except InterruptedError:
        raise
    except Exception as error:
        report['error'] = f'{type(error).__name__}: {error}'
        partial = getattr(error, 'output', None)
        if partial is not None:
            report['output'] = partial.decode(errors='replace') if isinstance(partial,bytes) else partial
    report['finished_monotonic_ns'] = time.monotonic_ns()
    try:
        from native_clock_control import atomic_json
        atomic_json(destination, report)
    except InterruptedError:
        raise
    except Exception as error:
        print(f'Cannot save pre-retirement thread evidence: {error}', file=sys.stderr)
        print(json.dumps(report), file=sys.stderr)
    return report


def capture_retiring_threads(command, pid, destination, *, package, thread_snapshot,
                             inventory, inventory_monotonic_ns, deadline):
    """Opt-in early observation; consume only the caller's remaining retirement budget."""
    report = dict(pid=pid, phase='first-zombie', inventory=inventory,
                  inventory_monotonic_ns=inventory_monotonic_ns,
                  started_monotonic_ns=time.monotonic_ns(), thread_snapshot=thread_snapshot)
    observation_deadline = min(deadline, time.monotonic()+.350)
    try:
        if (type(pid) is not int or pid <= 0 or thread_snapshot.get('pid') != pid
                or not re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]*(?:\.[A-Za-z_][A-Za-z_0-9]*)+', package)):
            raise ValueError('Invalid early retirement identity')
        threads = thread_snapshot.get('threads', [])
        if (not threads or any(type(t.get('tid')) is not int or t['tid'] <= 0
                               or type(t.get('start_ticks')) is not int or t['start_ticks'] < 0
                               for t in threads)):
            raise ValueError('Early retirement thread baseline unavailable')
        tids = [t['tid'] for t in threads]
        if pid not in tids or len(tids) != len(set(tids)):
            raise ValueError('Invalid early retirement thread baseline')
        siblings = [tid for tid in tids if tid != pid]
        report['detail_tids'] = siblings
        script = "printf 'STATS\\n'; cat " + ' '.join(f'/proc/{tid}/stat' for tid in tids) + ' 2>&1; '
        if siblings:
            # Re-read identity after all details, including the final status read.
            script += 'for f in wchan syscall schedstat stack status stat; do for t in '
            script += ' '.join(map(str, siblings)) + '; do [ -e /proc/$t/stat ] || continue; '
            script += 'printf "\\nTID=%s FILE=%s\\n" "$t" "$f"; cat /proc/$t/$f 2>&1; '
            script += 'printf "\\nEXIT=%s\\n" "$?"; done; done'
        report['command'] = ['shell', 'run-as', package, 'sh', '-c', shlex.quote(script)]
        remaining = observation_deadline-time.monotonic()
        report['timeout_s'] = max(0, remaining)
        if remaining <= 0:
            raise TimeoutError('No retirement observation budget remains')
        report['output'] = command(*report['command'], timeout=remaining).decode(errors='replace')
    except InterruptedError:
        raise
    except Exception as error:
        report['error'] = f'{type(error).__name__}: {error}'
        partial = getattr(error, 'output', None)
        if partial is not None:
            report['output'] = partial.decode(errors='replace') if isinstance(partial, bytes) else partial
    report['finished_monotonic_ns'] = time.monotonic_ns()
    try:
        from native_clock_control import atomic_json
        atomic_json(destination, report)
    except InterruptedError:
        raise
    except Exception as error:
        print(f'Cannot save early retirement evidence: {error}', file=sys.stderr)
        print(json.dumps(report), file=sys.stderr)
    return report


def capture_retirement(command, pid, processes, destination, *, package=None, thread_snapshot=None):
    """Observe a failed retirement without changing its verdict or signaling its parent."""
    report = dict(pid=pid, inventory=processes, started_monotonic_ns=time.monotonic_ns(),
                  timeout_s=3, phase='after-retirement-deadline')
    try:
        lines = [line.split() for line in processes.splitlines()]
        if not lines or lines[0] != ['PID', 'PPID', 'STAT', 'NAME']:
            raise ValueError('Parent inventory unavailable')
        targets = [row for row in lines[1:] if row[0] == str(pid)]
        if len(targets) != 1:
            raise ValueError('Target parent unavailable in final inventory')
        parent = int(targets[0][1])
        if type(pid) is not int or pid <= 0 or parent < 0:
            raise ValueError('Invalid process identity')
        report['parent_pid'] = parent
        threads = []
        if thread_snapshot is not None:
            report['thread_snapshot'] = thread_snapshot
            if thread_snapshot.get('pid') != pid:
                raise ValueError('Thread snapshot belongs to another process')
            threads = thread_snapshot.get('threads', [])
            if any(type(row.get('tid')) is not int or row['tid'] <= 0 for row in threads):
                raise ValueError('Invalid saved thread identity')
        tids = [row['tid'] for row in threads if row['tid'] != pid]
        thread_script = ''
        if tids:
            # Direct TID paths remain addressable when the group leader's task directory is gone.
            # Capture all stats in one read before spending the shared deadline on wait details.
            thread_script = "printf '\\nTHREAD_STATS\\n'; cat " + ' '.join(f'/proc/{tid}/stat' for tid in tids)
            thread_script += " 2>&1; printf '\\nEXIT=%s\\n' \"$?\"; "
            thread_script += 'for t in ' + ' '.join(map(str,tids)) + '; do '
            thread_script += '[ -e /proc/$t/stat ] || continue; '
            thread_script += 'for tf in wchan syscall fd status schedstat stack stat; do '
            thread_script += 'printf "\\nTID=%s FILE=%s\\n" "$t" "$tf"; '
            thread_script += 'if [ "$tf" = fd ]; then ls -l /proc/$t/fd; else cat /proc/$t/$tf; fi 2>&1; '
            thread_script += 'printf "\\nEXIT=%s\\n" "$?"; done; done; '

        # stat before/after preserves start ticks for detecting reuse during this later
        # observation. The earlier ps row alone does not establish a stable identity.
        pids = list(dict.fromkeys(p for p in (parent, pid) if p != 0))
        report['observed_pids'] = pids
        script = 'for p in ' + ' '.join(map(str, pids)) + '; do '
        script += 'for f in stat wchan syscall fd status schedstat stack; do '
        script += 'printf "\\nPID=%s FILE=%s\\n" "$p" "$f"; '
        script += 'if [ "$f" = fd ]; then ls -l /proc/$p/fd; else cat /proc/$p/$f; fi 2>&1; printf "\\nEXIT=%s\\n" "$?"; '
        if thread_script:
            script += f'if [ "$p" = {pids[0]} ] && [ "$f" = fd ]; then ' + thread_script + 'fi; '
        script += 'done; '
        script += 'printf "\\nPID=%s FILE=stat-after\\n" "$p"; cat /proc/$p/stat 2>&1; '
        script += 'printf "\\nEXIT=%s\\n" "$?"; done'
        report['contexts'] = ['shell']
        if package is not None:
            if not re.fullmatch(r'[A-Za-z0-9_]+(?:\.[A-Za-z0-9_]+)+', package):
                raise ValueError('Invalid diagnostic package')
            report['package'] = package
            report['contexts'] = ['run-as', 'shell']
            # Both views share one command deadline. run-as is limited to the supplied
            # debuggable fixture; unavailable access remains in the raw output.
            # App permissions expose wait/descriptors hidden from shell. Capture those
            # before bulky status/stack reads, while the parent may still exist.
            script = ("printf '\\nCONTEXT=run-as\\n'; run-as " + shlex.quote(package)
                      + ' sh -c ' + shlex.quote(script) + ' 2>&1; '
                      + "printf '\\nRUN_AS_EXIT=%s\\n' \"$?\"; "
                      + "printf '\\nCONTEXT=shell\\n'; " + script)
        report['command'] = ['shell', script]
        report['output'] = command(*report['command'], timeout=report['timeout_s']).decode(errors='replace')
    except InterruptedError:
        raise
    except Exception as error:
        report['error'] = f'{type(error).__name__}: {error}'
        partial = getattr(error, 'output', None)
        if partial is not None:
            report['output'] = partial.decode(errors='replace') if isinstance(partial, bytes) else partial
    report['finished_monotonic_ns'] = time.monotonic_ns()
    try:
        from native_clock_control import atomic_json
        atomic_json(destination, report)
    except InterruptedError:
        raise
    except Exception as error:
        print(f'Cannot save retirement diagnostic: {error}', file=sys.stderr)
        print(json.dumps(report), file=sys.stderr)
    return report


def capture(adb, directory, phase):
    report = dict(phase=phase, started=time.time(), commands=[])
    commands = [('clock', ['shell', 'date +%s%N; cat /proc/uptime /proc/loadavg']),
                ('broadcasts', ['shell', 'dumpsys', 'activity', 'broadcasts']),
                ('cpu-online', ['shell', 'cat', '/sys/devices/system/cpu/online']),
                ('memory', ['shell', 'cat', '/proc/meminfo'])]
    if phase != 'startup':
        commands += [('processes', ['shell', 'ps', '-A', '-o', 'PID,NAME,ARGS']),
                     ('build', ['shell', 'getprop', 'ro.build.fingerprint']),
                     # ART is an APEX module; the OS fingerprint alone cannot identify it.
                     ('apex-info', ['shell', 'cat', '/apex/apex-info-list.xml']),
                     ('art-sha256', ['shell', 'sha256sum', '/apex/com.android.art/lib64/libart.so'])]
    if phase == 'after':
        # DropBox keeps completed crash reports that may not yet be in the crash buffer.
        commands += [('dropbox', ['shell', 'dumpsys', 'dropbox', '--print']),
                     ('crash', ['logcat', '-b', 'crash', '-d']),
                     ('logcat', ['logcat', '-b', 'all', '-d', '-v', 'epoch']),
                     ('cpuinfo', ['shell', 'dumpsys', 'cpuinfo'])]
    for name, args in commands:
        record = dict(command=[*adb, *args], started=time.time(), started_monotonic_ns=time.monotonic_ns())
        report['commands'].append(record)
        try:
            with (directory / f'guest-{phase}-{name}.txt').open('wb') as stream:
                result = subprocess.run([*adb, *args], stdout=stream, stderr=subprocess.STDOUT,
                                        timeout=5)
            record['exit_code'] = result.returncode
            if result.returncode:
                record['error'] = 'Diagnostic command failed; inspect captured output'
        except InterruptedError:
            raise
        except (OSError, subprocess.SubprocessError) as error:
            record['error'] = str(error)
        record['finished'] = time.time()
        record['finished_monotonic_ns'] = time.monotonic_ns()
    report['finished'] = time.time()
    try:
        (directory / f'guest-{phase}-diagnostics.json').write_text(json.dumps(report, indent=2) + '\n')
    except InterruptedError:
        raise
    except OSError as error:
        report['report_error'] = str(error)
        # The shell retains stderr in run.log. Optional diagnostics must not stop the probe.
        print('Cannot save guest diagnostic report; retaining its outcome in the run log', file=sys.stderr)
        print(json.dumps(report), file=sys.stderr)
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    parser.add_argument('sdk', type=Path)
    parser.add_argument('port')
    parser.add_argument('phase', choices=['startup', 'before', 'after'])
    args = parser.parse_args()
    capture([str(args.sdk / 'platform-tools/adb'), '-s', 'emulator-' + args.port],
            args.directory, args.phase)
