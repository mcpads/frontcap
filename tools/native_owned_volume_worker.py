#!/usr/bin/env python3
"""Private process bridge for a live data-volume owner; EOF never proves writer retirement."""
import argparse
import errno
import json
import os
from pathlib import Path
import sys
import time
import traceback
from native_control_reserve import reserve_control

from native_owned_volume import OwnedVolume
from native_command_capture import capture
import hashlib
import native_volume_uikit


class BusyDetach(RuntimeError):
    """An original hdiutil child exited with EBUSY and was reaped with bounded output."""


def save(path, value):
    temporary = path.with_suffix('.tmp')
    with temporary.open('w') as stream:
        json.dump(value, stream, indent=2)
        stream.write('\n')
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)


def run(directory, size_mib, reserve_mib, incoming, outgoing, *, owner_type=OwnedVolume):
    if not directory.is_absolute() or type(size_mib) is not int or not 256 <= size_mib <= 65536:
        raise ValueError('absolute fresh worker directory and 256..65536 MiB volume required')
    if type(reserve_mib) is not int or not 1 <= reserve_mib <= min(64, size_mib // 4):
        raise ValueError('invalid retirement reserve')
    # The supervisor must authorize before any filesystem or disk creation.
    if incoming.read(1) != b'1':
        raise RuntimeError('volume worker not authorized')
    directory.mkdir(mode=0o700, parents=False, exist_ok=False)
    directory = directory.resolve()
    state = dict(state='starting', pid=os.getpid(), started=time.time())
    owner = None
    control_reserve = None
    number = 0
    command_deadline = None

    def release_control():
        nonlocal control_reserve
        if control_reserve is not None:
            control_reserve.close()
            control_reserve = None

    def reject_watch(filename, line, error):
        report = dict(request_sha256=hashlib.sha256(line).hexdigest(), error=f'{type(error).__name__}: {error}')
        try:
            save(directory/filename, report)
        except OSError as publication_error:
            if publication_error.errno not in (errno.ENOSPC, errno.EDQUOT):
                raise
            release_control()
            save(directory/filename, report)

    def publish_watch(record, acknowledgement, rejection, binding, line):
        try:
            save(directory/record, binding)
            save(directory/acknowledgement, dict(request_sha256=hashlib.sha256(line).hexdigest()))
        except OSError as error:
            if error.errno not in (errno.ENOSPC, errno.EDQUOT):
                raise
            # The binding was already validated in this process. Preserve it for
            # cleanup, but refuse admission when its durable acknowledgement failed.
            release_control()
            reject_watch(rejection, line, error)

    def command(arguments):
        nonlocal number
        number += 1
        prefix = directory / f'command-{number:03}'
        save(prefix.with_suffix('.json'), dict(argv=arguments))
        # Capture runs inside this independent worker, so controller loss cannot close its
        # command-output reader. Overflow/timeout is unresolved disk work, never a retry.
        remaining = 90 if command_deadline is None else command_deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError('original volume detach deadline expired')
        result = capture(arguments, prefix, timeout=min(90, remaining), output_limit=1024 * 1024)
        if (len(arguments) == 3 and arguments[:2] == ['hdiutil', 'detach']
                and result.get('exit_code') == errno.EBUSY and result.get('child_retired') is True
                and result['error'] == 'RuntimeError: Command capture command failed'
                and not result.get('cleanup_error')):
            raise BusyDetach(f'volume command {number} returned EBUSY')
        if result['error'] is not None or not result.get('child_retired'):
            raise RuntimeError(f'volume command {number} unresolved; inspect {prefix / "capture.json"}')
        return (prefix / 'stdout').read_bytes()

    try:
        # Allocate before creating any target image. Unlike the in-image reserve, this
        # protects command/result publication on the worker's own filesystem.
        control_reserve = reserve_control(directory)
        save(directory / 'state.json', state)
        owner = owner_type.create(directory / 'volume', size_mib, command)
        mount = owner.attach()
        owner.reserve_retirement(reserve_mib)
        ready = dict(mount=str(mount), image=str(owner.image), size_mib=size_mib,
                     reserve_mib=reserve_mib, owner_pid=os.getpid())
        save(directory / 'ready.json', ready)
        state['state'] = 'ready'
        save(directory / 'state.json', state)
        outgoing.write((json.dumps(ready) + '\n').encode())
        outgoing.flush()
        line = incoming.readline(4097)
        binding = None
        acquisition_binding = None
        if line:
            if len(line) > 4096 or not line.endswith(b'\n'):
                raise ValueError('invalid bounded volume message')
            request = json.loads(line)
            if isinstance(request, dict) and request.get('operation') == 'watch_uikit_acquisition':
                try:
                    acquisition_binding = native_volume_uikit.register_acquisition(mount, request)
                except Exception as error:
                    reject_watch('watch-acquisition-rejected.json', line, error)
                else:
                    publish_watch('watch-acquisition.json', 'watch-acquisition-ready.json',
                                  'watch-acquisition-rejected.json', acquisition_binding, line)
                line = incoming.readline(4097)
        if line:
            if len(line) > 4096 or not line.endswith(b'\n'):
                raise ValueError('invalid bounded volume message')
            request = json.loads(line)
            if isinstance(request, dict) and request.get('operation') == 'watch_uikit':
                try:
                    candidate = native_volume_uikit.register(mount, request)
                    if acquisition_binding is not None and any(
                            candidate[key] != acquisition_binding[key]
                            for key in ['diagnostic', 'diagnostic_process', 'diagnostic_config_sha256']):
                        raise ValueError('UIKit watch differs from original acquisition diagnostics')
                    if acquisition_binding is not None and any(
                            request[key] != acquisition_binding['registration'][key] for key in ['session', 'worker']):
                        raise ValueError('UIKit watch differs from original acquisition worker')
                    if acquisition_binding is not None and (
                            request.get('retirement_reader') != acquisition_binding['registration'].get('retirement_reader')
                            or any(candidate.get(key) != acquisition_binding.get(key)
                                   for key in ['retirement_reader_sha256', 'launch_sha256'])):
                        raise ValueError('UIKit retirement reader differs from acquisition registration')
                    binding = candidate
                except Exception as error:
                    # Reject the watch without discarding the original live volume authority.
                    # The caller can still retire after stopping its session; EOF has no proof.
                    reject_watch('watch-rejected.json', line, error)
                else:
                    publish_watch('watch-uikit.json', 'watch-ready.json', 'watch-rejected.json', binding, line)
                line = incoming.readline(4097)
        # Releasing our private bytes conveys no target authority. In particular, EOF
        # still requires the original registered writer proof before any detach.
        release_control()
        if not line:
            if binding is None and acquisition_binding is None:
                state.update(state='abandoned', reason='authority pipe closed without writer retirement proof')
                return 1
            state['state'] = 'waiting_for_uikit_retirement'
            save(directory / 'state.json', state)
            if binding is not None:
                proof = native_volume_uikit.wait_for_retirement(binding, time.monotonic() + 60)
                save(directory / 'owner-loss-writers-retired.json', proof)
            else:
                proof = native_volume_uikit.wait_for_acquisition_retirement(acquisition_binding, time.monotonic() + 60)
                save(directory / 'owner-loss-acquisition-retired.json', proof)
            request = dict(operation='retire', writers_retired=True, remove=False, compression='gzip')
        else:
            if len(line) > 4096 or not line.endswith(b'\n'):
                raise ValueError('invalid bounded retirement message')
            request = json.loads(line)
        if (not isinstance(request, dict) or set(request) not in ({'operation', 'writers_retired', 'remove'},
                                        {'operation', 'writers_retired', 'remove', 'compression'})
                or request['operation'] != 'retire' or request['writers_retired'] is not True
                or type(request['remove']) is not bool
                or ('compression' in request and (request['compression'] != 'gzip' or request['remove']))):
            raise ValueError('retirement requires explicit writer quiescence and disposal choice')
        if line and binding is not None and binding.get('registration', {}).get('retirement_reader') is not None:
            # A group can finish before its separate diagnostic collectors. The original volume
            # owner checks those writers even when a live recorder requests explicit retirement.
            proof = native_volume_uikit.wait_for_retirement(binding, time.monotonic() + 60)
            save(directory / 'writers-retired.json', proof)
        save(directory / 'retirement-request.json', request)
        owner.release_retirement(writers_retired=True)
        command_deadline = time.monotonic() + 15
        attempts = []
        while True:
            try:
                # Each attempt rechecks the original image, mount identity and full device map.
                owner.detach(writers_retired=True)
                attempts.append(dict(detached=True, observed=time.time()))
                save(directory / 'detach-attempts.json', attempts)
                break
            except BusyDetach as error:
                attempts.append(dict(detached=False, error=str(error), observed=time.time()))
                save(directory / 'detach-attempts.json', attempts)
                remaining = command_deadline - time.monotonic()
                if remaining <= 0:
                    raise
                time.sleep(min(1, remaining))
                # Preserve the original reaped EBUSY when the retry window expires during
                # sleep. Starting another identity query would replace it with a timeout.
                if time.monotonic() >= command_deadline:
                    raise
        command_deadline = None
        if request['remove']:
            owner.remove()
        result = dict(passed=True, detached=True, removed=request['remove'])
        if not request['remove']:
            result['retained_image'] = (owner.compress_retained_image() if request.get('compression') == 'gzip'
                                        else owner.retained_image())
        save(directory / 'retirement.json', result)
        outgoing.write((json.dumps(result) + '\n').encode())
        outgoing.flush()
        state['state'] = 'retired'
        return 0
    except BaseException:
        state.update(state='failed', error=traceback.format_exc())
        return 1
    finally:
        # No destructor cleanup: an absent controller or failed command grants no detach authority.
        release_control()
        state.update(finished=time.time(), live_owner_acquired=owner is not None)
        save(directory / 'state.json', state)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    parser.add_argument('--size-mib', type=int, required=True)
    parser.add_argument('--reserve-mib', type=int, default=8)
    args = parser.parse_args()
    raise SystemExit(run(args.directory, args.size_mib, args.reserve_mib, sys.stdin.buffer, sys.stdout.buffer))
