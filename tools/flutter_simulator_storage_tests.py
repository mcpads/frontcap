import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch
from flutter_simulator_storage import SimulatorStorage, inventory, validate

class SimulatorStorageTests(unittest.TestCase):
    def test_configuration_refuses_other_platforms_source_builds_and_bad_capacity(self):
        with patch('flutter_simulator_storage.sys.platform', 'darwin'):
            good = dict(size_mib=8192, reserve_mib=64)
            validate(good, 'ios', {})
            for config, platform, installed in [(good, 'android', {}), (good, 'ios', None),
                (dict(size_mib=True, reserve_mib=1), 'ios', {}),
                (dict(size_mib=256, reserve_mib=65), 'ios', {})]:
                with self.assertRaises(ValueError): validate(config, platform, installed)
        validate(None, 'android', None)

    def test_copy_is_verified_before_relocation_and_device_must_retire_before_volume(self):
        import shutil
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary).resolve()
            uid = '11111111-1111-4111-8111-111111111111'
            source = home/'Library/Developer/CoreSimulator/Devices'/uid/'data'
            source.mkdir(parents=True)
            (source/'settings').write_bytes(b'original')
            mount = home/'mount'; mount.mkdir()
            directory = home/'volume'; directory.mkdir()
            devices = [dict(udid=uid, state='Shutdown', dataPath=str(source))]
            def command(argv):
                if argv[0] == '/usr/bin/ditto':
                    shutil.copytree(argv[1], argv[2]); return b''
                if argv[2] == 'get_app_container':
                    return str(source/'Containers/app').encode()
                return json.dumps(dict(devices={'runtime': devices})).encode()
            owner = Mock(attached=True, mount=mount)
            owner.attach.return_value = mount
            original_stat = Path.stat
            def mounted_stat(path, *args, **kwargs):
                metadata = original_stat(path, *args, **kwargs)
                if path.is_relative_to(mount):
                    fields = list(metadata); fields[2] += 1
                    return os.stat_result(fields)
                return metadata
            storage = SimulatorStorage(directory, dict(size_mib=8192,reserve_mib=64), command)
            with patch.object(Path, 'home', return_value=home), patch.object(Path, 'stat', mounted_stat), \
                 patch('flutter_simulator_storage.OwnedVolume.create', return_value=owner):
                storage.prepare(uid)
                self.assertTrue(source.is_symlink())
                self.assertEqual((source/'settings').read_bytes(), b'original')
                (source/'Containers/app').mkdir(parents=True)
                storage.verify('dev.client')
                source.unlink(); source.symlink_to(home)
                with self.assertRaisesRegex(ValueError, 'route changed'):storage.verify()
                source.unlink(); source.symlink_to(mount/'device-data')
                with self.assertRaisesRegex(ValueError, 'still exists'):storage.retire()
                owner.detach.assert_not_called()
                devices.clear()
                storage.retire()
                owner.release_retirement.assert_called_once_with(writers_retired=True)
                owner.detach.assert_called_once_with(writers_retired=True)
                owner.remove.assert_called_once()

    def test_foreign_device_and_unresolved_image_creation_are_never_adopted(self):
        storage = SimulatorStorage(Path('/unused'), {}, lambda _: b'{"devices":{}}')
        with patch('flutter_simulator_storage.OwnedVolume.create') as create:
            with self.assertRaises(ValueError): storage.prepare('11111111-1111-4111-8111-111111111111')
            create.assert_not_called()
        storage.attempted = True
        with self.assertRaisesRegex(ValueError, 'unresolved'):storage.retire()

    def test_inventory_tracks_bytes_and_refuses_links_without_following_external_data(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root/'data').write_bytes(b'a')
            first = inventory(root)
            (root/'data').write_bytes(b'b')
            self.assertNotEqual(first, inventory(root))
            (root/'link').symlink_to('/not-followed')
            with self.assertRaisesRegex(ValueError, 'Linked'):inventory(root)
