"""Acquire a frozen Android device through the original independent resource group."""
from pathlib import Path
import re
import sys
import time


def validate(plan, executable, platform, installed, avd, sdk):
    if plan is None:
        return
    if (sys.platform != 'darwin' or platform != 'android' or installed is None or executable is None
            or not isinstance(plan, dict) or set(plan) - {'scratch_volume_root'} != {'avd_home', 'avd_inventory', 'port', 'adb_server_port', 'scratch_volume'}
            or not isinstance(avd, str) or avd in ('.', '..') or not re.fullmatch(r'[A-Za-z0-9_.-]{1,128}', avd)
            or not isinstance(sdk, str) or not Path(sdk).is_absolute()
            or not isinstance(plan['avd_home'], str) or not Path(plan['avd_home']).is_absolute()
            or type(plan['port']) is not int or not 1024 <= plan['port'] <= 65534 or plan['port'] % 2
            or type(plan['adb_server_port']) is not int or not 1024 <= plan['adb_server_port'] <= 65535
            or plan['adb_server_port'] in (5037, plan['port'], plan['port']+1)):
        raise ValueError('Invalid installed Flutter Android resource configuration')
    root = plan.get('scratch_volume_root')
    if root is not None:
        if (not isinstance(root, str) or not Path(root).is_absolute() or '..' in Path(root).parts
                or Path(root).parent == Path(root) or Path(root).parent.resolve(strict=True) != Path(root).parent):
            raise ValueError('Scratch volume requires a canonical absolute parent')
    volume = plan['scratch_volume']
    if (not isinstance(volume, dict) or set(volume) != {'python', 'worker', 'size_mib', 'reserve_mib'}
            or any(not isinstance(volume[key], str) or not Path(volume[key]).is_absolute() for key in ('python', 'worker'))
            or type(volume['size_mib']) is not int or not 256 <= volume['size_mib'] <= 65536
            or type(volume['reserve_mib']) is not int or not 1 <= volume['reserve_mib'] <= min(64, volume['size_mib']//4)
            or not isinstance(plan['avd_inventory'], dict)):
        raise ValueError('Invalid Flutter Android scratch volume or AVD inventory')


def acquire(resources, config, deadline):
    from flutter_resource_group import Process
    plan = resources.android
    sdk = Path(config['android_sdk'])
    adb = sdk/'platform-tools/adb'
    def remaining(limit):
        value = min(limit, deadline-time.monotonic())
        if value <= 0:
            raise TimeoutError('Flutter Android acquisition expired')
        return value
    resources.group.prepare_avd(Path(plan['avd_home']), config['avd'], plan['avd_inventory'], timeout=remaining(120))
    port = plan['adb_server_port']
    server = resources.group.start_adb_server('adb', port,
        [str(adb), '-L', f'tcp:{port}', 'server', 'nodaemon'], directory=resources.output, confine=True, timeout=remaining(20))
    resources.env['ADB_SERVER_SOCKET'] = f'tcp:127.0.0.1:{port}'
    server_process = Process(resources.group, 'adb', server)
    while True:
        if server_process.poll() is not None:
            raise RuntimeError('Original private ADB server exited')
        log = resources.output/'adb-readiness.log'
        code = resources.execute([str(adb), 'devices', '-l'], log, resources.env, remaining(5), resources.output)
        if code == 0:
            if log.read_text().strip() != 'List of devices attached':
                raise RuntimeError('Private ADB server has unexpected transports')
            break
        time.sleep(min(.1, remaining(1)))
    resources.emulator = resources.group.start_emulator('emulator', [str(sdk/'emulator/emulator'),
        '-avd', config['avd'], '-no-window', '-no-audio', '-no-snapshot', '-read-only', '-no-boot-anim',
        '-port', str(plan['port']), '-cores', '4', '-memory', '2048'], directory=resources.output, timeout=remaining(10))
    resources.device = 'emulator-'+str(plan['port'])
    emulator = Process(resources.group, 'emulator', resources.emulator)
    while True:
        if emulator.poll() is not None or server_process.poll() is not None:
            raise RuntimeError('Original Android resources exited before boot')
        log = resources.output/'android-boot.log'
        code = resources.execute([str(adb), '-s', resources.device, 'shell', 'getprop', 'sys.boot_completed'],
            log, resources.env, remaining(5), resources.output)
        if code == 0 and log.read_text().strip() == '1':
            break
        time.sleep(min(.5, remaining(1)))
