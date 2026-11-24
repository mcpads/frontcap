"""Optional live-owner Simulator data volume for the Flutter worker.

No saved image or PID is adopted. Abrupt owner loss leaves the host fence unretired.
Shared CoreSimulator services and logs are outside this filesystem.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import time
import uuid

from native_owned_volume import OwnedVolume, identity
from native_host_process import write_json
from native_write_budget import charge_payload


def validate(config, platform, installed):
    if config is None:
        return
    if (platform != 'ios' or installed is None or sys.platform != 'darwin'
            or not isinstance(config, dict) or set(config) != {'size_mib', 'reserve_mib'}
            or type(config['size_mib']) is not int or not 256 <= config['size_mib'] <= 65536
            or type(config['reserve_mib']) is not int
            or not 1 <= config['reserve_mib'] <= min(64, config['size_mib']//4)):
        raise ValueError('Flutter data volume requires an installed iOS product on macOS and valid bounds')


def inventory(root):
    result = {}
    total = 0
    for path in root.rglob('*'):
        if len(result) >= 100000:
            raise ValueError('Initial Simulator data inventory exceeds entry limit')
        key = str(path.relative_to(root))
        if path.is_symlink():
            raise ValueError('Linked initial Simulator data is unsupported')
        elif path.is_dir():
            result[key] = dict(directory=True)
        elif path.is_file():
            size = path.stat().st_size
            total += size
            if total > 512*1024*1024:
                raise ValueError('Initial Simulator data exceeds payload limit')
            with path.open('rb') as stream:
                digest = hashlib.sha256()
                remaining = size
                while remaining:
                    block = stream.read(min(65536, remaining))
                    if not block: raise ValueError('Initial Simulator file shortened')
                    digest.update(block); remaining -= len(block)
                if stream.read(1): raise ValueError('Initial Simulator file grew')
            result[key] = dict(bytes=size, sha256=digest.hexdigest(), executable=bool(path.stat().st_mode & 0o111))
        else:
            raise ValueError('Unsupported initial Simulator data entry')
    return result


class SimulatorStorage:
    def __init__(self, directory, config, command):
        self.directory = Path(directory)
        self.config, self.command = config, command
        self.owner = None
        self.attempted = False
        self.device = None
        self.source = self.target = self.mount_identity = None

    def prepare(self, device):
        self.device = str(uuid.UUID(device)).upper()
        devices = json.loads(self.command(['xcrun', 'simctl', 'list', 'devices', '-j']))
        matches = [d for rows in devices['devices'].values() for d in rows if d['udid'] == self.device]
        source = Path.home()/'Library/Developer/CoreSimulator/Devices'/self.device/'data'
        if (len(matches) != 1 or matches[0]['state'] != 'Shutdown'
                or Path(matches[0]['dataPath']) != source or source.is_symlink() or not source.is_dir()
                or source.resolve() != source):
            raise ValueError('Fresh Simulator data identity differs from its inventory')
        original = identity(source)
        initial = inventory(source)
        self.attempted = True
        self.owner = OwnedVolume.create(self.directory, self.config['size_mib'], self.command)
        mount = self.owner.attach()
        self.owner.reserve_retirement(self.config['reserve_mib'])
        if mount.stat().st_dev == source.stat().st_dev:
            raise ValueError('Simulator data requires a separate filesystem')
        target = mount/'device-data'
        charge_payload(sum(entry.get('bytes', 0) for entry in initial.values()), 'ordinary')
        self.command(['/usr/bin/ditto', str(source), str(target)])
        if inventory(target) != initial or identity(source) != original or inventory(source) != initial:
            raise ValueError('Initial Simulator data copy changed')
        backup = source.with_name('data-frontcap-original')
        if os.path.lexists(backup):
            raise ValueError('Original Simulator data backup already exists')
        write_json(self.directory/'relocation.json', dict(device=self.device, source=str(source), backup=str(backup),
            destination=str(target), original_identity=original, mounted_identity=identity(mount),
            initial_inventory=initial, scope='simulator_data_only', whole_run_quota=False))
        source.rename(backup)
        source.symlink_to(target, target_is_directory=True)
        if source.resolve() != target or target.stat().st_dev != mount.stat().st_dev:
            raise ValueError('Simulator data relocation differs')
        shutil.rmtree(backup)
        self.source, self.target, self.mount_identity = source, target, identity(mount)

    def verify(self, bundle=None):
        if (self.owner is None or self.source is None or not self.source.is_symlink()
                or self.source.resolve() != self.target or self.target.resolve() != self.target
                or identity(self.owner.mount) != self.mount_identity):
            raise ValueError('Owned Flutter Simulator data route changed')
        if bundle is not None:
            raw = self.command(['xcrun', 'simctl', 'get_app_container', self.device, bundle, 'data'])
            path = Path(raw.decode().strip()).resolve(strict=True)
            if path == self.target or not path.is_relative_to(self.target) or path.stat().st_dev != self.target.stat().st_dev:
                raise ValueError('Flutter app container is outside its owned data volume')

    def retire(self):
        if self.device is not None:
            devices = json.loads(self.command(['xcrun', 'simctl', 'list', 'devices', '-j']))
            if any(d['udid'] == self.device for rows in devices['devices'].values() for d in rows):
                raise ValueError('Simulator still exists; preserve its data volume')
        if self.owner is None:
            if self.attempted:
                raise ValueError('Simulator volume creation outcome unresolved')
            return
        if self.owner.attached is not None:
            self.owner.release_retirement(writers_retired=True)
            deadline = time.monotonic()+30
            while True:
                try:
                    self.owner.detach(writers_retired=True)
                    break
                except Exception:
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(1)
        self.owner.remove()
        write_json(self.directory/'retirement.json', dict(passed=True, removed=True))
