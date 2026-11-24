"""Read the external byte streams of a retained process exchange."""
import gzip
import hashlib
import io
import json
import os
import stat


def read_exchange(path, raw):
    """Resolve only fixed sibling diagnostic files; legacy arrays remain accepted."""
    exchange = json.loads(raw)
    for name in ('stdout', 'stderr'):
        record = exchange.get(name)
        if not isinstance(record, dict):
            continue
        compressed = record.get('gzip')
        expected_keys = {'file', 'bytes', 'sha256'} | ({'gzip'} if compressed is not None else set())
        if compressed is not None and (not isinstance(compressed, dict)
                or set(compressed) != {'bytes', 'sha256'}):
            raise ValueError('Invalid compressed diagnostic descriptor')
        stored = compressed if compressed is not None else record
        filename = name + ('.bin.gz' if compressed is not None else '.bin')
        if (set(record) != expected_keys or record['file'] != filename
                or type(record['bytes']) is not int or not 0 <= record['bytes'] <= 16 * 1024 * 1024
                or type(stored['bytes']) is not int or not 0 <= stored['bytes'] <= 16 * 1024 * 1024
                or path.parent.is_symlink()):
            raise ValueError('Diagnostic stream binding differs')
        fd = os.open(path.parent / record['file'], os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as stream:
            metadata = os.fstat(stream.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_size != stored['bytes']:
                raise ValueError('Diagnostic stream type or size changed')
            data = stream.read(stored['bytes'] + 1)
        if len(data) != stored['bytes'] or hashlib.sha256(data).hexdigest() != stored['sha256']:
            raise ValueError('Stored diagnostic stream digest changed')
        if compressed is not None:
            with gzip.GzipFile(fileobj=io.BytesIO(data)) as stream:
                data = stream.read(record['bytes'] + 1)
        if len(data) != record['bytes'] or hashlib.sha256(data).hexdigest() != record['sha256']:
            raise ValueError('Diagnostic stream digest changed')
        exchange[name] = data
    return exchange
