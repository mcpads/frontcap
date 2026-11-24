"""Sample an owned simulator app after a failed advance; never changes test outcomes."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time

from native_process_resources import process_resources
from native_process_threads import process_threads
from native_process_files import process_files
from native_sampler_inspection import (SamplerInspection, blocked_symbol_threads,
                                       sampled_threads)


def host_context(directory):
    """Best-effort context after failure sampling, never an acceptance condition."""
    report = dict(started=time.time(), logical_cpus=os.cpu_count())
    try:
        report['load_1_5_15'] = os.getloadavg()
    except InterruptedError:
        raise
    except OSError as error:
        report['load_error'] = str(error)
    report['storage'] = dict(path=str(directory.absolute()))
    try:
        usage = shutil.disk_usage(directory)
        report['storage'].update(total_bytes=usage.total, used_bytes=usage.used,
                                 free_bytes=usage.free)
    except InterruptedError:
        raise
    except OSError as error:
        report['storage']['error'] = str(error)
    try:
        result = subprocess.run(['/usr/bin/pmset', '-g', 'therm'], capture_output=True,
                                text=True, timeout=2)
        report['thermal'] = dict(exit_code=result.returncode, stdout=result.stdout[:8192],
                                 stderr=result.stderr[:8192])
    except InterruptedError:
        raise
    except (OSError, UnicodeError, subprocess.SubprocessError) as error:
        report['thermal'] = dict(error=str(error))
    report['finished'] = time.time()
    return report


def failed_advances(path):
    if not path.exists():
        return []
    rows = []
    for index, line in enumerate(path.read_text().splitlines(keepends=True)):
        if not line.endswith("\n"):  # The app may still be writing the last record.
            continue
        row = json.loads(line)
        if row["result"]:
            rows.append((index, row))
    return rows


def owned_app(command, simulator, executable_name='NativeProbe'):
    executable = command.strip().split(" ", 1)[0]
    return (f"/CoreSimulator/Devices/{simulator}/data/Containers/Bundle/Application/" in executable
            and executable.endswith(f"/{executable_name}.app/{executable_name}"))


def capture_sample(pid, prefix, timeout):
    """Record sampler progress without extending its command deadline."""
    report = dict(sample_started_monotonic_ns=time.monotonic_ns(), sample_events=[], sampler_resources=[], sample_log_reads=[])
    command = ['/usr/bin/sample', str(pid), '1', '10', '-file', str(prefix.with_suffix('.sample.txt'))]
    report['sample_command'] = command
    deadline = time.monotonic() + timeout
    report['sample_deadline_monotonic_ns'] = int(deadline * 1e9)
    process = None
    resource_identity = None
    resource_identity_changed = False
    next_resource_at = 0
    resource_interval = .25
    symbols_started = None
    file_probe = os.environ.get('FC_SAMPLER_FILES')
    inspection = SamplerInspection(prefix, deadline)

    def target_state(budget=1):
        observed = dict(started_monotonic_ns=time.monotonic_ns())
        try:
            if budget <= 0:
                raise OSError('Sampler deadline exhausted before target query')
            result = subprocess.run(['ps', '-p', str(pid), '-o', 'command='],
                                    capture_output=True, text=True, timeout=budget)
            observed.update(exit_code=result.returncode, command=result.stdout.strip(), stderr=result.stderr)
        except InterruptedError:
            raise
        except (OSError, UnicodeError, subprocess.SubprocessError) as error:
            observed['error'] = str(error)
        observed['finished_monotonic_ns'] = time.monotonic_ns()
        return observed

    def record(event):
        report['sample_events'].append(event)
        try:
            with prefix.with_suffix('.events.jsonl').open('a') as trace:
                trace.write(json.dumps(event) + '\n')
        except InterruptedError:
            raise
        except OSError as error:
            report['observation_error'] = str(error)

    def observe_resources(reason):
        nonlocal resource_identity, resource_identity_changed, next_resource_at
        # Never query a reaped PID or spend a new command budget on diagnostics.
        if (process.returncode is not None or resource_identity_changed
                or (inspection.report is not None and 'primary_exit_observed_monotonic_ns' in inspection.report)
                or time.monotonic() >= deadline):
            return
        observation = process_resources(process.pid)
        observation['reason'] = reason
        if observation['status'] == 'available':
            if resource_identity is None:
                resource_identity = observation['identity']
            elif observation['identity'] != resource_identity:
                observation.update(status='identity_mismatch', expected_identity=resource_identity)
                resource_identity_changed = True
            if not resource_identity_changed:
                observation_deadline = min(deadline, time.monotonic()+.02)
                observation['thread_snapshot'] = process_threads(process.pid, resource_identity, deadline=observation_deadline)
                if file_probe and symbols_started is not None and blocked_symbol_threads(observation):
                    observation['file_snapshot'] = process_files(file_probe,process.pid,resource_identity,
                                                                 deadline=observation_deadline)
        report['sampler_resources'].append(observation)
        try:
            with prefix.with_suffix('.resources.jsonl').open('a') as trace:
                trace.write(json.dumps(observation) + '\n')
        except InterruptedError:
            raise
        except OSError as error:
            report['resource_observation_error'] = str(error)
        next_resource_at = time.monotonic() + resource_interval

    try:
        # Direct output survives interruption even if progress observation fails.
        with prefix.with_suffix('.log').open('wb') as log:
            process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
            report['sampler_pid'] = process.pid
            record(dict(phase='started', sampler_pid=process.pid, command=command,
                        host_monotonic_ns=time.monotonic_ns(), host_time=time.time()))
            observe_resources('started')
            pending = b''
            offset = 0

            def observe():
                nonlocal pending, offset, resource_interval, symbols_started
                try:
                    reading = dict(offset=offset, started_monotonic_ns=time.monotonic_ns())
                    with prefix.with_suffix('.log').open('rb') as source:
                        source.seek(offset)
                        chunk = source.read(65536)
                        offset = source.tell()
                    reading['finished_monotonic_ns'] = time.monotonic_ns()
                    if chunk:
                        reading.update(length=len(chunk), sha256=hashlib.sha256(chunk).hexdigest())
                        previous = report.get('sample_log_last_empty_read')
                        if previous is not None and previous['offset'] == reading['offset']:
                            reading['previous_empty_read'] = dict(previous)
                        report['sample_log_reads'].append(reading)
                        try:
                            with prefix.with_suffix('.log-reads.jsonl').open('a') as trace:
                                trace.write(json.dumps(reading) + '\n')
                        except InterruptedError:
                            raise
                        except OSError as error:
                            report['log_read_observation_error'] = str(error)
                    else:
                        report['sample_log_last_empty_read'] = reading
                    pending += chunk
                    lines = pending.split(b'\n')
                    pending = lines.pop()
                    for line in lines:
                        text = line.decode(errors='replace')
                        event = dict(host_monotonic_ns=time.monotonic_ns(), host_time=time.time(), line=text)
                        if text.startswith('Sampling process '):
                            event['phase'] = 'sampling'
                        elif text == 'Sampling completed, processing symbols...':
                            event['phase'] = 'processing_symbols'
                        elif text.startswith('Sample analysis of process ') and ' written to file ' in text:
                            event['phase'] = 'output_written'
                        # Persist the marker before any extra process query.
                        record(event)
                        if event.get('phase') == 'processing_symbols':
                            symbols_started = time.monotonic()
                            # Short symbol phases need reads at the existing poll cadence.
                            resource_interval = .05
                            observe_resources('processing_symbols')
                            report['target_at_processing'] = target_state(min(1, deadline - time.monotonic()))
                        elif event.get('phase') == 'output_written':
                            resource_interval = .25
                            symbols_started = None
                except InterruptedError:
                    raise
                except OSError as error:
                    report['observation_error'] = str(error)

            while True:
                inspection.update()
                remaining = deadline - time.monotonic()
                try:
                    if inspection.active:
                        # An inspector may still be processing collected stacks after
                        # primary exit. Reserve that PID until inspection completion
                        # or its original limit.
                        inspection.primary_exited(process)
                        if inspection.active:
                            time.sleep(max(0, min(.05, remaining)))
                            raise subprocess.TimeoutExpired(command, timeout)
                    report['exit_code'] = process.wait(timeout=max(0, min(.05, remaining)))
                    observe()
                    break
                except subprocess.TimeoutExpired:
                    if remaining <= 0:
                        raise subprocess.TimeoutExpired(command, timeout) from None
                    observe()
                    if time.monotonic() >= next_resource_at:
                        observe_resources('periodic')
                    if (inspection.report is None and not resource_identity_changed
                            and symbols_started is not None and time.monotonic() - symbols_started >= 1
                            and report['sampler_resources']):
                        latest = report['sampler_resources'][-1]
                        blocked = blocked_symbol_threads(latest)
                        if blocked:
                            inspection.start(process, resource_identity, dict(
                                resource_index=len(report['sampler_resources']) - 1,
                                thread_ids=blocked, phase='processing_symbols',
                                observed_monotonic_ns=latest['finished_monotonic_ns']))
        report['sampled_threads'] = sampled_threads(prefix.with_suffix('.sample.txt'))
        if report['exit_code'] or not report['sampled_threads']:
            report['error'] = 'Sampler failed or returned no thread stacks'
    except InterruptedError:
        raise
    except (OSError, UnicodeError, subprocess.SubprocessError) as error:
        report['error'] = str(error)
    finally:
        try:
            inspection.close('primary_finished_or_interrupted')
        finally:
            if process is not None and process.poll() is None:
                # Persist the boundary before a potentially delayed kernel exit.
                # Diagnostic publication must never bypass owned-child retirement.
                try:
                    record(dict(phase='retirement_requested', sampler_pid=process.pid,
                                host_monotonic_ns=time.monotonic_ns()))
                finally:
                    process.kill()
                    try:
                        record(dict(phase='retirement_signalled', sampler_pid=process.pid,
                                    host_monotonic_ns=time.monotonic_ns()))
                    finally:
                        process.wait()
                        record(dict(phase='retirement_reaped', sampler_pid=process.pid,
                                    host_monotonic_ns=time.monotonic_ns(), exit_code=process.returncode))
        if inspection.report is not None:
            report['sampler_inspection'] = inspection.report
        report['sample_finished_monotonic_ns'] = time.monotonic_ns()
        if process is not None:
            report.setdefault('exit_code', process.returncode)
    report['target_after_sample'] = target_state()
    return report


def watch(simulator, directory):
    stopped = False

    def stop(signum, frame):
        nonlocal stopped
        stopped = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    seen = set()
    container = None
    while not stopped:
        try:
            if container is None:
                result = subprocess.run(["xcrun", "simctl", "get_app_container", simulator,
                                         "dev.frontcap.s18.probe", "data"],
                                        capture_output=True, text=True, timeout=5)
                if result.returncode == 0:
                    container = Path(result.stdout.strip())
            if container is not None:
                for index, row in failed_advances(container / "Documents/clock-advances.jsonl"):
                    identity = (row["pid"], index)
                    if identity in seen:
                        continue
                    seen.add(identity)
                    report = dict(advance=row, index=index, host_started=time.time())
                    name = directory / f"advance-failure-{row['pid']}-{index}"
                    try:
                        process = subprocess.run(["ps", "-p", str(row["pid"]), "-o", "command="],
                                                 capture_output=True, text=True, timeout=5)
                        report["command"] = process.stdout.strip()
                        if process.returncode or not owned_app(process.stdout, simulator):
                            report["error"] = "App exited or process identity does not match owned simulator"
                        else:
                            report.update(capture_sample(row['pid'], name, 10))
                    except (OSError, UnicodeError, subprocess.SubprocessError) as error:
                        report["error"] = str(error)
                    finally:
                        report['host_context'] = host_context(directory)
                        report["host_finished"] = time.time()
                        try:
                            temporary = name.with_suffix('.tmp')
                            temporary.write_text(json.dumps(report, indent=2) + '\n')
                            temporary.replace(name.with_suffix('.json'))
                        except InterruptedError:
                            raise
                        except OSError as error:
                            report['report_error'] = str(error)
                            # run.sh retains stderr in ui-diagnostics.log even if JSON publication fails.
                            print('Cannot save UI failure report; retaining its outcome in the diagnostic log',
                                  file=sys.stderr, flush=True)
                            print(json.dumps(report), file=sys.stderr, flush=True)
        except (OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
            print(f"UI diagnostics: {error}", flush=True)
        time.sleep(.1 if container else .5)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("simulator")
    parser.add_argument("directory", type=Path)
    args = parser.parse_args()
    watch(args.simulator, args.directory)
