"""Spend an existing Rust launch allowance; never create, replenish or adopt one."""
import contextvars
import errno
import fcntl
import json
import os
from pathlib import Path
import stat
import tempfile

_ENVIRONMENT = 'FRONTCAP_NATIVE_WRITE_BUDGET'
_CLASS = contextvars.ContextVar('native_write_class', default='ordinary')
_MAX = (1 << 64) - 1


def begin_retirement():
    """The calling worker thread has permanently stopped admitting ordinary work."""
    _CLASS.set('retirement')


def _object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate write budget field')
        result[key] = value
    return result


def _number(value):
    if type(value) is not int or not 0 <= value <= _MAX:
        raise ValueError('invalid write allowance integer')
    return value


def _limits(value, positive=False):
    if not isinstance(value, dict) or set(value) != {'ordinary', 'retirement'}:
        raise ValueError('invalid write allowance fields')
    for amount in value.values():
        if _number(amount) < int(positive):
            raise ValueError('write allowances must be positive')


def _identity(path, directory):
    metadata = path.lstat()
    valid = stat.S_ISDIR(metadata.st_mode) if directory else stat.S_ISREG(metadata.st_mode)
    if not valid:
        raise ValueError('write budget path has wrong type')
    return [metadata.st_dev, metadata.st_ino]


def charge_payload(size, kind=None, *, binding=None):
    """Durably charge before writing payload. Failures and overwritten files never refund it."""
    value = os.environ.get(_ENVIRONMENT) if binding is None else binding
    if value is None:
        return
    try:
        size = _number(size)
        kind = kind or _CLASS.get()
        if kind not in ('ordinary', 'retirement') or len(value.encode()) > 4096:
            raise ValueError('invalid write budget binding or class')
        binding = json.loads(value, object_pairs_hook=_object)
        if not isinstance(binding, dict) or set(binding) != {
                'directory', 'owner', 'limits', 'directory_identity', 'lock_identity'}:
            raise ValueError('invalid write budget binding fields')
        directory = Path(binding['directory'])
        if not directory.is_absolute() or not isinstance(binding['owner'], str):
            raise ValueError('invalid write budget path or owner')
        _limits(binding['limits'], positive=True)
        for name in ('directory_identity', 'lock_identity'):
            identity = binding[name]
            if not isinstance(identity, list) or len(identity) != 2:
                raise ValueError('invalid write budget identity')
            for number in identity:
                _number(number)
        def check():
            if (_identity(directory, True) != binding['directory_identity']
                    or _identity(directory/'lock', False) != binding['lock_identity']):
                raise ValueError('write budget identity changed')
        check()
        fd = os.open(directory/'lock', os.O_RDWR | os.O_NOFOLLOW)
        with os.fdopen(fd, 'r+b') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            metadata = os.fstat(lock.fileno())
            if [metadata.st_dev, metadata.st_ino] != binding['lock_identity']:
                raise ValueError('write budget lock changed')
            check()
            fd = os.open(directory/'ledger.json', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
            with os.fdopen(fd, 'rb') as source:
                metadata = os.fstat(source.fileno())
                if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > 4096:
                    raise ValueError('write budget ledger has wrong type or size')
                data = source.read(4097)
            if len(data) > 4096:
                raise ValueError('write budget ledger grew beyond limit')
            ledger = json.loads(data, object_pairs_hook=_object)
            if (not isinstance(ledger, dict) or set(ledger) != {'owner', 'limits', 'charged'}
                    or ledger['owner'] != binding['owner'] or ledger['limits'] != binding['limits']):
                raise ValueError('write budget owner or limits changed')
            _limits(ledger['limits'], positive=True)
            _limits(ledger['charged'])
            if any(ledger['charged'][key] > ledger['limits'][key] for key in ('ordinary', 'retirement')):
                raise ValueError('write budget charge exceeds limit')
            next_charge = ledger['charged'][kind] + size
            if next_charge > ledger['limits'][kind]:
                raise OSError(errno.ENOSPC, 'write allowance exhausted')
            if not size:
                return
            ledger['charged'][kind] = next_charge
            check()
            data = json.dumps(ledger, sort_keys=True, separators=(',', ':'), ensure_ascii=False, allow_nan=False).encode()
            fd, temporary = tempfile.mkstemp(prefix='.ledger.json.tmp-', dir=directory)
            try:
                with os.fdopen(fd, 'wb') as output:
                    output.write(data)
                    output.flush()
                    os.fsync(output.fileno())
                os.replace(temporary, directory/'ledger.json')
                descriptor = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
                try:
                    os.fsync(descriptor)
                finally:
                    os.close(descriptor)
            finally:
                try:
                    os.unlink(temporary)
                except FileNotFoundError:
                    pass
    except (ValueError, TypeError, KeyError, UnicodeError) as error:
        raise OSError(errno.EINVAL, f'invalid write budget: {error}') from error
