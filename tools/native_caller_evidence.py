"""Durable admission for caller-owned MCP records, separate from recorder/SDK storage.

Only writes through this owner participate. The ledger and one bounded fence record are control
metadata, not payload. This does not bound arbitrary caller files or filesystem allocation.
"""
import contextlib
import contextvars
import errno
import fcntl
import json
import os
from pathlib import Path
import secrets
import stat
import tempfile

from native_write_budget import charge_payload
from native_control_reserve import reserve_control

_ACTIVE = contextvars.ContextVar('native_caller_evidence', default=None)


def active():
    return _ACTIVE.get()


def _identity(path):
    value = path.lstat()
    if not stat.S_ISDIR(value.st_mode):
        raise ValueError('Caller evidence root must be a real directory')
    return [value.st_dev, value.st_ino]


class CallerEvidence:
    @classmethod
    def create(cls, root, *, ordinary=256*1024*1024, retirement=16*1024*1024):
        if any(type(n) is not int or not 0 < n < 2**64 for n in (ordinary, retirement)):
            raise ValueError('Positive caller allowances required')
        root = Path(root).absolute()
        root_identity = _identity(root)
        root = root.resolve()
        directory = root/'.caller-write-budget'
        directory.mkdir(mode=0o700)
        with (directory/'lock').open('xb'):
            pass
        lock = (directory/'lock').stat()
        limits = dict(ordinary=ordinary, retirement=retirement)
        binding = dict(directory=str(directory), owner='g-'+secrets.token_hex(8), limits=limits,
                       directory_identity=_identity(directory), lock_identity=[lock.st_dev, lock.st_ino])
        ledger = dict(owner=binding['owner'], limits=limits, charged=dict(ordinary=0, retirement=0))
        manifest = dict(root=str(root), root_identity=root_identity, binding=binding,
                        scope='participating_caller_payload', whole_run_quota=False,
                        excluded=['recorder_and_target_storage', 'arbitrary_caller_writes',
                                  'SDK_diagnostics_outside_this_writer', 'filesystem_metadata'])
        for name, value in [('ledger.json', ledger), ('caller.json', manifest)]:
            with (directory/name).open('x') as stream:
                json.dump(value, stream, sort_keys=True, separators=(',', ':'))
                stream.flush()
                os.fsync(stream.fileno())
        return cls.open(root)

    @classmethod
    def open(cls, root, *, _allow_control=True):
        root = Path(root).absolute()
        root_identity = _identity(root)
        root = root.resolve()
        path = root/'.caller-write-budget/caller.json'
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as stream:
            if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                raise ValueError('Caller binding must be regular')
            raw = stream.read(4097)
        if len(raw) > 4096:
            raise ValueError('Caller binding exceeds limit')
        manifest = json.loads(raw)
        if manifest['root'] != str(root) or manifest['root_identity'] != root_identity:
            raise ValueError('Caller root identity changed')
        binding = manifest['binding']
        if binding['directory'] != str(root/'.caller-write-budget'):
            raise ValueError('Caller allowance escaped its root')
        owner = cls()
        owner.root, owner.root_identity = root, root_identity
        owner._fenced = False
        owner._control = None
        owner._control_reserve = None
        owner.binding = json.dumps(binding, sort_keys=True, separators=(',', ':'))
        charge_payload(0, 'retirement', binding=owner.binding)
        if 'fence_control' in manifest:
            if not _allow_control:
                raise ValueError('Nested caller fence controls are unsupported')
            saved = manifest['fence_control']
            control = cls.open(saved['root'], _allow_control=False)
            if control.binding != saved['binding'] or control.root.stat().st_dev == root.stat().st_dev:
                raise ValueError('Caller fence control identity changed')
            owner._control = control
        return owner

    def reserve_control(self):
        """Allocate physical headroom for this live owner, never reconstruct a saved handle."""
        self._check()
        if self._control_reserve is None:
            self._control_reserve = reserve_control(self.root)

    def release_control(self):
        reserve, self._control_reserve = self._control_reserve, None
        if reserve is not None:
            reserve.close()

    def bind_control(self, control):
        """Register a separate existing owner before dispatch; never replace its binding."""
        charge_payload(0, 'retirement', binding=self.binding)
        binding = json.loads(self.binding)
        fd = os.open(Path(binding['directory'])/'lock', os.O_RDWR | os.O_NOFOLLOW)
        with os.fdopen(fd, 'r+b') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            meta = os.fstat(lock.fileno())
            if [meta.st_dev, meta.st_ino] != binding['lock_identity']:
                raise ValueError('Caller binding lock changed')
            self._bind_control_locked(control)

    def _bind_control_locked(self, control):
        self._check()
        control._check()
        if control._control is not None or control.root.stat().st_dev == self.root.stat().st_dev:
            raise ValueError('Caller fence control requires an independent non-nested owner')
        charge_payload(0, 'retirement', binding=control.binding)
        if self._control is not None:
            if self._control.binding != control.binding:
                raise ValueError('Caller fence control cannot be replaced')
            # Keep the supplied live reserve reachable; the persisted binding is unchanged.
            self._control = control
            return
        path = self.root/'.caller-write-budget/caller.json'
        fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as stream:
            if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                raise ValueError('Caller binding must be regular')
            raw = stream.read(4097)
        if len(raw) > 4096:
            raise ValueError('Caller binding exceeds limit')
        manifest = json.loads(raw)
        if (manifest['root'] != str(self.root) or manifest['root_identity'] != self.root_identity
                or manifest['binding'] != json.loads(self.binding)):
            raise ValueError('Caller binding changed')
        saved = dict(root=str(control.root), binding=control.binding)
        if manifest.get('fence_control', saved) != saved:
            raise ValueError('Caller fence control cannot be replaced')
        manifest['fence_control'] = saved
        data = json.dumps(manifest, sort_keys=True, separators=(',', ':')).encode()
        if len(data) > 4096:
            raise ValueError('Caller binding exceeds limit')
        fd, temporary = tempfile.mkstemp(prefix='.caller-', dir=path.parent)
        try:
            with os.fdopen(fd, 'wb') as stream:
                stream.write(data); stream.flush(); os.fsync(stream.fileno())
            os.replace(temporary, path)
            directory = os.open(path.parent, os.O_RDONLY)
            try:
                os.fsync(directory)
            finally:
                os.close(directory)
        finally:
            if os.path.exists(temporary):
                os.unlink(temporary)
        self._control = control

    def _control_fence_path(self):
        owner = json.loads(self.binding)['owner']
        return self._control.root/f'caller-fence-{owner}.json'

    @classmethod
    def for_root(cls, root):
        root = Path(root)
        if os.path.lexists(root/'.caller-write-budget'):
            return cls.open(root)
        return cls.create(root)

    @contextlib.contextmanager
    def scope(self):
        token = _ACTIVE.set(self)
        try:
            yield self
        finally:
            _ACTIVE.reset(token)

    @property
    def fenced(self):
        if self._control is not None:
            self._control._check()
            charge_payload(0, 'retirement', binding=self._control.binding)
            if os.path.lexists(self._control_fence_path()):
                return True
        return self._fenced or os.path.lexists(self.root/'.caller-write-budget/fenced.json')

    def fence(self, reason):
        # Storage failure can prevent even the durable fence. Keep this live owner closed.
        self._fenced = True
        self.release_control()
        if self._control is not None:
            self._control.release_control()
        control_saved = False
        control_error = None
        if self._control is not None:
            try:
                self._control.save(self._control_fence_path(),
                    dict(ordinary_admission='closed', binding=self.binding, reason=str(reason)[:256]), kind='retirement')
                control_saved = True
            except Exception as error:
                control_error = error
        try:
            self._local_fence(reason)
        except Exception as error:
            if not control_saved:
                if control_error is not None:
                    error.add_note(f'Caller control fence unavailable: {control_error}')
                raise

    def _local_fence(self, reason):
        self._check()
        charge_payload(0, 'retirement', binding=self.binding)
        path = self.root/'.caller-write-budget/fenced.json'
        data = json.dumps(dict(ordinary_admission='closed', reason=str(reason)[:256])).encode()
        try:
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
        except FileExistsError:
            return
        with os.fdopen(fd, 'wb') as stream:
            stream.write(data)
            stream.flush()
            os.fsync(stream.fileno())

    def _check(self):
        if _identity(self.root) != self.root_identity:
            raise ValueError('Caller root identity changed')

    def charge(self, size, kind='ordinary'):
        self._check()
        if kind == 'ordinary' and self.fenced:
            raise OSError(errno.ENOSPC, 'Caller ordinary admission is fenced; preserve original requests')
        try:
            charge_payload(size, kind, binding=self.binding)
        except OSError as error:
            if kind == 'ordinary':
                self._fence_failure(error)
            raise

    def path(self, path):
        self._check()
        path = Path(path).absolute()
        parent = path.parent.resolve()
        if not parent.is_relative_to(self.root) or path.is_symlink():
            raise ValueError('Caller output escaped its root')
        relative = (parent/path.name).relative_to(self.root)
        if any(part in ('runtimes', '.caller-write-budget') for part in relative.parts):
            raise ValueError('Caller writer cannot replace runtime or allowance records')
        return parent/path.name

    def _fence_failure(self, error):
        try:
            self.fence(error)
        except Exception as failure:
            error.add_note(f'Caller durable fence unavailable: {failure}')

    def write(self, path, data, *, kind='ordinary', append=False):
        path = self.path(path)
        self.charge(len(data), kind)
        try:
            if append:
                fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND | os.O_NOFOLLOW | os.O_NONBLOCK, 0o600)
                with os.fdopen(fd, 'wb') as stream:
                    if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                        raise ValueError('Caller append target must be regular')
                    stream.write(data)
                    stream.flush()
                    os.fsync(stream.fileno())
            else:
                fd, temporary = tempfile.mkstemp(prefix='.'+path.name+'-', dir=path.parent)
                try:
                    with os.fdopen(fd, 'wb') as stream:
                        stream.write(data)
                        stream.flush()
                        os.fsync(stream.fileno())
                    os.replace(temporary, path)
                finally:
                    if os.path.exists(temporary):
                        os.unlink(temporary)
        except OSError as error:
            if kind == 'ordinary':
                self._fence_failure(error)
            raise

    def save(self, path, value, *, kind='ordinary'):
        self.write(path, (json.dumps(value, indent=2)+'\n').encode(), kind=kind)
