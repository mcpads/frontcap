"""Live ownership of one fresh macOS image; persisted records never grant detach authority.

The caller records command output and proves its writers have retired before detach.
There is no automatic destructor cleanup, stored-device adoption, or force-detach fallback.
"""
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import stat


def identity(path):
    value = path.lstat()
    return dict(device=value.st_dev, inode=value.st_ino, mode=stat.S_IFMT(value.st_mode))


def allocated_bytes(path):
    return path.stat().st_blocks * 512


class OwnedVolume:
    @classmethod
    def _prepare(cls, directory, size_mib, command):
        if type(size_mib) is not int or not 256 <= size_mib <= 64 * 1024:
            raise ValueError('volume size must be 256..65536 MiB')
        directory = Path(directory)
        directory.mkdir(mode=0o700, parents=False, exist_ok=False)
        owner = cls()
        owner.directory = directory.resolve()
        owner.directory_identity = identity(owner.directory)
        owner.image = owner.directory / 'bounded.dmg'
        owner.mount = owner.directory / 'mount'
        owner.mount.mkdir(mode=0o700)
        owner.mount_identity = identity(owner.mount)
        owner.command = command
        owner.image_identity = None
        owner.image_allocation = None
        owner.attached = None
        owner.removed = False
        owner.archive = None
        owner.reserve = None
        owner.size_mib = size_mib
        return owner

    @classmethod
    def create(cls, directory, size_mib, command):
        owner = cls._prepare(directory, size_mib, command)
        owner._record('creating')
        try:
            command(['hdiutil', 'create', '-size', f'{size_mib}m', '-fs', 'APFS',
                     '-type', 'UDIF', '-nospotlight', '-volname', 'frontcap-owned-storage', str(owner.image)])
            owner.image_identity = identity(owner.image)
            owner._verify_image()
            owner._record('created')
        except BaseException:
            owner._record('creation_unresolved')
            raise
        return owner

    @classmethod
    def restore(cls, directory, source, expected, command):
        """Create new live ownership of a verified copy, never adopt the retained source."""
        from native_image_archive import restore
        capacity = expected['bytes']
        if type(capacity) is not int or capacity % (1024 * 1024):
            raise ValueError('restored image capacity must be whole MiB')
        owner = cls._prepare(directory, capacity // (1024 * 1024), command)
        owner._record('restoring')
        space = os.statvfs(owner.directory)
        if space.f_bavail * space.f_frsize < capacity + 16 * 1024 * 1024:
            raise RuntimeError('insufficient host space for a restored image and inspection headroom')
        restore(source, owner.image, expected)
        owner.image_identity = identity(owner.image)
        owner._verify_image()
        owner._record('restored')
        return owner

    def _record(self, phase):
        if identity(self.directory) != self.directory_identity:
            raise RuntimeError('owned volume directory identity changed before publication')
        record = dict(phase=phase, image=str(self.image), mount=str(self.mount),
                      size_mib=self.size_mib, owner_pid=os.getpid(),
                      directory_identity=self.directory_identity,
                      image_identity=self.image_identity, image_allocation=self.image_allocation, attached=self.attached,
                      retirement_reserve=self.reserve, retained_archive=self.archive)
        path = self.directory / 'volume.json'
        temporary = path.with_suffix('.tmp')
        with temporary.open('w') as stream:
            json.dump(record, stream, indent=2)
            stream.write('\n')
            stream.flush()
            os.fsync(stream.fileno())
        temporary.replace(path)

    def _verify_image(self):
        if identity(self.directory) != self.directory_identity:
            raise RuntimeError('owned volume directory identity changed')
        if self.removed or self.image_identity is None or identity(self.image) != self.image_identity:
            raise RuntimeError('owned image identity changed or unavailable')
        if self.image_identity['mode'] != stat.S_IFREG or self.image.stat().st_size != self.size_mib * 1024 * 1024:
            raise RuntimeError('owned image type or capacity differs')
        self.image_allocation = dict(logical_bytes=self.image.stat().st_size,
                                     allocated_bytes=allocated_bytes(self.image))
        if self.image_allocation['allocated_bytes'] < self.image_allocation['logical_bytes']:
            raise RuntimeError('owned image backing allocation is smaller than its capacity')

    def _mounted_images(self):
        inventory = plistlib.loads(self.command(['hdiutil', 'info', '-plist']))
        return [entry for entry in inventory['images'] if entry.get('image-path') == str(self.image)]

    def attach(self, *, readonly=False):
        self._verify_image()
        if self.attached is not None or self._mounted_images():
            raise RuntimeError('owned image already attached or attachment unresolved')
        if identity(self.mount) != self.mount_identity or any(self.mount.iterdir()):
            raise RuntimeError('owned mount directory changed or is not empty')
        self._record('attaching')
        arguments = ['hdiutil', 'attach', str(self.image), '-nobrowse',
                     '-mountpoint', str(self.mount), '-plist']
        if readonly:
            arguments.append('-readonly')
        reply = plistlib.loads(self.command(arguments))
        # Keep the original device/volume map, including any unexpected shape, before validation.
        self.attached = dict(reply=reply, readonly=readonly)
        self._record('attachment_observed')
        entities = reply['system-entities']
        devices = [e['dev-entry'] for e in entities
                   if re.fullmatch(r'/dev/disk\d+', e.get('dev-entry', ''))]
        if not devices or not any(e.get('mount-point') == str(self.mount) for e in entities):
            raise RuntimeError('attachment does not establish the owned mount')
        if self.mount.stat().st_dev == self.directory.stat().st_dev:
            raise RuntimeError('image mount did not establish a separate filesystem')
        self.attached.update(device=devices[0], mount_identity=identity(self.mount))
        self._verify_attachment()
        self._record('mounted')
        return self.mount

    def _verify_attachment(self):
        self._verify_image()
        if self.attached is None or 'device' not in self.attached:
            raise RuntimeError('no validated live attachment authority')
        images = self._mounted_images()
        if len(images) != 1:
            raise RuntimeError('live image attachment differs or is ambiguous')
        entries = images[0]['system-entities']
        expected = self.attached['reply']['system-entities']
        if {e.get('dev-entry') for e in entries} != {e.get('dev-entry') for e in expected}:
            raise RuntimeError('live image device map changed')
        if not any(e.get('mount-point') == str(self.mount) for e in entries):
            raise RuntimeError('live image mount path changed')
        if self.attached['readonly'] and not os.statvfs(self.mount).f_flag & os.ST_RDONLY:
            raise RuntimeError('requested read-only filesystem is writable')
        if identity(self.mount) != self.attached['mount_identity']:
            raise RuntimeError('mounted filesystem identity changed')

    def reserve_retirement(self, size_mib):
        """Allocate once while the caller still owns a quiescent writable volume.

        This is post-writer finalization headroom, not capacity for stopping live writers.
        A partial allocation remains evidence; it cannot silently become a new allowance.
        """
        if type(size_mib) is not int or not 1 <= size_mib <= min(64, self.size_mib // 4):
            raise ValueError('reserve size must be 1..64 MiB and at most a quarter of the volume')
        self._verify_attachment()
        if self.attached['readonly'] or self.reserve is not None:
            raise RuntimeError('reserve requires a writable attachment and no previous allowance')
        path = self.mount / 'retirement-reserve'
        self.reserve = dict(path=str(path), bytes=size_mib * 1024 * 1024, state='allocating')
        self._record('reserve_allocating')
        # Exclusive creation rejects an existing file or symlink. Random bytes avoid a sparse
        # placeholder; actual allocation is checked after syncing, not inferred from file length.
        with path.open('xb', buffering=0) as stream:
            os.chmod(path, 0o600)
            self.reserve['identity'] = identity(path)
            for _ in range(size_mib):
                block = memoryview(os.urandom(1024 * 1024))
                while block:
                    count = stream.write(block)
                    if not count:
                        raise RuntimeError('reserve allocation made no progress')
                    block = block[count:]
            os.fsync(stream.fileno())
            allocated = os.fstat(stream.fileno()).st_blocks * 512
        if allocated < self.reserve['bytes']:
            raise RuntimeError('reserve allocation is smaller than its requested capacity')
        self.reserve.update(state='reserved', allocated_bytes=allocated)
        self._record('reserve_ready')
        return dict(self.reserve)

    def release_retirement(self, *, writers_retired):
        """Release only the original allowance after the live owner proves quiescence."""
        if writers_retired is not True:
            raise RuntimeError('writers must retire before reserve release')
        self._verify_attachment()
        if self.attached['readonly'] or not self.reserve or self.reserve['state'] != 'reserved':
            raise RuntimeError('no original writable retirement reserve')
        path = Path(self.reserve['path'])
        if identity(path) != self.reserve['identity']:
            raise RuntimeError('retirement reserve identity changed')
        stats = path.lstat()
        if stats.st_nlink != 1 or stats.st_size != self.reserve['bytes']:
            raise RuntimeError('retirement reserve size or links changed')
        self._record('reserve_releasing')
        path.unlink()
        self.reserve['state'] = 'released'
        self._record('reserve_released')
        return dict(self.reserve)

    def detach(self, *, writers_retired):
        # This attestation comes from the live resource owner, never from a PID file.
        if writers_retired is not True:
            raise RuntimeError('writers must retire before volume detach')
        self._verify_attachment()
        self._record('detaching')
        self.command(['hdiutil', 'detach', self.attached['device']])
        if self._mounted_images() or identity(self.mount) != self.mount_identity:
            raise RuntimeError('volume detachment unresolved; retain the image')
        self.attached = None
        self._record('detached')

    def retained_image(self):
        """Hash the original detached image without granting attachment or deletion authority."""
        self._verify_image()
        if self.attached is not None or self._mounted_images():
            raise RuntimeError('cannot attest an attached or uncertain image')
        descriptor = os.open(self.image, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(descriptor, 'rb') as stream:
            before = os.fstat(stream.fileno())
            expected = self.image_identity
            if (before.st_dev != expected['device'] or before.st_ino != expected['inode']
                    or not stat.S_ISREG(before.st_mode) or before.st_nlink != 1
                    or before.st_size != self.size_mib * 1024 * 1024):
                raise RuntimeError('retained image identity or size changed')
            digest = hashlib.sha256()
            remaining = before.st_size
            while remaining:
                block = stream.read(min(65536, remaining))
                if not block:
                    raise RuntimeError('retained image shortened while hashing')
                digest.update(block)
                remaining -= len(block)
            after = os.fstat(stream.fileno())
            if stream.read(1) or (before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
                    after.st_size, after.st_mtime_ns, after.st_ctime_ns):
                raise RuntimeError('retained image changed while hashing')
        self._verify_image()
        return dict(bytes=before.st_size, sha256=digest.hexdigest())

    def compress_retained_image(self):
        """Preserve original image bytes in a verified archive before retiring raw storage."""
        from native_image_archive import compress, sync_directory
        original = self.retained_image()
        self._record('archiving')
        compressed = compress(self.image, original)
        self.archive = dict(path=str(self.image) + '.gz', **compressed)
        self._record('archive_verified')
        self._verify_image()
        self.image.unlink()
        sync_directory(self.directory)
        self.removed = True
        self._record('archived')
        return dict(**original, gzip=compressed)

    def remove(self):
        """Explicitly discard only after the caller has preserved the required evidence."""
        self._verify_image()
        if self.attached is not None or self._mounted_images():
            raise RuntimeError('cannot remove an attached or uncertain image')
        if identity(self.mount) != self.mount_identity or any(self.mount.iterdir()):
            raise RuntimeError('mount directory differs after detachment')
        self._record('removing')
        self.image.unlink()
        self.mount.rmdir()
        self.removed = True
        self._record('removed')
