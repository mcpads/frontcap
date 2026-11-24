"""Flutter's live access to the existing independent native resource owner."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

from native_exchange_streams import read_exchange
from native_resource_group import ResourceGroup, monotonic_ns
from native_host_process import write_json
from native_write_budget import charge_payload


def validate(config, platform, installed, android=None, avd=None, sdk=None):
    if config is not None and (sys.platform != 'darwin' or (platform == 'android' and android is None) or installed is None
            or not isinstance(config, str) or not Path(config).is_absolute()):
        raise ValueError('Independent Flutter resources require an installed product, an absolute executable with original resource ownership')

    from flutter_android_resources import validate as validate_android
    validate_android(android, config, platform, installed, avd, sdk)


class Resources:
    def __init__(self, executable, output, host, config, env, deadline):
        self.output, self.env, self.number = output, env, 0
        self.android = config.get('android_resources')
        self.adb = str(Path(config['android_sdk'])/'platform-tools/adb') if self.android else None
        options = dict(ports=[self.android['port'], self.android['port']+1], scratch_volume=self.android['scratch_volume'], scratch_volume_root=self.android.get('scratch_volume_root')) if self.android else {}
        self.group = ResourceGroup(Path(executable), output/'resource-group', host=host,
            target=output/'resource-target', evidence_root=output,
            lifetime_s=config['acquisition_timeout_s']+config['lifetime_s']+120, **options)

    def acquire(self, config, deadline):
        output = self.output
        try:
            if self.android:
                from flutter_android_resources import acquire
                acquire(self, config, deadline)
                return
            simulator = dict(xcrun='/usr/bin/xcrun', runtime=config.get('runtime', 'com.apple.CoreSimulator.SimRuntime.iOS-26-1'),
                device_type='com.apple.CoreSimulator.SimDeviceType.iPhone-16', source_sha256=config['source_sha256'])
            if config.get('data_volume'):
                simulator.update(data_volume=dict(config['data_volume'], python=sys.executable,
                    worker=str(Path(__file__).with_name('native_owned_volume_worker.py'))),
                    data_volume_root=str(output/'simulator-data-volume'))
            result = self.group.execute(dict(operation='acquire_simulator', config=simulator,
                deadline_ns=min(self.group.deadline_ns, monotonic_ns()+int(max(0, deadline-time.monotonic())*10**9))),
                timeout=max(.001, deadline-time.monotonic()))
            if result['reply'] != 'simulator_acquired':
                raise RuntimeError('Independent Simulator acquisition unresolved')
            self.device = result['udid'].upper()
        except BaseException:
            self.group.revoke()
            raise

    def execute(self, argv, log, env, timeout, cwd):
        self.number += 1
        executable = shutil.which(str(argv[0]), path=env.get('PATH'))
        if executable is None:
            raise ValueError('Supervised command executable unavailable')
        # The group inherits the worker environment. Persist only actual overrides.
        overrides = {k: v for k, v in env.items() if os.environ.get(k) != v}
        if self.android:
            if executable != self.adb:
                raise ValueError('Owned Android command must use its frozen ADB executable')
            overrides['ADB_SERVER_SOCKET'] = f"tcp:127.0.0.1:{self.android['adb_server_port']}"
        evidence = self.output/f'resource-command-{self.number}'
        original = self.group.capture([executable, *argv[1:]], evidence,
            directory=cwd, environment=overrides, timeout=timeout, confine_adb=bool(self.android))
        exchange = read_exchange(original, original.read_bytes())
        if exchange['cleanup_failure'] or exchange['evidence_failure']:
            raise RuntimeError('Original supervised helper retirement or evidence unresolved')
        charge_payload(len(exchange['stdout']) + len(exchange['stderr']))
        log.write_bytes(bytes(exchange['stdout']))
        # Preserve existing callers' code/stdout interface without merging diagnostic warnings.
        log.with_name(log.name+'.stderr').write_bytes(bytes(exchange['stderr']))
        if exchange['failure'] and not (exchange['failure'] == 'bridge exited unsuccessfully'
                and type(exchange['exit_code']) is int and exchange['exit_code'] != 0):
            raise RuntimeError('Original supervised helper failed: '+exchange['failure'])
        return exchange['exit_code']

    def verify(self, bundle=None):
        if self.group.authority is None or self.group.child.poll() is not None:
            raise RuntimeError('Original independent resource owner unavailable')
        plan = self.output/'resource-target/simulator/data-relocation.json'
        if plan.exists():
            route = json.loads(plan.read_text())
            source, target = Path(route['source']), Path(route['destination'])
            if not source.is_symlink() or source.resolve() != target or target.resolve() != target:
                raise RuntimeError('Supervised Simulator data route changed')
            if bundle is not None:
                log = self.output/'supervised-container.log'
                if self.execute(['/usr/bin/xcrun','simctl','get_app_container',self.device,bundle,'data'],
                        log, self.env, 20, self.output):
                    raise RuntimeError('Supervised app container unavailable')
                data = Path(log.read_text().strip()).resolve(strict=True)
                if not data.is_relative_to(target) or data.stat().st_dev != target.stat().st_dev:
                    raise RuntimeError('Supervised app container escaped the original volume')

    def retire(self):
        result = self.group.stop()
        write_json(self.output/'cleanup.json', result)
        return result

    def spawn(self, arguments, env, directory):
        identity = 'system-runner'
        overrides = {k: v for k, v in env.items() if os.environ.get(k) != v}
        process = self.group.start_process(identity, arguments, directory=directory, environment=overrides)
        return Process(self.group, identity, process)


class Process:
    def __init__(self, group, identity, process):
        self.group, self.identity, self.pid = group, identity, process['pid']
        self.result = group.directory/'resources'/identity/'result.json'

    def poll(self):
        if self.result.exists():
            result = json.loads(self.result.read_text())
            if result['retirement_error']:
                raise RuntimeError('Original supervised runner retirement unresolved')
            return result['exit_code'] if result['exit_code'] is not None else -1
        if self.group.child.poll() is not None:
            raise RuntimeError('Independent owner exited without runner retirement')
        return None

    def wait(self, timeout):
        end = time.monotonic()+timeout
        while True:
            result = self.poll()
            if result is not None:
                return result
            if time.monotonic() >= end:
                raise subprocess.TimeoutExpired('supervised system runner', timeout)
            time.sleep(.02)

    def retire(self):
        reply = self.group.execute(dict(operation='stop', id=self.identity), timeout=15)
        if reply['reply'] != 'stopped' or reply['result']['retirement_error']:
            raise RuntimeError('Original supervised system runner retirement unresolved')
