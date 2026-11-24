"""Opt-in owned Simulator data relocation on a real bounded filesystem.

Uses only a newly created default-set device. This is a feasibility probe, not
whole-Run containment: CoreSimulator services and host evidence remain outside.
"""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import sys
import time
import traceback

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'tools'))
from native_command_capture import capture
from native_owned_volume import OwnedVolume, identity
from native_host_process import execute


def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')


def tree(root):
    result = {}
    for path in sorted(root.rglob('*')):
        key = str(path.relative_to(root))
        if path.is_symlink():
            result[key] = dict(link=os.readlink(path))
        elif path.is_file():
            result[key] = dict(bytes=path.stat().st_size,
                               sha256=hashlib.sha256(path.read_bytes()).hexdigest())
        elif path.is_dir():
            result[key] = dict(directory=True)
        else:
            raise RuntimeError(f'unsupported initial data entry: {key}')
    return result


def probe(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    host = Path.home() / '.cache/frontcap'
    lock = (host / 'native-tests.lock').open('a+')
    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    assert json.loads((host / 'native-session.json').read_text())['retired']
    number = 0

    def call(command, timeout=90, allow_failure=False):
        nonlocal number
        number += 1
        directory = output / f'command-{number:03d}'
        result = capture(command, directory, timeout=timeout)
        assert result['child_retired'], result
        if not allow_failure and result['error']:
            raise RuntimeError(f'command {number}: {result["error"]}')
        return (directory / 'stdout').read_bytes()

    def devices():
        value = json.loads(call(['/usr/bin/xcrun', 'simctl', 'list', 'devices', '-j']))
        return [d for ds in value['devices'].values() for d in ds]

    baseline = devices()
    assert not any(d['state'] == 'Booted' for d in baseline)
    test = args.xctestrun.resolve(strict=True)
    test_hash = hashlib.sha256(test.read_bytes()).hexdigest()
    sources = [Path(__file__).resolve(), Path(__file__).with_name('system_input.py').resolve()] + [Path(__file__).resolve().parents[2] / 'tools' / name
        for name in ['native_owned_volume.py', 'native_command_capture.py', 'native_host_process.py']]
    save(output / 'inputs.json', dict(runtime=args.runtime, device_type=args.device_type,
         xctestrun=str(test), xctestrun_sha256=test_hash, size_mib=args.size_mib,
         exercise_system_input=args.exercise_system_input,
         sources={str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in sources},
         baseline_devices=baseline))
    state = dict(state='running', pid=os.getpid(), started=time.time())
    save(output / 'state.json', state)
    save(host / 'native-session.json', dict(directory=str(output), retired=False))
    reports = Path.home() / 'Library/Logs/DiagnosticReports'
    before = {p.name for p in reports.glob('*.ips')}
    owner = uid = device_root = None
    device_retired = False
    try:
        owner = OwnedVolume.create(output / 'volume', args.size_mib, call)
        mount = owner.attach()
        owner.reserve_retirement(64)
        uid = call(['/usr/bin/xcrun', 'simctl', 'create', 'frontcap-bounded-data',
                    args.device_type, args.runtime]).decode().strip()
        assert len(uid) == 36
        save(output / 'device.json', dict(udid=uid))
        assert [d['state'] for d in devices() if d['udid'] == uid] == ['Shutdown']
        device_root = Path.home() / 'Library/Developer/CoreSimulator/Devices' / uid
        data = device_root / 'data'
        assert data.is_dir() and not data.is_symlink()
        original = identity(data)
        initial = tree(data)
        relocated = mount / 'device-data'
        call(['/usr/bin/ditto', str(data), str(relocated)])
        assert initial == tree(relocated) and identity(data) == original
        save(output / 'initial-data.json', initial)
        shutil.rmtree(data)
        data.symlink_to(relocated, target_is_directory=True)
        save(output / 'relocation.json', dict(data=str(data), target=str(relocated),
             original_identity=original, mounted_identity=identity(mount), relocated_identity=identity(relocated)))
        call(['/usr/bin/xcrun', 'simctl', 'boot', uid])
        call(['/usr/bin/xcrun', 'simctl', 'bootstatus', uid, '-b'], 120)
        command = ['/usr/bin/xcodebuild', 'test-without-building', '-xctestrun', str(test),
                   '-destination', 'platform=iOS Simulator,id=' + uid, '-destination-timeout', '5',
                   '-enumerate-tests', '-test-enumeration-format', 'json']
        save(output / 'enumeration-command.json', dict(command=command, timeout=30))
        execute(command, output / 'enumeration.log', os.environ.copy(), 30, output)
        assert 'testSystemBridge' in (output / 'enumeration.log').read_text()
        container = Path(call(['/usr/bin/xcrun', 'simctl', 'get_app_container', uid,
                              'dev.frontcap.system-ui.tests.xctrunner', 'data']).decode().strip())
        assert container.resolve().is_relative_to(relocated.resolve())
        assert container.stat().st_dev == mount.stat().st_dev
        assert data.is_symlink() and data.resolve() == relocated.resolve()
        stats = os.statvfs(mount)
        save(output / 'observed-storage.json', dict(container=str(container), resolved=str(container.resolve()),
             filesystem_device=mount.stat().st_dev, runtime_device=output.stat().st_dev,
             filesystem_bytes=stats.f_blocks * stats.f_frsize, available_bytes=stats.f_bavail * stats.f_frsize,
             container_entries=[str(p.relative_to(container)) for p in container.iterdir()]))
        if args.exercise_system_input:
            from system_input import exercise
            exercise(output, uid, test, container, call)
        call(['/usr/bin/xcrun', 'simctl', 'io', uid, 'screenshot', str(output / 'screen.png')])
        assert hashlib.sha256(test.read_bytes()).hexdigest() == test_hash
        state.update(state='passed', finished=time.time())
    except BaseException:
        state.update(state='failed', error=traceback.format_exc(), finished=time.time())
        raise
    finally:
        try:
            if uid:
                current = [d for d in devices() if d['udid'] == uid]
                if current and current[0]['state'] != 'Shutdown':
                    call(['/usr/bin/xcrun', 'simctl', 'shutdown', uid], 60)
                call(['/usr/bin/xcrun', 'simctl', 'delete', uid], 60)
                assert all(d['udid'] != uid for d in devices()) and not device_root.exists()
            device_retired = True
            if owner and owner.attached:
                owner.release_retirement(writers_retired=True)
                # Device shutdown/delete may return before CoreSimulator releases the mount.
                # Keep this original owner alive for bounded ordinary detach retries; each
                # attempt revalidates the original image, device map and mount identities.
                deadline = time.monotonic() + 30
                attempts = []
                while True:
                    try:
                        owner.detach(writers_retired=True)
                        attempts.append(dict(detached=True, observed=time.time()))
                        save(output / 'detach-attempts.json', attempts)
                        break
                    except Exception as error:
                        attempts.append(dict(detached=False, error=str(error), observed=time.time()))
                        save(output / 'detach-attempts.json', attempts)
                        if time.monotonic() >= deadline:
                            raise
                        time.sleep(1)
            if owner:
                owner.remove()
            state.update(device_deleted=True, volume_removed=bool(owner))
        except BaseException:
            state.update(state='failed', cleanup_error=traceback.format_exc())
            raise
        finally:
            crashes = []
            if uid:
                for p in reports.glob('*.ips'):
                    if p.name not in before and uid in p.read_text(errors='replace'):
                        crashes.append(dict(name=p.name, sha256=hashlib.sha256(p.read_bytes()).hexdigest(), family=p.name.startswith('Family-')))
            save(output / 'owned-crashes.json', crashes)
            state['family_reports'] = sum(c['family'] for c in crashes)
            if state['family_reports']:
                state.update(state='failed', family_error='Owned Family crash observed')
            save(output / 'state.json', state)
            if device_retired and (owner is None or owner.removed):
                save(host / 'native-session.json', dict(directory=str(output), retired=True))
    assert state['state'] == 'passed', state


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--xctestrun', type=Path, required=True)
    parser.add_argument('--runtime', required=True)
    parser.add_argument('--device-type', required=True)
    parser.add_argument('--size-mib', type=int, default=8192)
    parser.add_argument('--exercise-system-input', action='store_true')
    probe(parser.parse_args())
