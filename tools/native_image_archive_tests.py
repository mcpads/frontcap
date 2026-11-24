"""Preserve exact source bytes through exclusive, durable archive publication."""
import gzip
import hashlib
from pathlib import Path
import tempfile
import unittest
from native_image_archive import compress, restore


class ArchiveTests(unittest.TestCase):
    def test_lossless_archive_preserves_source_and_refuses_replacement(self):
        with tempfile.TemporaryDirectory() as temporary:
            image = Path(temporary) / 'bounded.dmg'
            original = b'original evidence\n' * 65536
            image.write_bytes(original)
            expected = dict(bytes=len(original), sha256=hashlib.sha256(original).hexdigest())
            result = compress(image, expected)
            archive = image.with_name('bounded.dmg.gz')
            self.assertLess(result['bytes'], expected['bytes'])
            self.assertEqual(hashlib.sha256(archive.read_bytes()).hexdigest(), result['sha256'])
            self.assertEqual(gzip.decompress(archive.read_bytes()), original)
            self.assertEqual(image.read_bytes(), original)
            with self.assertRaises(FileExistsError):
                compress(image, expected)
            self.assertEqual(gzip.decompress(archive.read_bytes()), original)
            self.assertEqual(image.read_bytes(), original)

    def test_restore_verifies_raw_and_encoded_identity_and_never_overwrites(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'image'
            original = b'preserved bytes' * 10000
            source.write_bytes(original)
            expected = dict(bytes=len(original), sha256=hashlib.sha256(original).hexdigest())
            gzip_proof = compress(source, expected)
            archive = root / 'image.gz'
            proof = dict(**expected, gzip=gzip_proof)
            for index, (path, receipt) in enumerate([(source, expected), (archive, proof)]):
                target = root / str(index)
                restore(path, target, receipt)
                self.assertEqual(target.read_bytes(), original)
                with self.assertRaises(FileExistsError):
                    restore(path, target, receipt)
            wrong = dict(proof, sha256='0' * 64)
            with self.assertRaisesRegex(RuntimeError, 'decoded image differs'):
                restore(archive, root / 'wrong', wrong)
            self.assertFalse((root / 'wrong').exists())
            self.assertEqual(source.read_bytes(), original)
            trailing = archive.read_bytes() + gzip.compress(b'')
            extra = root / 'trailing.gz'
            extra.write_bytes(trailing)
            extra_proof = dict(proof, gzip=dict(bytes=len(trailing), sha256=hashlib.sha256(trailing).hexdigest()))
            with self.assertRaisesRegex(RuntimeError, 'trailing bytes'):
                restore(extra, root / 'trailing', extra_proof)
            corrupted = bytearray(archive.read_bytes()); corrupted[-1] ^= 1
            archive.write_bytes(corrupted)
            with self.assertRaisesRegex(RuntimeError, 'stored image digest differs'):
                restore(archive, root / 'damaged', proof)
            self.assertFalse((root / 'damaged').exists())

    def test_invalid_source_digest_never_publishes_archive_or_removes_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            image = Path(temporary) / 'bounded.dmg'
            image.write_bytes(b'evidence')
            with self.assertRaisesRegex(RuntimeError, 'changed during compression'):
                compress(image, dict(bytes=8, sha256='0' * 64))
            self.assertFalse(image.with_name('bounded.dmg.gz').exists())
            self.assertEqual(image.read_bytes(), b'evidence')
            self.assertTrue(image.with_name('bounded.dmg.gz.partial').exists())


if __name__ == '__main__':
    unittest.main()
