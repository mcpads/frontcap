#!/usr/bin/env python3
"""Compare preallocated overwrite and post-quiescence reserve release on a fresh APFS image.

No app, shared volume, or host filesystem is filled. Results are measurements, not a quota
or a guarantee against concurrent writers, snapshots, power loss, or owner loss.
"""
import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import sys
import time
import traceback

from native_clock_control import atomic_json
from native_command_capture import capture
from native_owned_volume import OwnedVolume
from native_worker_control import TerminalRecord


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def probe(output):
    if sys.platform != 'darwin':
        raise RuntimeError('requires macOS hdiutil')
    output.mkdir(parents=True, exist_ok=False)
    output = output.resolve()
    state = dict(state='running', pid=os.getpid(), started=time.time())
    atomic_json(output/'state.json', state)
    number = 0
    owner = None

    def command(arguments):
        nonlocal number
        number += 1
        directory = output/f'command-{number:03}'
        result = capture(arguments, directory, timeout=90, output_limit=1024*1024)
        if result['error'] is not None or not result.get('child_retired'):
            raise RuntimeError(f'volume command unresolved: {directory}')
        return (directory/'stdout').read_bytes()

    def attempt(name, operation):
        result = dict(operation=name)
        try:
            operation()
            result['succeeded'] = True
        except OSError as error:
            result.update(succeeded=False, errno=error.errno, error=str(error))
            if error.errno not in (errno.ENOSPC, errno.EDQUOT):
                raise
        atomic_json(output/(name+'.json'), result)
        return result

    try:
        owner = OwnedVolume.create(output/'volume', 256, command)
        owner.attach()
        owner.reserve_retirement(8)
        slot = owner.mount/'preallocated.bin'
        original = os.urandom(64*1024)
        with slot.open('xb') as stream:
            stream.write(original); stream.flush(); os.fsync(stream.fileno())
        assert slot.stat().st_blocks*512 >= len(original)
        atomic_json(output/'slot-before.json', dict(bytes=slot.stat().st_size,
                    allocated_bytes=slot.stat().st_blocks*512, sha256=digest(slot)))
        control = owner.mount/'terminal-before-release';control.mkdir()
        terminal = TerminalRecord(control)
        stages = []
        deadline = time.monotonic()+90
        # Progressively smaller writes distinguish a large-write rejection from remaining
        # tail capacity. Each stage has finite attempts and the whole filler a deadline.
        with (owner.mount/'pressure.bin').open('xb', buffering=0) as stream:
            for size in (1024*1024, 4096, 512, 1):
                row = dict(write_bytes=size, written=0)
                block = os.urandom(size)
                for _ in range(16384):
                    if time.monotonic() >= deadline:
                        raise TimeoutError('pressure write deadline')
                    try:
                        written = stream.write(block)
                        if not written:raise RuntimeError('pressure writer made no progress')
                        row['written'] += written
                        os.fsync(stream.fileno())
                    except OSError as error:
                        if error.errno not in (errno.ENOSPC, errno.EDQUOT):raise
                        row['errno'] = error.errno
                        break
                else:
                    raise RuntimeError('pressure stage did not reach capacity within bound')
                stages.append(row)
                atomic_json(output/'pressure.json', stages)
        # The only filler has closed: subsequent operations do not race another writer.
        stats = os.statvfs(owner.mount)
        atomic_json(output/'quiescence.json', dict(filler_closed=True, filler_bytes=(owner.mount/'pressure.bin').stat().st_size,
                    available_bytes=stats.f_bavail*stats.f_frsize, reserve=owner.reserve))
        replacement = os.urandom(len(original))
        def overwrite():
            with slot.open('r+b', buffering=0) as stream:
                if stream.write(replacement) != len(replacement):raise RuntimeError('partial slot overwrite')
                os.fsync(stream.fileno())
        rewritten = attempt('preallocated-overwrite', overwrite)
        atomic_json(output/'slot-after.json', dict(sha256=digest(slot),
                    original_unchanged=digest(slot)==hashlib.sha256(original).hexdigest(),
                    replacement_complete=digest(slot)==hashlib.sha256(replacement).hexdigest()))
        value = dict(state='stopped', retirement=dict(passed=True, errors=[], signal_error=None),
                     experiment='no device was acquired')
        before = attempt('terminal-before-release', lambda: terminal.publish(value))
        retained = {}
        for path in control.iterdir():
            if path.is_file():
                (output/('before-'+path.name)).write_bytes(path.read_bytes())
                retained[path.name] = dict(bytes=path.stat().st_size, sha256=digest(path))
        atomic_json(output/'before-release-files.json', retained)
        owner.release_retirement(writers_retired=True)
        after = owner.mount/'terminal-after-release';after.mkdir()
        TerminalRecord(after).publish(value)
        expected = digest(after/'result.json')
        assert all(digest(control/name)==info['sha256'] for name,info in retained.items())
        (output/'terminal-result.json').write_bytes((after/'result.json').read_bytes())
        owner.detach(writers_retired=True)
        owner.attach(readonly=True)
        assert digest(after/'result.json') == expected
        assert all(digest(control/name)==info['sha256'] for name,info in retained.items())
        owner.detach(writers_retired=True)
        owner.remove()
        state.update(state='passed', preallocated_overwrite=rewritten['succeeded'],
                     terminal_before_release=before['succeeded'], terminal_after_release=True,
                     readonly_result_sha256=expected, image_removed=owner.removed)
    except BaseException:
        state.update(state='failed', error=traceback.format_exc())
        # Failures retain the exact image/attachment for explicit ownership inspection.
        raise
    finally:
        state['finished'] = time.time()
        atomic_json(output/'state.json', state)


if __name__ == '__main__':
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    args=parser.parse_args()
    if not args.output.is_absolute():parser.error('output must be absolute')
    probe(args.output)
