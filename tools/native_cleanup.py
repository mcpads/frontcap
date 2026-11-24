"""Retire or inspect owned devices without touching unrelated resources."""
import errno
import json
from pathlib import Path
import socket
import subprocess
import sys
import time
import uuid


def require_retired_native_session(lock_directory):
    """Called under native-tests.lock; do not replace a Rust adapter's unretired target."""
    path = Path(lock_directory) / 'native-session.json'
    if not path.exists():
        return
    state = json.loads(path.read_text())
    if (type(state) is not dict or set(state) != {'directory', 'retired'}
            or type(state['retired']) is not bool or type(state['directory']) is not str
            or not Path(state['directory']).is_absolute()):
        raise RuntimeError('Invalid native session ownership record')
    if not state['retired']:
        raise RuntimeError(f"Previous native session is unretired; inspect {state['directory']}")


def inspect_cleanup(platform, directory, env=None, *, deadline=None):
    root = Path(directory)
    checks = []

    def record(name, passed, detail):
        checks.append(dict(name=name, passed=bool(passed), detail=detail))

    def remaining(limit):
        if deadline is None:
            return limit
        left = deadline - time.monotonic()
        if left <= 0:
            raise TimeoutError('Cleanup observation deadline reached')
        return min(limit, left)

    try:
        if platform == "ios":
            device = str(uuid.UUID((root / "simulator-id").read_text().strip())).upper()
            output = subprocess.run(["xcrun", "simctl", "list", "devices", "-j"],
                                    env=env, capture_output=True, text=True, timeout=5, check=True)
            inventory = json.loads(output.stdout)["devices"]
            matches = [item for devices in inventory.values() for item in devices
                       if item["udid"].upper() == device]
            record("simulator_deleted", not matches, dict(udid=device, remaining=matches))
        elif platform == "android":
            pid = int((root / "emulator-pid").read_text().strip())
            port = int((root / "emulator-port").read_text().strip())
            if pid <= 0 or not 1 <= port < 65535:
                raise ValueError("Invalid emulator identity")
            process = subprocess.run(["ps", "-p", str(pid), "-o", "pid=,stat=,command="],
                                     env=env, capture_output=True, text=True, timeout=remaining(5))
            record("emulator_exited", process.returncode == 1 and not process.stdout.strip() and not process.stderr.strip(),
                   dict(pid=pid, exit_code=process.returncode, stdout=process.stdout, stderr=process.stderr))
            for number in (port, port + 1):
                with socket.socket() as probe:
                    probe.settimeout(remaining(1))
                    result = probe.connect_ex(("127.0.0.1", number))
                # A timeout or other network error is not proof that the port was released.
                record("emulator_port_released", result == errno.ECONNREFUSED,
                       dict(port=number, connect_result=result))
        else:
            raise ValueError(f"Unknown native platform: {platform}")
    except InterruptedError:
        raise
    except (OSError, ValueError, KeyError, TypeError, AttributeError, subprocess.SubprocessError) as error:
        record("cleanup_observed", False, str(error))
    return dict(passed=bool(checks) and all(item["passed"] for item in checks), checks=checks)


def wait_for_android_cleanup(directory, env=None, *, timeout=7):
    """Observe retirement within the original process/port query budget, without signaling again."""
    started = time.monotonic()
    deadline = started + timeout
    observations = []
    while True:
        result = inspect_cleanup('android', directory, env, deadline=deadline)
        finished = time.monotonic()
        observations.append(dict(elapsed_s=finished-started, **result))
        if result['passed'] and finished <= deadline:
            state = 'retired'
        elif finished >= deadline:
            state = 'deadline'
        else:
            # Only known live resources warrant waiting. Failed inventories and
            # ambiguous connection errors remain failed observations.
            known = len(result['checks']) == 3
            for row in result['checks']:
                detail = row['detail']
                if row['name'] == 'emulator_exited':
                    known &= (not detail['stderr'] and
                              ((detail['exit_code'] == 0 and bool(detail['stdout'].strip())) or
                               (detail['exit_code'] == 1 and not detail['stdout'].strip())))
                elif row['name'] == 'emulator_port_released':
                    known &= detail['connect_result'] in (0, errno.ECONNREFUSED)
                else:
                    known = False
            state = 'waiting' if known else 'unavailable'
        if state != 'waiting':
            return dict(passed=state == 'retired', checks=result['checks'], state=state,
                        timeout_s=timeout, elapsed_s=finished-started, observations=observations)
        time.sleep(min(.05, max(0, deadline-time.monotonic())))


def retire_simulator(device, directory, *, progress=None):
    """Attempt both owned-device actions even if shutdown or diagnostic storage fails."""
    device = str(uuid.UUID(device)).upper()
    records = []
    for action in ('shutdown', 'delete'):
        record = dict(action=action, device=device, state='started')
        records.append(record)
        if progress:
            progress(records)
        try:
            result = subprocess.run(['xcrun', 'simctl', action, device],
                                    capture_output=True, text=True, timeout=20)
            record.update(state='finished', exit_code=result.returncode, stdout=result.stdout, stderr=result.stderr)
            try:
                (Path(directory)/f'{action}.log').write_text(result.stdout+result.stderr)
            except InterruptedError:
                raise
            except OSError as error:
                record['report_error'] = str(error)
        except InterruptedError:
            raise
        except (OSError, subprocess.SubprocessError) as error:
            record.update(state='failed', error=str(error))
        finally:
            if progress:
                progress(records)
        if 'error' in record or 'report_error' in record or record.get('exit_code'):
            print('Simulator cleanup: '+json.dumps(record), file=sys.stderr, flush=True)
    return records


def recover_simulator_cleanup(directory, env=None):
    """After the probe group retires, remove a confirmed survivor without erasing its failure."""
    report = inspect_cleanup('ios', directory, env)
    if report['passed']:
        return report
    survivors = [row['detail'] for row in report['checks']
                 if row['name'] == 'simulator_deleted' and not row['passed']
                 and row['detail'].get('remaining')]
    if len(survivors) != 1:
        return report  # Missing identity or an unavailable inventory does not authorize a target.
    recovery = dict(started=time.time(), state='observing', actions=[])
    report['recovery'] = recovery
    logs = Path(directory)/'runner-cleanup'
    try:
        logs.mkdir(exist_ok=True)
    except InterruptedError:
        raise
    except OSError as error:
        recovery['storage_error'] = str(error)
    def save(actions=None):
        if actions is not None:
            recovery['actions'] = actions
        temporary = Path(directory)/'runner-cleanup.tmp'
        try:
            temporary.write_text(json.dumps(report, indent=2)+'\n')
            temporary.replace(Path(directory)/'runner-cleanup.json')
        except InterruptedError:
            raise
        except OSError as error:
            recovery['report_error'] = str(error)
            print('Simulator recovery evidence: '+json.dumps(report), file=sys.stderr, flush=True)

    try:
        save()
        recovery['actions'] = retire_simulator(survivors[0]['udid'], logs, progress=save)
        recovery['observation'] = inspect_cleanup('ios', directory, env)
        recovery['state'] = 'finished'
        recovery['finished'] = time.time()
    except BaseException as error:
        # Preserve partial command effects even for the worker's BaseException cancellation.
        recovery.update(state='aborted', error_type=type(error).__name__, error=str(error))
        raise
    finally:
        save()
    return report
