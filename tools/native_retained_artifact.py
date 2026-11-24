#!/usr/bin/env python3
"""Explicit offline artifact export through a new, read-only copy of a retained image.

The caller supplies image and artifact identities from original receipts. No source image is
attached or changed; failure evidence remains in the exclusive output directory.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import stat
import sys
import re

from native_clock_control import atomic_json
from native_command_capture import capture
from native_image_archive import stamp
from native_owned_volume import OwnedVolume


def read_artifact(root, artifact):
    relative = artifact['path']
    parts = PurePosixPath(relative).parts
    if (not parts or relative != '/'.join(parts) or relative.startswith('/')
            or any(part in ('.', '..') for part in parts)):
        raise ValueError('artifact path must be a normalized relative path')
    size = artifact['bytes']
    if type(size) is not int or not 0 <= size <= 16 * 1024 * 1024:
        raise ValueError('artifact exceeds inspection bound')
    descriptor = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for part in parts[:-1]:
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        file = os.open(parts[-1], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=descriptor)
        with os.fdopen(file, 'rb') as stream:
            before = os.fstat(stream.fileno())
            if not stat.S_ISREG(before.st_mode) or before.st_size != size:
                raise RuntimeError('artifact type or size differs')
            data = stream.read(size + 1)
            if stamp(before) != stamp(os.fstat(stream.fileno())):
                raise RuntimeError('artifact changed during inspection')
        if len(data) != size or hashlib.sha256(data).hexdigest() != artifact['sha256']:
            raise RuntimeError('artifact differs from original receipt')
        return data
    finally:
        os.close(descriptor)


def read_uikit_artifact(mount, session, artifact):
    if len(session) > 128 or not re.fullmatch(r'[a-z0-9]+(?:-[a-z0-9]+)*', session):
        raise ValueError('invalid UIKit session identity')
    prefixes = [f'{session}/target/ui-client/']
    descriptor = os.open(mount, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        for part in (session, 'target'):
            child = os.open(part, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        try:
            epochs = os.open('epochs', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW, dir_fd=descriptor)
        except FileNotFoundError:
            epochs = None
        if epochs is not None:
            try:
                with os.scandir(epochs) as entries:
                    for index, entry in enumerate(entries):
                        if index >= 256:
                            raise RuntimeError('UIKit epoch inspection bound exceeded')
                        if not re.fullmatch(r'[0-9a-f]{8}-(?:[0-9a-f]{4}-){3}[0-9a-f]{12}', entry.name):
                            raise RuntimeError('invalid UIKit epoch name')
                        prefixes.append(f'{session}/target/epochs/{entry.name}/ui-client/')
            finally:
                os.close(epochs)
    finally:
        os.close(descriptor)
    found = None
    for prefix in prefixes:
        try:
            data = read_artifact(mount, dict(artifact, path=prefix + artifact['path']))
        except FileNotFoundError:
            continue
        if found is not None:
            raise RuntimeError('artifact occurs in multiple UIKit epochs')
        found = data
    if found is None:
        raise FileNotFoundError('original UIKit artifact absent from retained image')
    return found


def export(source, expected, artifact, directory, *, uikit_session=None):
    directory = Path(directory)
    directory.mkdir(mode=0o700, parents=False, exist_ok=False)
    atomic_json(directory / 'request.json', dict(image=str(source), image_identity=expected, artifact=artifact, uikit_session=uikit_session))
    commands = directory / 'commands'
    commands.mkdir()
    sequence = 0

    def command(arguments):
        nonlocal sequence
        sequence += 1
        output = commands / str(sequence)
        report = capture(arguments, output, timeout=30, output_limit=1024 * 1024)
        if report['error'] or not report.get('child_retired') or report['exit_code'] != 0:
            raise RuntimeError(f'image inspection command failed; inspect {output}')
        return (output / 'stdout').read_bytes()

    owner = None
    result = dict(passed=False, copy_removed=False, artifact_exported=False)
    try:
        owner = OwnedVolume.restore(directory / 'copy', source, expected, command)
        if owner.retained_image() != {key: expected[key] for key in ('bytes', 'sha256')}:
            raise RuntimeError('restored copy differs before attachment')
        mount = owner.attach(readonly=True)
        try:
            data = (read_uikit_artifact(mount, uikit_session, artifact) if uikit_session is not None
                    else read_artifact(mount, artifact))
            with (directory / 'artifact').open('xb') as stream:
                stream.write(data)
                stream.flush()
                os.fsync(stream.fileno())
            result['artifact_exported'] = True
        finally:
            # All local read descriptors have closed. Only this live owner may detach its copy.
            owner.detach(writers_retired=True)
        owner.remove()
        result.update(passed=True, copy_removed=True)
        return result
    except BaseException as error:
        result['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        atomic_json(directory / 'result.json', result)


def main():
    if sys.argv[1:] == ['--bridge']:
        raw = sys.stdin.buffer.read(65537)
        if len(raw) > 65536:
            raise ValueError('artifact request exceeds bound')
        request = json.loads(raw)
        result = export(request['image'], request['image_identity'], request['artifact'],
                        request['directory'], uikit_session=request['uikit_session'])
        print(json.dumps(result))
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', type=Path, required=True)
    parser.add_argument('--image-proof', type=Path, required=True)
    parser.add_argument('--artifact-proof', type=Path, required=True)
    parser.add_argument('--output-dir', type=Path, required=True)
    args = parser.parse_args()
    export(args.image, json.loads(args.image_proof.read_text()),
           json.loads(args.artifact_proof.read_text()), args.output_dir)
    print(json.dumps(dict(result=str(args.output_dir / 'result.json'), artifact=str(args.output_dir / 'artifact'))))


if __name__ == '__main__':
    main()
