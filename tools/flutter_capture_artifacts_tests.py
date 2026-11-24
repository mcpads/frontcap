import base64
import json
from pathlib import Path
import struct
import tempfile
import unittest
import zlib

from flutter_capture_artifacts import collect_captures


def image_bytes():
    def chunk(kind, payload):
        return struct.pack('>I', len(payload))+kind+payload+struct.pack('>I', zlib.crc32(kind+payload))
    return (b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR', struct.pack('>IIBBBBB', 1, 1, 8, 6, 0, 0, 0))
            +chunk(b'IDAT', zlib.compress(b'\0\xff\0\0\xff'))+chunk(b'IEND', b''))


class RasterTransportTests(unittest.TestCase):
    def record(self, root, data, *, count=1):
        log = root/'capture.log'
        log.write_text('FRONTCAP_FLUTTER_CAPTURE_META '+json.dumps(dict(name='frame', observation='o1'))+'\n'
            +'FRONTCAP_FLUTTER_CAPTURE_CHUNK '+json.dumps(dict(name='frame', index=0, count=count,
                                                              data=base64.b64encode(data).decode()))+'\n')
        return log

    def test_complete_png_survives_log_transport(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            data = image_bytes()
            report = collect_captures(self.record(root, data), root)
            self.assertTrue(report['passed'])
            self.assertEqual((root/'captures/frame.png').read_bytes(), data)

    def test_missing_chunk_preserves_semantics_metadata_as_partial(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            with self.assertRaises(RuntimeError):
                collect_captures(self.record(root, image_bytes(), count=2), root)
            report = json.loads((root/'capture-report.json').read_text())
            self.assertFalse(report['passed'])
            self.assertEqual(report['captures'][0]['metadata']['observation'], 'o1')
            self.assertFalse((root/'captures/frame.png').exists())

    def test_corrupt_png_cannot_qualify_as_complete(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            data = bytearray(image_bytes()); data[20] ^= 1
            with self.assertRaises(RuntimeError):
                collect_captures(self.record(root, data), root)
            self.assertFalse(json.loads((root/'capture-report.json').read_text())['passed'])

    def test_missing_entire_required_capture_cannot_pass(self):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            with self.assertRaises(RuntimeError):
                collect_captures(self.record(root, image_bytes()), root,
                                 required_names=('frame', 'later'))
            report = json.loads((root/'capture-report.json').read_text())
            self.assertEqual(report['missing'], ['later'])
            self.assertTrue(report['captures'][0]['complete'])


if __name__ == '__main__':
    unittest.main()
