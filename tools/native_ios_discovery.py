"""Resolve an installed app directory while retaining bounded simulator command evidence."""
import json
from pathlib import Path
import subprocess
import sys
import time
import uuid

from native_clock_control import atomic_json
from native_ui_diagnostics import capture_sample, host_context


def app_container(simulator, bundle, directory, timeout=5):
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    command = ['xcrun', 'simctl', 'get_app_container', simulator, bundle, 'data']
    row = dict(attempt=uuid.uuid4().hex, command=command, timeout_s=timeout,
               started_monotonic_ns=time.monotonic_ns())

    def record(phase):
        try:
            with (directory/'simulator-commands.jsonl').open('a') as stream:
                stream.write(json.dumps(dict(row, phase=phase, observed_monotonic_ns=time.monotonic_ns()))+'\n')
        except OSError as error:
            print(f'Cannot save simulator command evidence: {error}', file=sys.stderr)

    process = None
    record('started')
    try:
        process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        row['pid'] = process.pid
        record('spawned')
        stdout, stderr = process.communicate(timeout=timeout)
        row.update(stdout=stdout, stderr=stderr, exit_code=process.returncode)
        if process.returncode:
            raise subprocess.CalledProcessError(process.returncode, command, stdout, stderr)
        return Path(stdout.strip())
    except subprocess.TimeoutExpired as error:
        row.update(error=str(error), error_type=type(error).__name__)
        record('timed_out')
        # Preserve the failed deadline. Observe only this still-owned command before
        # reaping it; diagnostics never turn a late result into successful discovery.
        prefix = directory/('discovery-'+row['attempt'])
        diagnostic = dict(attempt=row['attempt'], command=command, pid=process.pid,
                          timeout_s=timeout, observed_monotonic_ns=time.monotonic_ns())
        try:
            diagnostic['sample'] = (capture_sample(process.pid, prefix, 3) if process.poll() is None
                                    else dict(skipped='Command exited before sampling'))
            diagnostic['host_context'] = host_context(directory)
            atomic_json(prefix.with_suffix('.json'), diagnostic)
            row['diagnostic'] = prefix.with_suffix('.json').name
        except Exception as evidence_error:
            row['diagnostic_error'] = f'{type(evidence_error).__name__}: {evidence_error}'
        raise
    except BaseException as error:
        row.update(error=str(error), error_type=type(error).__name__)
        raise
    finally:
        if process is not None:
            if process.poll() is None:
                process.kill()
            if 'stdout' not in row:
                stdout, stderr = process.communicate()
                row.update(stdout=stdout, stderr=stderr)
            row['exit_code'] = process.returncode
        row['finished_monotonic_ns'] = time.monotonic_ns()
        record('finished')
