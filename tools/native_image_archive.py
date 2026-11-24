"""Lossless, bounded-memory encoding of an already detached image; never deletes its source."""
import gzip
import hashlib
import os
from pathlib import Path
import stat
import zlib


def stamp(value):
    return (value.st_dev, value.st_ino, value.st_size, value.st_mtime_ns, value.st_ctime_ns)


def compress(image, expected):
    image = Path(image)
    archive = image.with_name(image.name + '.gz')
    partial = archive.with_name(archive.name + '.partial')
    try:
        archive.lstat()
    except FileNotFoundError:
        pass
    else:
        raise FileExistsError(archive)
    descriptor = os.open(image, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, 'rb') as source:
        before = os.fstat(source.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or before.st_size != expected['bytes']:
            raise RuntimeError('original image type or size differs')
        original = hashlib.sha256()
        # Exclusive creation preserves any earlier attempt; never resume or overwrite it.
        with partial.open('xb') as output:
            with gzip.GzipFile(filename='', mode='wb', fileobj=output, compresslevel=1, mtime=0) as encoded:
                remaining = expected['bytes']
                while remaining:
                    block = source.read(min(65536, remaining))
                    if not block:
                        raise RuntimeError('original image shortened during compression')
                    original.update(block)
                    encoded.write(block)
                    remaining -= len(block)
            output.flush()
            os.fsync(output.fileno())
            archived = os.fstat(output.fileno())
        if source.read(1) or original.hexdigest() != expected['sha256'] or stamp(before) != stamp(os.fstat(source.fileno())):
            raise RuntimeError('original image changed during compression')
        # Verify the complete decoded stream before publishing an archive that may replace raw storage.
        descriptor = os.open(partial, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(descriptor, 'rb') as stored:
            if stamp(os.fstat(stored.fileno())) != stamp(archived):
                raise RuntimeError('partial archive changed')
            decoded_hash = hashlib.sha256()
            with gzip.GzipFile(fileobj=stored, mode='rb') as decoded:
                remaining = expected['bytes']
                while remaining:
                    block = decoded.read(min(65536, remaining))
                    if not block:
                        raise RuntimeError('archive decoded image shortened')
                    decoded_hash.update(block)
                    remaining -= len(block)
                if decoded.read(1) or decoded_hash.hexdigest() != expected['sha256']:
                    raise RuntimeError('archive decoded image differs')
            stored.seek(0)
            stored_hash = hashlib.sha256()
            while block := stored.read(65536):
                stored_hash.update(block)
            digest = stored_hash.hexdigest()
            if stamp(os.fstat(stored.fileno())) != stamp(archived):
                raise RuntimeError('archive changed during verification')
        if stamp(before) != stamp(os.fstat(source.fileno())) or stamp(before) != stamp(image.lstat()):
            raise RuntimeError('original image changed before archive publication')
        if stamp(partial.lstat()) != stamp(archived):
            raise RuntimeError('partial archive replaced before publication')
        os.link(partial, archive, follow_symlinks=False)
        partial.unlink()
        sync_directory(image.parent)
        return dict(bytes=archived.st_size, sha256=digest)


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def restore(source, destination, expected):
    """Publish one fully verified raw copy; preserve source and failed partial evidence."""
    source, destination = Path(source), Path(destination)
    capacity = expected['bytes']
    if type(capacity) is not int or not 0 < capacity <= 64 * 1024 ** 3:
        raise ValueError('invalid restored image capacity')
    stored = expected.get('gzip', expected)
    if type(stored['bytes']) is not int or not 0 < stored['bytes'] <= capacity + capacity // 100 + 1024 * 1024:
        raise ValueError('invalid stored image size')
    partial = destination.with_name(destination.name + '.partial')
    if destination.exists() or destination.is_symlink():
        raise FileExistsError(destination)
    descriptor = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(descriptor, 'rb') as encoded:
        before = os.fstat(encoded.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or before.st_size != stored['bytes']:
            raise RuntimeError('stored image type or size differs')
        digest = hashlib.sha256()
        remaining = stored['bytes']
        while remaining:
            block = encoded.read(min(65536, remaining))
            if not block:
                raise RuntimeError('stored image shortened')
            digest.update(block)
            remaining -= len(block)
        if encoded.read(1):
            raise RuntimeError('stored image grew')
        if digest.hexdigest() != stored['sha256']:
            raise RuntimeError('stored image digest differs')
        encoded.seek(0)
        if 'gzip' in expected:
            if encoded.read(10)[:4] != b'\x1f\x8b\x08\x00':
                raise RuntimeError('unexpected compressed image header')
            encoded.seek(0)
            blocks = decoded_blocks(encoded)
        else:
            blocks = iter(lambda: encoded.read(65536), b'')
        original = hashlib.sha256()
        with partial.open('xb') as output:
            remaining = capacity
            for block in blocks:
                if len(block) > remaining:
                    raise RuntimeError('decoded image exceeds capacity')
                output.write(block)
                original.update(block)
                remaining -= len(block)
            if remaining or original.hexdigest() != expected['sha256']:
                raise RuntimeError('decoded image differs')
            output.flush()
            os.fsync(output.fileno())
            copied = os.fstat(output.fileno())
        if stamp(before) != stamp(os.fstat(encoded.fileno())) or stamp(before) != stamp(source.lstat()):
            raise RuntimeError('stored image changed during restoration')
        if stamp(copied) != stamp(partial.lstat()):
            raise RuntimeError('restored image changed before publication')
        os.link(partial, destination, follow_symlinks=False)
        partial.unlink()
        sync_directory(destination.parent)


def decoded_blocks(encoded):
    """Decode exactly one gzip member with bounded input and output buffers."""
    decoder = zlib.decompressobj(16 + zlib.MAX_WBITS)
    while block := encoded.read(65536):
        if decoder.eof:
            raise RuntimeError('compressed image has trailing bytes')
        while block:
            decoded = decoder.decompress(block, 65536)
            if decoder.unused_data:
                raise RuntimeError('compressed image has trailing bytes')
            block = decoder.unconsumed_tail
            if decoded:
                yield decoded
    if not decoder.eof:
        raise RuntimeError('compressed image is incomplete')
