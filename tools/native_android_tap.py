"""Diagnostic lifetime receipts around the existing adb input subprocess."""
import json
import os
import signal
import subprocess
import time
from native_clock_control import atomic_json


def host_processes(*, timeout=5):
    result = subprocess.run(['ps','-axo','pid=,ppid=,pgid=,stat=,lstart=,command='],
                            capture_output=True,text=True,timeout=timeout)
    result.check_returncode()
    if result.stderr.strip(): raise RuntimeError(result.stderr)
    rows = []
    for line in result.stdout.splitlines():
        fields = line.split(None,9)
        if len(fields) != 10: raise RuntimeError('Incomplete host process inventory: '+line)
        rows.append(dict(pid=int(fields[0]),ppid=int(fields[1]),group=int(fields[2]),state=fields[3],
                         started=' '.join(fields[4:9]),command=fields[9]))
    return rows


def matching_processes(rows, identity):
    return [r for r in rows if r['pid']==identity['pid'] and r['started']==identity['started']
            and (r['command']==identity['command'] or 'Z' in r['state']) and r['group']==identity['group']]


def wait_for_exit(identity, observations):
    deadline=time.monotonic()+2
    while True:
        started=time.monotonic()
        remaining=deadline-started
        if remaining<=0: return False
        observation=dict(started_monotonic_s=started, deadline_monotonic_s=deadline,
                         timeout_s=remaining)
        observations.append(observation)
        try:
            # exec and process-group changes do not prove exit. Do not signal an
            # unfamiliar identity, even when the original PID is still present.
            current=[p for p in host_processes(timeout=remaining)
                     if p['pid']==identity['pid'] and p['started']==identity['started']]
            observation['processes']=current
        except subprocess.TimeoutExpired as error:
            observation['error']=f'{type(error).__name__}: {error}'
            return False
        except BaseException as error:
            observation['error']=f'{type(error).__name__}: {error}'
            raise
        finally:
            observation['finished_monotonic_s']=time.monotonic()
            observation['monotonic_ns']=time.monotonic_ns()
        if observation['finished_monotonic_s']>=deadline: return False
        if not current: return True
        remaining=deadline-time.monotonic()
        if remaining<=0: return False
        time.sleep(min(.02,remaining))


def retire_process(identity):
    evidence=dict(identity=identity,observations=[],signals=[])
    if wait_for_exit(identity,evidence['observations']):
        evidence['absent']=True;return evidence
    for sig in (signal.SIGTERM,signal.SIGKILL):
        current=matching_processes(host_processes(),identity)
        if current and 'Z' not in current[0]['state']:
            os.kill(identity['pid'],sig);evidence['signals'].append(int(sig))
        if wait_for_exit(identity,evidence['observations']):
            evidence['absent']=True;return evidence
    evidence['absent']=False;return evidence


def run_tap(command, directory, *, record_output=False):
    started=time.monotonic()
    state=json.loads((directory/'input-client.json').read_text())
    row=dict(phase='starting',parent_pid=os.getpid(),command=command,request=state['pending'],started_monotonic_ns=time.monotonic_ns())
    with subprocess.Popen(command,stdout=subprocess.PIPE) as process:
        row['pid']=process.pid
        try:
            observed=[p for p in host_processes() if p['pid']==process.pid]
            if len(observed)!=1 or observed[0]['ppid']!=os.getpid():
                raise RuntimeError('Tap child identity unavailable before wait')
            row['identity_observed']=observed[0]
            row['identity']={**observed[0],'command':' '.join(command)}
            row['phase']='started';atomic_json(directory/'input-transport.json',row)
            output,_=process.communicate(timeout=max(0,20-(time.monotonic()-started)))
            if record_output:
                (directory/'input-transport.stdout').write_bytes(output)
            if process.returncode:
                raise subprocess.CalledProcessError(process.returncode,command,output=output)
            return output
        except BaseException as error:
            row['error']=f'{type(error).__name__}: {error}'
            raise
        finally:
            if process.poll() is None:
                process.kill();process.communicate(timeout=5)
            row.update(phase='finished',exit_code=process.returncode,finished_monotonic_ns=time.monotonic_ns())
            atomic_json(directory/'input-transport.json',row)


def run_suspended_tap(command, directory):
    """Opt-in fault boundary: an actual adb child exists but has not run user code."""
    from native_suspended_process import spawn_suspended, wait_child
    started = time.monotonic()
    state = json.loads((directory/'input-client.json').read_text())
    row = dict(phase='starting', parent_pid=os.getpid(), command=command, request=state['pending'],
               started_monotonic_ns=time.monotonic_ns(), start_suspended=True)
    pid, exit_code = None, None
    output = directory/'input-transport.stdout'
    try:
        with output.open('wb') as stream:
            pid = spawn_suspended(command, stream.fileno())
        row['pid'] = pid
        observed = [p for p in host_processes() if p['pid'] == pid]
        if (len(observed) != 1 or observed[0]['ppid'] != os.getpid()
                or observed[0]['group'] != os.getpgrp() or 'T' not in observed[0]['state']
                or observed[0]['command'] != ' '.join(command)):
            raise RuntimeError('Actual suspended tap child identity unavailable')
        row.update(identity=observed[0], identity_observed=observed[0], phase='suspended')
        atomic_json(directory/'input-transport.json', row)
        exit_code = wait_child(pid, max(0, 20-(time.monotonic()-started)))
        data = output.read_bytes()
        if exit_code:
            raise subprocess.CalledProcessError(exit_code, command, output=data)
        return data
    except BaseException as error:
        row['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        try:
            if pid is not None and exit_code is None:
                try:
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
                exit_code = wait_child(pid, 2)
        except BaseException as error:
            row['cleanup_error'] = f'{type(error).__name__}: {error}'
            raise
        finally:
            row.update(phase='finished', exit_code=exit_code, finished_monotonic_ns=time.monotonic_ns())
            atomic_json(directory/'input-transport.json', row)
