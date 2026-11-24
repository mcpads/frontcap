"""Bounded subprocess execution and evidence publication for native hosts."""
import datetime
import json
import os
import shlex
import signal
import subprocess
import sys
import time

from native_output_log import OutputLog
from native_write_budget import charge_payload
from native_process_diagnostics import capture_timeout, group_processes


class WorkerInterrupted(BaseException):
    """Cancellation must escape syscall retries and ordinary diagnostic errors."""


def now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat()


def write_json(path, value):
    data = (json.dumps(value, indent=2) + "\n").encode()
    charge_payload(len(data))
    temporary = path.with_suffix(".tmp")
    temporary.write_bytes(data)
    temporary.replace(path)


def signal_owned_group(group, signum, log):
    try:
        os.killpg(group, signum)
    except ProcessLookupError:
        pass
    except PermissionError as error:
        # Darwin can return EPERM for a group containing only unreaped zombies.
        # Establish that no live target remains; never suppress a live/unknown denial.
        report = dict(group=group, signal=int(signum), errno=error.errno, observed=now())
        try:
            report['processes'] = group_processes(group)
            report['no_live_members'] = all(row['state'].startswith('Z') for row in report['processes'])
        except (InterruptedError, WorkerInterrupted):
            raise
        except Exception as observation_error:
            report.update(no_live_members=False, observation_error=str(observation_error))
        with log.with_name(log.stem + '-cleanup-signals.jsonl').open('a') as stream:
            stream.write(json.dumps(report) + '\n')
        if not report['no_live_members']:
            raise


def execute(command, log, env, timeout, cwd, *, output_limit=4 * 1024 * 1024):
    if type(output_limit) is not int or output_limit < 0:
        raise ValueError('Invalid command log bound')
    with log.open("wb") as stream:
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                   cwd=cwd, env=env, start_new_session=True)

        def observe_exit():
            # Popen completed setsid before returning. Keep that original child
            # unreaped so its PID still pins the session/group we may signal.
            if process.returncode is not None:
                raise RuntimeError('Command leader already reaped; group ownership is lost')
            observation = os.waitid(os.P_PID, process.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
            if observation is not None and observation.si_pid != process.pid:
                raise RuntimeError('Command exit observation names another child')
            return observation

        collector = OutputLog(process.stdout, stream, output_limit)
        deadline = time.monotonic() + timeout
        primary_error = None
        try:
            try:
                while True:
                    collector.check()
                    left = deadline - time.monotonic()
                    if left <= 0:
                        raise subprocess.TimeoutExpired(command, timeout)
                    if observe_exit() is not None:
                        break
                    time.sleep(min(.05, left))
            except subprocess.TimeoutExpired:
                try:
                    capture_timeout(process.pid, log)
                except Exception as error:
                    print(f"Timeout diagnostics failed: {error}", file=sys.stderr)
                raise RuntimeError(f"Timed out after {timeout}s: {shlex.join(command)}")
        except BaseException as error:
            primary_error = error
            raise
        finally:
            try:
                observe_exit()
                signal_owned_group(process.pid, signal.SIGTERM, log)
                retirement_deadline = time.monotonic() + 30
                while observe_exit() is None and time.monotonic() < retirement_deadline:
                    time.sleep(.05)
                # Even an exited leader must remain unreaped until the final
                # group signal. ECHILD or any observation error revokes authority.
                observe_exit()
                signal_owned_group(process.pid, signal.SIGKILL, log)
                code = process.wait(timeout=10)
            except BaseException as error:
                detail = f'Command group cleanup unproven: {type(error).__name__}: {error}'
                collector.report['cleanup_error'] = detail
                if primary_error is None:
                    raise
                primary_error.add_note(detail)
            finally:
                report = collector.finish(log.with_name(log.name + '.output.json'))
        if report['error']:
            raise RuntimeError(report['error'])
        if not report['eof']:
            raise RuntimeError('Command output did not close after group cleanup')
        return code
