"""Bound command output while retaining partial evidence and its child exit.

A failed capture does not establish whether a mutating command applied its effect. Callers
must preserve that uncertainty; child retirement is not operation rollback or descendant proof.
"""
import hashlib
import json
import math
import os
from pathlib import Path
import selectors
import subprocess
import time

from native_clock_control import atomic_json


def capture(command, directory, *, cancelled=lambda: False,
            timeout=2, output_limit=4 * 1024 * 1024, on_complete=None, env=None, evidence=None):
    if type(output_limit) is not int or output_limit < 0 or not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("Invalid command output bound or timeout")
    directory = Path(directory)
    if evidence is not None:
        evidence.charge(0)  # A fenced caller cannot start another SDK reader.
        evidence.path(directory/'request.json')
    directory.mkdir()
    if evidence is not None:
        evidence.save(directory/'request.json', dict(command=command, timeout_s=timeout, output_limit_bytes=output_limit))
    report = {'command': command,
              'started_monotonic_ns': time.monotonic_ns(), 'timeout_s': timeout,
              'output_limit_bytes': output_limit, 'exit_code': None, 'error': None}
    child = None
    interrupted = None
    buffers = {'stdout': bytearray(), 'stderr': bytearray()}
    deadline = time.monotonic() + timeout
    try:
        if cancelled():
            raise InterruptedError('Command capture cancelled before launch')
        child = subprocess.Popen(command, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
        report['pid'] = child.pid
        if evidence is None:
            atomic_json(directory / 'process.json', {'pid': child.pid, 'command': command})
        else:
            evidence.save(directory/'process.json', dict(pid=child.pid))
        with selectors.DefaultSelector() as selector:
            for name in buffers:
                stream = getattr(child, name)
                os.set_blocking(stream.fileno(), False)
                selector.register(stream, selectors.EVENT_READ, name)
            while selector.get_map():
                if cancelled():
                    raise InterruptedError('Command capture cancelled')
                left = deadline - time.monotonic()
                if left <= 0:
                    raise TimeoutError('Command capture exceeded its command budget')
                for key, _ in selector.select(min(.05, left)):
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    remaining = output_limit - sum(len(b) for b in buffers.values())
                    buffers[key.data].extend(chunk[:remaining])
                    if len(chunk) > remaining:
                        raise ValueError('Command capture output limit exceeded')
            report['exit_code'] = child.wait(timeout=max(.001, deadline - time.monotonic()))
            if report['exit_code'] != 0:
                raise RuntimeError('Command capture command failed')
    except BaseException as error:
        report['error'] = f'{type(error).__name__}: {error}'
        if not isinstance(error, Exception):
            interrupted = error
    finally:
        if child is not None:
            try:
                if child.poll() is None:
                    child.kill()
                report['exit_code'] = child.wait(timeout=5)
                report['child_retired'] = True
            except Exception as error:
                report['cleanup_error'] = f'{type(error).__name__}: {error}'
                report['child_retired'] = False
            finally:
                child.stdout.close()
                child.stderr.close()
        report['finished_monotonic_ns'] = time.monotonic_ns()
        report['outputs'] = {}
        for name, data in buffers.items():
            if evidence is None:
                with (directory / name).open('xb') as stream:
                    stream.write(data)
                    stream.flush()
                    os.fsync(stream.fileno())
                report['outputs'][name] = {'bytes': len(data), 'sha256': hashlib.sha256(data).hexdigest()}
            else:
                row = dict(bytes=len(data), sha256=hashlib.sha256(data).hexdigest(), retained=False)
                report['outputs'][name] = row
                try:
                    evidence.write(directory/name, data)
                    row['retained'] = True
                except Exception as error:
                    report.setdefault('evidence_error', f'{type(error).__name__}: {error}')
                    if report['error'] is None:
                        report['error'] = report['evidence_error']
        if evidence is None:
            atomic_json(directory / 'capture.json', report)
        else:
            # Only bounded control/omission evidence may use reserve, after child retirement.
            control = {key: report[key] for key in ('pid', 'exit_code', 'child_retired',
                'started_monotonic_ns', 'finished_monotonic_ns', 'outputs') if key in report}
            control.update(request='request.json', command_sha256=hashlib.sha256(json.dumps(command).encode()).hexdigest())
            for key in ('error', 'cleanup_error', 'evidence_error'):
                if report.get(key) is not None:control[key] = report[key][:256]
            try:
                evidence.save(directory/'capture.json', control,
                    kind='retirement' if report['error'] is not None else 'ordinary')
            except Exception as error:
                report.setdefault('evidence_error', f'{type(error).__name__}: {error}')
                if report['error'] is None:
                    report['error'] = report['evidence_error']
                    control.update(error=report['error'][:256], evidence_error=report['evidence_error'][:256])
                    try:evidence.save(directory/'capture.json', control, kind='retirement')
                    except Exception as final_error:report['control_error'] = f'{type(final_error).__name__}: {final_error}'
                else:report['control_error'] = f'{type(error).__name__}: {error}'
                if interrupted is not None:
                    interrupted.add_note('Capture control evidence unavailable: '+report.get('control_error', report['evidence_error']))
        if on_complete is not None and (evidence is None or 'evidence_error' not in report):
            on_complete(report)
    if child is not None and not report.get('child_retired'):
        if evidence is not None and interrupted is not None:
            interrupted.add_note('Captured command child retirement is unconfirmed')
            raise interrupted
        error = RuntimeError('Captured command child retirement is unconfirmed')
        if evidence is not None and report['error'] is not None:
            error = RuntimeError(report['error'])
            error.add_note('Captured command child retirement is unconfirmed')
        raise error
    if interrupted is not None:
        raise interrupted
    return report
