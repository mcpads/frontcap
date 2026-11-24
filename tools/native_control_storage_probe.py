#!/usr/bin/env python3
"""Opt-in macOS check of control writes while owned data writers remain full and live.

Uses two fresh fixed-size images. Never fills the host filesystem or adopts old volumes.
This qualifies a storage boundary, not recorder routing or a whole-run quota.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time
import traceback

from native_clock_control import atomic_json
from native_command_capture import capture
from native_owned_volume import OwnedVolume
from native_storage_volume_probe import write_until_full


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def probe(output):
    if sys.platform != 'darwin':
        raise RuntimeError('this experiment requires macOS hdiutil')
    output.mkdir(parents=True, exist_ok=False)
    output = output.resolve()
    owners, children = [], []
    state = dict(state='running', pid=os.getpid(), started=time.time())
    atomic_json(output/'state.json', state)
    number = 0

    def command(arguments):
        nonlocal number
        number += 1
        directory = output/f'command-{number:03}'
        result = capture(arguments, directory, timeout=90, output_limit=1024*1024)
        if result['error'] is not None or not result.get('child_retired'):
            raise RuntimeError(f'volume command unresolved: {directory}')
        return (directory/'stdout').read_bytes()

    try:
        for role in ('data', 'control'):
            owner = OwnedVolume.create(output/role, 256, command)
            owners.append(owner)
            owner.attach()
        data, control = owners
        assert data.mount.stat().st_dev != control.mount.stat().st_dev
        data.reserve_retirement(8)
        original = data.mount/'original.bin'
        original.write_bytes(os.urandom(65536))
        expected_original = digest(original)
        for index in range(2):
            log = output/f'writer-{index}.jsonl'
            with log.open('xb') as stream:
                child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()),
                    '--writer', str(data.mount/f'pressure-{index}')], stdin=subprocess.PIPE,
                    stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
            children.append(child)
            atomic_json(output/f'writer-{index}-process.json', dict(pid=child.pid))
        deadline = time.monotonic()+120
        for index, child in enumerate(children):
            log = output/f'writer-{index}.jsonl'
            while not log.read_bytes().endswith(b'\n'):
                if child.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError('data writer exited or did not reach capacity')
                time.sleep(.05)
            reply = json.loads(log.read_text())
            assert reply['state'] == 'capacity_rejected', reply
            assert child.poll() is None
        assert data.reserve['state'] == 'reserved'
        # No space is released on the data filesystem to make these writes succeed.
        evidence = control.mount/'control-evidence.bin'
        with evidence.open('xb') as stream:
            stream.write(os.urandom(1024*1024)); stream.flush(); os.fsync(stream.fileno())
        atomic_json(control.mount/'stop-intent.json', dict(operation='stop', writers=[p.pid for p in children]))
        assert all(child.poll() is None for child in children)
        atomic_json(output/'live-control-write.json', dict(writers_live=True, data_reserve='reserved',
            data_device=data.mount.stat().st_dev, control_device=control.mount.stat().st_dev,
            control_evidence_bytes=evidence.stat().st_size))
        for child in children:
            child.stdin.write(b'retire\n'); child.stdin.flush(); child.stdin.close()
        for child in children:
            assert child.wait(timeout=10) == 0
        assert digest(original) == expected_original
        atomic_json(control.mount/'stop-result.json', dict(passed=True, writers_retired=True))
        expected = {p.name: digest(p) for p in control.mount.iterdir() if p.is_file()}
        atomic_json(output/'control-files.json', expected)
        retained = output/'retained-control'; retained.mkdir()
        for name in expected:
            shutil.copyfile(control.mount/name, retained/name)
        control.detach(writers_retired=True)
        control.attach(readonly=True)
        assert os.statvfs(control.mount).f_flag & os.ST_RDONLY
        assert all(digest(control.mount/name) == value for name, value in expected.items())
        state.update(state='passed', control_files_verified=len(expected),
                     original_data_sha256=expected_original, writer_exits=[p.returncode for p in children])
    except BaseException:
        state.update(state='failed', error=traceback.format_exc())
    finally:
        for child in children:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill(); child.wait(timeout=5)
            if child.stdin and not child.stdin.closed:
                child.stdin.close()
        for owner in reversed(owners):
            try:
                if owner.attached is not None:
                    if owner.reserve and owner.reserve['state'] == 'reserved':
                        owner.release_retirement(writers_retired=all(p.poll() is not None for p in children))
                    owner.detach(writers_retired=all(p.poll() is not None for p in children))
                if state['state'] == 'passed':
                    owner.remove()
            except BaseException:
                state.update(state='failed', cleanup_error=traceback.format_exc())
        state.update(finished=time.time(), images_removed=[owner.removed for owner in owners])
        atomic_json(output/'state.json', state)
    return 0 if state['state'] == 'passed' else 1


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    choice = parser.add_mutually_exclusive_group(required=True)
    choice.add_argument('--output', type=Path)
    choice.add_argument('--writer', type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.writer:
        write_until_full(args.writer)
        if sys.stdin.buffer.readline(16) != b'retire\n':
            raise RuntimeError('writer authority ended without explicit retirement')
    else:
        raise SystemExit(probe(args.output))
