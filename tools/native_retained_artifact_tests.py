import hashlib
from pathlib import Path
import tempfile
import unittest
from native_retained_artifact import read_artifact, read_uikit_artifact


class ArtifactTests(unittest.TestCase):
    def test_uikit_epoch_lookup_requires_one_exact_receipt_match(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / 'g-test/target'
            epoch = target / 'epochs/00000000-0000-0000-0000-000000000001/ui-client'
            epoch.mkdir(parents=True)
            data = b'epoch capture'
            (epoch / 'capture').write_bytes(data)
            proof = dict(path='capture', bytes=len(data), sha256=hashlib.sha256(data).hexdigest())
            self.assertEqual(read_uikit_artifact(root, 'g-test', proof), data)
            (target / 'ui-client').mkdir()
            (target / 'ui-client/capture').write_bytes(data)
            with self.assertRaisesRegex(RuntimeError, 'multiple UIKit epochs'):
                read_uikit_artifact(root, 'g-test', proof)
            (target / 'ui-client/capture').unlink()
            (epoch / 'capture').write_bytes(b'x' * len(data))
            with self.assertRaisesRegex(RuntimeError, 'differs from original receipt'):
                read_uikit_artifact(root, 'g-test', proof)

    def test_receipt_bound_read_refuses_escape_links_and_changed_content(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'observations').mkdir()
            data = b'original capture'
            path = root / 'observations/capture'
            path.write_bytes(data)
            proof = dict(path='observations/capture', bytes=len(data), sha256=hashlib.sha256(data).hexdigest())
            self.assertEqual(read_artifact(root, proof), data)
            for relative in ('../capture', '/capture', 'observations/../capture', 'observations//capture'):
                with self.assertRaises(ValueError):
                    read_artifact(root, dict(proof, path=relative))
            (root / 'linked').symlink_to(root / 'observations', target_is_directory=True)
            with self.assertRaises(OSError):
                read_artifact(root, dict(proof, path='linked/capture'))
            path.write_bytes(b'x' * len(data))
            with self.assertRaisesRegex(RuntimeError, 'differs from original receipt'):
                read_artifact(root, proof)
            path.unlink(); path.symlink_to(root / 'missing')
            with self.assertRaises(OSError):
                read_artifact(root, proof)


if __name__ == '__main__':
    unittest.main()
