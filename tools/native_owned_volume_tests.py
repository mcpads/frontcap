"""Negative ownership checks without creating or attaching host disks."""
from pathlib import Path
import plistlib
import tempfile
import unittest
from unittest.mock import patch

from native_owned_volume import OwnedVolume, identity


class OwnedVolumeTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / 'owner'
        self.calls = []
        self.images = []
        # Fake hdiutil creates a sparse stand-in; host allocation is qualified separately.
        allocation = patch('native_owned_volume.allocated_bytes', return_value=256 * 1024 * 1024)
        self.allocated = allocation.start()
        self.addCleanup(allocation.stop)

    def command(self, arguments):
        self.calls.append(arguments)
        if arguments[1] == 'create':
            with Path(arguments[-1]).open('xb') as stream:
                stream.truncate(256 * 1024 * 1024)
            return b''
        if arguments[1] == 'info':
            return plistlib.dumps(dict(images=self.images))
        self.fail('destructive command must not be issued by an ownership rejection')

    def test_sparse_backing_is_refused_before_attachment_and_preserved(self):
        self.allocated.return_value = 4096
        with self.assertRaisesRegex(RuntimeError, 'backing allocation'):
            OwnedVolume.create(self.root, 256, self.command)
        self.assertTrue((self.root/'bounded.dmg').exists())
        import json
        record = json.loads((self.root/'volume.json').read_text())
        self.assertEqual(record['phase'], 'creation_unresolved')
        self.assertEqual(record['image_allocation']['allocated_bytes'], 4096)
        self.assertEqual([call[1] for call in self.calls], ['create'])

    def test_replacement_image_and_existing_owner_directory_do_not_grant_authority(self):
        owner = OwnedVolume.create(self.root, 256, self.command)
        with self.assertRaises(FileExistsError):
            OwnedVolume.create(self.root, 256, self.command)
        original = owner.image.with_suffix('.original')
        owner.image.rename(original)
        with owner.image.open('xb') as stream:
            stream.truncate(original.stat().st_size)
        for operation in [owner.attach, owner.remove]:
            with self.assertRaisesRegex(RuntimeError, 'identity changed'):
                operation()
        self.assertTrue(original.exists())
        self.assertTrue(owner.image.exists())
        self.assertEqual(len(self.calls), 1)

    def test_attached_image_and_unretired_writers_cannot_be_removed_or_detached(self):
        owner = OwnedVolume.create(self.root, 256, self.command)
        self.images = [dict(**{'image-path': str(owner.image)}, **{'system-entities': []})]
        with self.assertRaisesRegex(RuntimeError, 'attached or uncertain'):
            owner.remove()
        with self.assertRaisesRegex(RuntimeError, 'writers must retire'):
            owner.detach(writers_retired=False)
        self.assertTrue(owner.image.exists())
        self.assertFalse(any(call[1] == 'detach' for call in self.calls))

    def test_changed_device_map_refuses_detach_and_preserves_image(self):
        owner = OwnedVolume.create(self.root, 256, self.command)
        owner.attached = dict(device='/dev/disk8', mount_identity=identity(owner.mount),
                              reply={'system-entities': [{'dev-entry': '/dev/disk8'}]})
        self.images = [{'image-path': str(owner.image), 'system-entities': [{'dev-entry': '/dev/disk9'}]}]
        with self.assertRaisesRegex(RuntimeError, 'device map changed'):
            owner.detach(writers_retired=True)
        self.assertTrue(owner.image.exists())
        self.assertFalse(any(call[1] == 'detach' for call in self.calls))

    def test_reserve_is_allocated_once_and_requires_retired_writers_to_release(self):
        owner = OwnedVolume.create(self.root, 256, self.command)
        owner.attached = dict(readonly=False)
        # Unit scope: real file allocation/ownership; mounted-image validation is qualified
        # separately by the opt-in volume probe rather than simulated as a host guarantee.
        with patch.object(owner, '_verify_attachment'):
            receipt = owner.reserve_retirement(1)
            reserve = Path(receipt['path'])
            self.assertGreaterEqual(reserve.stat().st_blocks * 512, 1024 * 1024)
            with self.assertRaisesRegex(RuntimeError, 'writers must retire'):
                owner.release_retirement(writers_retired=False)
            self.assertTrue(reserve.exists())
            with self.assertRaisesRegex(RuntimeError, 'previous allowance'):
                owner.reserve_retirement(1)
            self.assertEqual(owner.release_retirement(writers_retired=True)['state'], 'released')
            self.assertFalse(reserve.exists())
            with self.assertRaisesRegex(RuntimeError, 'previous allowance'):
                owner.reserve_retirement(1)
            with self.assertRaisesRegex(RuntimeError, 'no original'):
                owner.release_retirement(writers_retired=True)

    def test_replaced_reserve_does_not_grant_deletion_authority(self):
        owner = OwnedVolume.create(self.root, 256, self.command)
        owner.attached = dict(readonly=False)
        with patch.object(owner, '_verify_attachment'):
            reserve = Path(owner.reserve_retirement(1)['path'])
            original = reserve.with_suffix('.original')
            reserve.rename(original)
            reserve.write_bytes(b'unrelated')
            with self.assertRaisesRegex(RuntimeError, 'identity changed'):
                owner.release_retirement(writers_retired=True)
            self.assertEqual(reserve.read_bytes(), b'unrelated')
            self.assertEqual(original.stat().st_size, 1024 * 1024)


if __name__ == '__main__':
    unittest.main()
