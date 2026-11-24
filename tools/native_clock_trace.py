"""Timestamp native stdout independently of forwarding and diagnostic storage."""
import codecs
import io
import json
import locale
import os
from pathlib import Path
import queue
import selectors
import subprocess
import sys
import threading
import time


MAX_PENDING_LINES = 256
MAX_LINE_CHARS = 65536


def trace(command, output, *, events=None):
    pending = queue.Queue(maxsize=MAX_PENDING_LINES)
    ready, launch, cancelled, done = (threading.Event() for _ in range(4))
    process_lock = threading.Lock()
    state = dict(process=None, error=None, cleanup_error=None, reader_thread_id=None, received_lines=0)
    previous_record = None
    forwarded_lines = 0

    def record(**fields):
        nonlocal previous_record
        if events is None:
            return
        before = time.monotonic_ns()
        process = state['process']
        try:
            with Path(events).open('a') as stream:
                stream.write(json.dumps(dict(pid=os.getpid(), thread_id=threading.get_native_id(),
                    reader_thread_id=state['reader_thread_id'], child_pid=process.pid if process else None,
                    previous_record=previous_record, **fields))+'\n')
        except OSError as error:
            print(f'Cannot save clock reader evidence: {error}', file=sys.stderr)
        finally:
            previous_record = dict(started_monotonic_ns=before, finished_monotonic_ns=time.monotonic_ns())

    def receive():
        process = None
        try:
            with selectors.DefaultSelector() as selector:
                decoder = io.IncrementalNewlineDecoder(codecs.getincrementaldecoder(locale.getencoding())(), True)
                state['reader_thread_id'] = threading.get_native_id()
                ready.set()
                launch.wait()
                with process_lock:
                    if cancelled.is_set():
                        return
                    process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0)
                    state['process'] = process
                os.set_blocking(process.stdout.fileno(), False)
                selector.register(process.stdout, selectors.EVENT_READ)
                buffer = ''

                def emit(line):
                    if len(line) > MAX_LINE_CHARS:
                        raise RuntimeError('Clock output line limit exceeded')
                    now = time.monotonic_ns()
                    row = dict(sequence=state['received_lines'], line=line, received_monotonic_ns=now,
                               reader_cpu_ns=time.thread_time_ns() if events is not None else None)
                    state['received_lines'] += 1
                    try:
                        pending.put_nowait(row)
                    except queue.Full:
                        raise RuntimeError('Clock output buffer limit exceeded; timestamps are incomplete') from None

                while not cancelled.is_set():
                    if not selector.select(.05):
                        continue
                    chunk = os.read(process.stdout.fileno(), 65536)
                    buffer += decoder.decode(chunk, final=not chunk)
                    while '\n' in buffer:
                        line, buffer = buffer.split('\n', 1)
                        emit(line+'\n')
                    if len(buffer) > MAX_LINE_CHARS:
                        raise RuntimeError('Clock output line limit exceeded')
                    if not chunk:
                        if buffer:
                            emit(buffer)
                        break
                # EOF need not mean process exit. Keep the original command lifetime,
                # while allowing a forwarding failure to cancel even an inherited pipe.
                while process.poll() is None and not cancelled.is_set():
                    cancelled.wait(.05)
        except BaseException as error:
            state['error'] = error
        finally:
            try:
                if process is not None:
                    try:
                        if process.poll() is None:
                            process.kill()
                        process.wait(timeout=2)
                    finally:
                        process.stdout.close()
            except BaseException as error:
                state['cleanup_error'] = error
                if state['error'] is None:
                    state['error'] = error
            finally:
                state['reader_finished_monotonic_ns'] = time.monotonic_ns()
                ready.set()
                done.set()

    reader = threading.Thread(target=receive, name='native-clock-reader')
    reader.start()
    try:
        if not ready.wait(2):
            raise RuntimeError('Clock reader did not initialize')
        if state['error']:
            raise state['error']
        # Evidence initialization completes before this initialized reader launches its producer.
        record(state='ready', command=command, host_monotonic_ns=time.monotonic_ns(), host_time_ns=time.time_ns())
        launch.set()
        start = None
        while True:
            try:
                row = pending.get(timeout=.05)
            except queue.Empty:
                if done.is_set() and pending.empty():
                    break
                continue
            if start is None:
                start = row['received_monotonic_ns']
            error = None
            before = time.monotonic_ns()
            cpu = time.thread_time_ns() if events is not None else None
            try:
                print(f"{(row['received_monotonic_ns']-start)/1e9:+8.3f} s  {row['line']}", end='', flush=True, file=output)
                forwarded_lines += 1
            except BaseException as failure:
                error = f'{type(failure).__name__}: {failure}'
                raise
            finally:
                record(state='line', **row, forwarding_started_monotonic_ns=before,
                       forwarded_monotonic_ns=time.monotonic_ns(), forwarder_cpu_before_ns=cpu,
                       forwarder_cpu_after_ns=time.thread_time_ns() if events is not None else None,
                       output_error=error)
        if state['error']:
            raise state['error']
        return state['process'].returncode
    finally:
        cancelled.set()
        launch.set()
        with process_lock:
            process = state['process']
            if process is not None and process.poll() is None:
                process.kill()
        reader.join(timeout=2)
        record(state='finished', exit_code=process.returncode if process else None,
               host_monotonic_ns=time.monotonic_ns(), reader_alive=reader.is_alive(),
               reader_finished_monotonic_ns=state.get('reader_finished_monotonic_ns'),
               received_lines=state['received_lines'], forwarded_lines=forwarded_lines,
               cleanup_error=f"{type(state['cleanup_error']).__name__}: {state['cleanup_error']}" if state['cleanup_error'] else None,
               reader_error=f"{type(state['error']).__name__}: {state['error']}" if state['error'] else None)
        if reader.is_alive():
            raise RuntimeError('Clock reader did not retire after cancellation')


if __name__ == '__main__':
    if len(sys.argv) < 2:
        raise SystemExit('Usage: native_clock_trace.py command [argument ...]')
    code = trace(sys.argv[1:], sys.stdout, events=os.environ.get('FC_CLOCK_TRACE_EVENTS'))
    raise SystemExit(code if code >= 0 else 128-code)
