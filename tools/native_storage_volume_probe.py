#!/usr/bin/env python3
"""Opt-in macOS storage boundary experiment; never acquires an app or existing volume.

Create a fresh 256 MiB image, exhaust it with two external writers, release an 8 MiB
retirement reserve, then reopen read-only in a fresh verifier and detach. Evidence lives
outside the image. This checks orderly persistence, not crash recovery.
This is not product quota enforcement. Interrupted runs must be inspected, not resumed.
"""
import argparse
import errno
import hashlib
import json
import os
from pathlib import Path
import plistlib
import subprocess
import sys
import time
import traceback

import native_owned_volume
from native_owned_volume import OwnedVolume

MIB = 1024 * 1024


def save(path, value):
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value, indent=2) + '\n')
    temporary.replace(path)


def digest(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def write_until_full(path, limit_mib=512):
    # Installed scratch volumes can be larger than this probe's 256 MiB image.
    # Keep every writer finite even if its caller supplies the wrong filesystem.
    if type(limit_mib) is not int or not 1 <= limit_mib <= 65536:
        raise ValueError('writer limit must be 1..65536 MiB')
    block = os.urandom(MIB)
    written = 0
    try:
        with path.open('xb', buffering=0) as stream:
            for _ in range(limit_mib):
                offset = 0
                while offset < len(block):
                    count = stream.write(block[offset:])
                    if not count:
                        raise RuntimeError('writer made no progress')
                    offset += count
                    written += count
                os.fsync(stream.fileno())
    except OSError as error:
        if error.errno not in (errno.ENOSPC, errno.EDQUOT):
            raise
        result = dict(state='capacity_rejected', errno=error.errno,
                      written=written, retained=path.stat().st_size)
        print(json.dumps(result), flush=True)
        return result
    raise RuntimeError('writer exceeded the expected volume bound')


def verify_reopened(output):
    mount = output / 'volume-owner/mount'
    expected = json.loads((output / 'recovery-input.json').read_text())
    stats = os.statvfs(mount)
    assert stats.f_flag & os.ST_RDONLY, 'recovery must not mutate the image'
    assert stats.f_blocks * stats.f_frsize == expected['filesystem_bytes']
    for name, original in expected['files'].items():
        assert Path(name).name == name
        assert (mount / name).stat().st_size == original['bytes']
        assert digest(mount / name) == original['sha256']
    assert json.loads((mount / 'retirement.json').read_text()) == dict(state='recorded_after_exhaustion')
    print(json.dumps(dict(state='passed', pid=os.getpid(), readonly=True,
                          files_verified=len(expected['files']))))


def probe(output):
    if sys.platform != 'darwin':
        raise RuntimeError('this experiment requires macOS hdiutil')
    output.mkdir(parents=True, exist_ok=False)
    output = output.resolve()
    image = output / 'volume-owner/bounded.dmg'
    mount = output / 'volume-owner/mount'
    state = dict(state='running', pid=os.getpid(), started=time.time(),
                 source_sha256=digest(Path(__file__)),
                 owner_source_sha256=digest(Path(native_owned_volume.__file__)), image=str(image), mount=str(mount))
    save(output / 'state.json', state)
    owner = None
    children = []
    command_number = 0

    def command(arguments):
        nonlocal command_number
        command_number += 1
        prefix = output / f'command-{command_number:02}'
        save(prefix.with_suffix('.json'), dict(argv=arguments))
        with prefix.with_suffix('.stdout').open('xb') as stdout, prefix.with_suffix('.stderr').open('xb') as stderr:
            result = subprocess.run(arguments, stdout=stdout, stderr=stderr, timeout=90)
        save(prefix.with_suffix('.result.json'), dict(exit_code=result.returncode))
        if result.returncode:
            raise RuntimeError(f'command {command_number} failed; inspect retained output')
        return prefix.with_suffix('.stdout').read_bytes()

    try:
        owner = OwnedVolume.create(output / 'volume-owner', 256, command)
        owner.attach()
        stats = os.statvfs(mount)
        state['filesystem_bytes'] = stats.f_blocks * stats.f_frsize
        assert 0 < state['filesystem_bytes'] <= 256 * MIB
        reserve = owner.reserve_retirement(8)
        state.update(device=owner.attached["device"], reserve_bytes=reserve['bytes'],
                     reserve_allocated_bytes=reserve['allocated_bytes'])
        save(output / 'state.json', state)
        for index in range(2):
            path = mount / f'writer-{index}.bin'
            child = subprocess.Popen([sys.executable, str(Path(__file__).resolve()),
                                      '--writer', str(path)], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            children.append(child)
            save(output / f'writer-{index}-owner.json', dict(pid=child.pid, path=str(path)))
        writers = []
        for index, child in enumerate(children):
            stdout, stderr = child.communicate(timeout=60)
            (output / f'writer-{index}.stdout').write_bytes(stdout)
            (output / f'writer-{index}.stderr').write_bytes(stderr)
            assert child.returncode == 0, f'writer {index} failed'
            result = json.loads(stdout)
            assert result['state'] == 'capacity_rejected'
            writers.append(result)
        originals = {path.name: dict(bytes=path.stat().st_size, sha256=digest(path))
                     for path in mount.glob('writer-*.bin')}
        assert len(originals) == 2 and sum(v['bytes'] for v in originals.values()) > 0
        save(output / 'originals.json', originals)
        state['writers'] = writers
        state['reserve_release'] = owner.release_retirement(
            writers_retired=all(child.poll() is not None for child in children))
        with (mount / 'retirement-proof.bin').open('xb', buffering=0) as stream:
            block = os.urandom(MIB)
            assert stream.write(block) == len(block)
            os.fsync(stream.fileno())
        save(mount / 'retirement.json', dict(state='recorded_after_exhaustion'))
        with (mount / 'retirement.json').open('rb') as stream:
            os.fsync(stream.fileno())
        for name, original in originals.items():
            assert (mount / name).stat().st_size == original['bytes']
            assert digest(mount / name) == original['sha256']
        retained = dict(originals)
        for name in ['retirement-proof.bin', 'retirement.json']:
            retained[name] = dict(bytes=(mount / name).stat().st_size, sha256=digest(mount / name))
        save(output / 'recovery-input.json', dict(filesystem_bytes=state['filesystem_bytes'], files=retained))
        owner.detach(writers_retired=all(child.poll() is not None for child in children))
        assert mount.stat().st_dev == output.stat().st_dev
        state['sealed_image_sha256'] = digest(image)
        save(output / 'state.json', state)
        owner.attach(readonly=True)
        recovery = json.loads(command([sys.executable, str(Path(__file__).resolve()), '--verify', str(output)]))
        assert recovery['state'] == 'passed' and recovery['pid'] != os.getpid()
        state['recovery'] = recovery
        state.update(state='passed', original_files_unchanged=True,
                     retirement_record_written=True, children_reaped=True)
    except BaseException:
        state.update(state='failed', error=traceback.format_exc())
    finally:
        for child in children:
            if child.poll() is None:
                child.kill()
                child.wait(timeout=10)
        try:
            if owner and owner.attached is not None:
                owner.detach(writers_retired=all(child.poll() is not None for child in children))
                inventory = plistlib.loads(command(['hdiutil', 'info', '-plist']))
                assert not any(Path(entry.get('image-path', '')).resolve() == image
                               for entry in inventory.get('images', []))
                assert mount.stat().st_dev == output.stat().st_dev
                state['detached'] = True
                if 'sealed_image_sha256' in state:
                    assert digest(image) == state['sealed_image_sha256']
                    state['recovery_image_unchanged'] = True
                if state['state'] == 'passed':
                    state['image_bytes_removed'] = image.stat().st_size
                    owner.remove()
                else:
                    state['failed_image_retained'] = True
            else:
                state['cleanup_requires_inspection'] = True
        except BaseException:
            state.update(state='failed', cleanup_error=traceback.format_exc())
        state['finished'] = time.time()
        save(output / 'state.json', state)
    print(json.dumps(state))
    return 0 if state['state'] == 'passed' and state.get('detached') else 1


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    choice = parser.add_mutually_exclusive_group(required=True)
    choice.add_argument('--output', type=Path)
    choice.add_argument('--writer', type=Path, help=argparse.SUPPRESS)
    choice.add_argument('--verify', type=Path, help=argparse.SUPPRESS)
    parser.add_argument('--writer-limit-mib', type=int, default=512, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.writer:
        write_until_full(args.writer, args.writer_limit_mib)
    elif args.verify:
        verify_reopened(args.verify)
    else:
        raise SystemExit(probe(args.output))
