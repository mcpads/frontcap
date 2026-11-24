"""Isolate filesystem metadata queries from the sampler's bounded observation loop."""
import json
import subprocess
import time


def process_files(executable, pid, identity, *, deadline):
    started = time.monotonic_ns()
    until = deadline
    result = dict(pid=pid, expected_identity=identity, started_monotonic_ns=started,
                  deadline_monotonic_ns=int(until*1e9), status='unavailable', identity_verified=False,
                  files=[], observations=[])
    child = None
    try:
        if (type(pid) is not int or not 0 < pid < 2**31 or identity.get('pid') != pid
                or not identity.get('start_abstime') or not identity.get('executable_uuid')):
            raise ValueError('Expected an identified owned process')
        if time.monotonic() >= until:raise TimeoutError('File observation budget exhausted')
        command = [str(executable),str(pid),str(identity['start_abstime']),identity['executable_uuid'],str(int(until*1e9))]
        result['command'] = command
        child = subprocess.Popen(command,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
        result['observer_pid'] = child.pid
        try:
            output, errors = child.communicate(timeout=max(0,until-time.monotonic()))
        except subprocess.TimeoutExpired:
            child.kill();output, errors = child.communicate()
            result['error'] = 'File observation deadline exceeded'
        result.update(exit_code=child.returncode,stderr=errors.decode(errors='replace'))
        for line in output.splitlines(keepends=True):
            if line.endswith(b'\n'):result['observations'].append(json.loads(line))
        rows = result['observations'];final = [r for r in rows if r['phase']=='end']
        result['files'] = [r for r in rows if r['phase']=='file']
        if len(final)==1 and final[0]['identity_verified']:
            result['identity_verified'] = True
            result['status'] = 'available' if not child.returncode and final[0]['complete'] and 'error' not in result else 'partial'
        if final and final[0].get('error'):result.setdefault('error',final[0]['error'])
    except InterruptedError:
        raise
    except (OSError,ValueError,subprocess.SubprocessError) as error:
        result['error'] = str(error)
    finally:
        if child is not None:
            if child.poll() is None:child.kill()
            child.wait()
            result['exit_code'] = child.returncode
            for stream in (child.stdout,child.stderr):
                if stream is not None:stream.close()
        result['finished_monotonic_ns'] = time.monotonic_ns()
    return result
