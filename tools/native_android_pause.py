"""Bound and record the existing whole-guest pause experiment; guest clocks need not freeze."""
import argparse
import json
import math
import os
from pathlib import Path
import subprocess
import time


def pause_guest(adb, directory):
    directory = Path(directory)
    report = dict(passed=False, hold_seconds=5, commands=[])
    path = directory / 'guest-pause.json'

    def save():
        temporary = path.with_suffix('.tmp')
        temporary.write_text(json.dumps(report, indent=2) + '\n')
        temporary.replace(path)

    def command(name, args, *, diagnostic_stderr=False):
        record = dict(name=name, command=[*adb, *args], started=time.time(),
                      started_monotonic_ns=time.monotonic_ns())
        report['commands'].append(record)
        # Execute before writing output so evidence-storage failure cannot prevent
        # the compensating resume command from being attempted.
        cancelled = None
        try:
            options = {}
            if diagnostic_stderr:
                # These fixed read-only queries need client/service progress when
                # no guest shell marker arrives. Do not enable tracing for pause/resume or
                # inherit an ambient all/packet trace into this observation.
                record['adb_trace'] = 'adb,shell'
                options['env'] = {**os.environ, 'ADB_TRACE': record['adb_trace']}
            result = subprocess.run(record['command'], capture_output=True, timeout=5, **options)
            record.update(exit_code=result.returncode)
            stdout, stderr = result.stdout, result.stderr
        except (OSError, subprocess.SubprocessError) as error:
            record['error'] = str(error)
            if isinstance(error, InterruptedError):
                cancelled = error
            stdout, stderr = ((error.stdout or b'', error.stderr or b'')
                              if isinstance(error, subprocess.TimeoutExpired) else (b'', b''))
        record.update(finished=time.time(), finished_monotonic_ns=time.monotonic_ns())
        record['stdout'] = stdout.decode(errors='replace')
        record['stderr'] = stderr.decode(errors='replace')
        text = (stdout + stderr).decode(errors='replace')
        record['output'] = text
        save()
        if cancelled is not None:
            raise cancelled
        if record.get('exit_code') != 0:
            raise RuntimeError(f'{name} failed; inspect guest-pause.json')
        return record['stdout'] if diagnostic_stderr else text

    def state(name, expected):
        observed = command(name, ['emu', 'avd', 'status'])
        if f'virtual device is {expected}' not in observed.lower().splitlines():
            raise RuntimeError(f'{name}: expected {expected}; inspect guest-pause.json')

    def clocks(name):
        # Preserve raw sequential readings and independent host request bounds.
        # Shell builtins expose progress without launching another observer. Keep
        # stderr separate: concatenated streams do not preserve inter-stream order.
        script = ('set -e; printf "FRONTCAP_CLOCK shell-entered pid=%s\\n" "$$" >&2; '
                  'cut -d" " -f1 /proc/uptime; '
                  'printf "FRONTCAP_CLOCK uptime-read\\n" >&2; date +%s.%N; '
                  'printf "FRONTCAP_CLOCK realtime-read\\n" >&2')
        observed = command(name, ['shell', script], diagnostic_stderr=True)
        try:
            values = [float(value) for value in observed.split()]
            if len(values) == 2 and all(math.isfinite(value) and value >= 0 for value in values):
                return
        except ValueError:
            pass
        raise RuntimeError(f'{name}: expected uptime and realtime; inspect guest-pause.json')

    resume_needed = False
    cancellation = None
    try:
        save()
        state('state-before', 'running')
        clocks('clock-before')
        resume_needed = True  # A failed reply does not prove pause had no effect.
        command('pause', ['emu', 'avd', 'pause'])
        state('state-paused', 'stopped')
        report['hold_started_monotonic_ns'] = time.monotonic_ns()
        save()
        time.sleep(report['hold_seconds'])
        report['hold_finished_monotonic_ns'] = time.monotonic_ns()
        command('resume', ['emu', 'avd', 'resume'])
        state('state-after', 'running')
        resume_needed = False
        clocks('clock-after')
        report['passed'] = True
    except (InterruptedError, KeyboardInterrupt, SystemExit) as error:
        report['error'] = f'{type(error).__name__}: {error}'
        cancellation = error
    except (OSError, RuntimeError) as error:
        report['error'] = str(error)
    finally:
        if resume_needed:
            try:
                command('resume-recovery', ['emu', 'avd', 'resume'])
                state('state-recovery', 'running')
            except (InterruptedError, KeyboardInterrupt, SystemExit) as error:
                report['recovery_error'] = f'{type(error).__name__}: {error}'
                cancellation = cancellation or error
            except (OSError, RuntimeError) as error:
                report['recovery_error'] = str(error)
        report['finished'] = time.time()
        save()
    if cancellation is not None:
        raise cancellation
    return report


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    parser.add_argument('sdk', type=Path)
    parser.add_argument('port')
    args = parser.parse_args()
    report = pause_guest([str(args.sdk / 'platform-tools/adb'), '-s', 'emulator-' + args.port],
                         args.directory)
    print(json.dumps(report, indent=2), flush=True)
    raise SystemExit(0 if report['passed'] else 1)
