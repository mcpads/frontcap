import json
import os
from unittest.mock import patch
from pathlib import Path
import tempfile
import unittest
from native_worker_control import TerminalRecord, LIMIT


class TerminalRecordTests(unittest.TestCase):
    def test_bounded_original_result_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)/'control';root.mkdir()
            record = TerminalRecord(root)
            with self.assertRaises(ValueError):record.publish({'error': 'x'*LIMIT})
            self.assertFalse((root/'result.json').exists())
            value = {'state': 'failed', 'retirement': {'passed': False, 'errors': ['original']}}
            record.publish(value)
            original = (root/'result.json').read_bytes()
            self.assertEqual(json.loads(original), value)
            with self.assertRaises(ValueError):record.publish({'state':'stopped'})
            self.assertEqual((root/'result.json').read_bytes(), original)

    def test_replaced_or_linked_directory_cannot_receive_terminal_record(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent=Path(temporary);root=parent/'control';root.mkdir()
            record=TerminalRecord(root);root.rename(parent/'original');root.mkdir()
            with self.assertRaises(ValueError):record.publish({'state':'stopped'})
            self.assertEqual(list(root.iterdir()), [])
            root.rmdir();root.symlink_to(parent/'original',target_is_directory=True)
            with self.assertRaises(ValueError):record.publish({'state':'stopped'})
            with self.assertRaises(ValueError):TerminalRecord(root)

    def test_concurrent_result_publication_preserves_the_first_result(self):
        with tempfile.TemporaryDirectory() as temporary:
            root=Path(temporary)/'control';root.mkdir();record=TerminalRecord(root)
            link=os.link
            def publish_first(*args,**kwargs):
                (root/'result.json').write_text('original competing result')
                return link(*args,**kwargs)
            with patch('native_worker_control.os.link',side_effect=publish_first), self.assertRaises(ValueError):
                record.publish({'state':'failed'})
            self.assertEqual((root/'result.json').read_text(),'original competing result')
            self.assertEqual(json.loads((root/'result.tmp').read_text()),{'state':'failed'})

    def test_directory_replacement_does_not_redirect_publication(self):
        with tempfile.TemporaryDirectory() as temporary:
            parent=Path(temporary);root=parent/'control';root.mkdir();record=TerminalRecord(root)
            link=os.link
            def replace_directory(*args,**kwargs):
                root.rename(parent/'original');root.mkdir()
                (root/'result.json').write_text('foreign evidence')
                return link(*args,**kwargs)
            with patch('native_worker_control.os.link',side_effect=replace_directory), self.assertRaises(ValueError):
                record.publish({'state':'failed'})
            self.assertEqual((root/'result.json').read_text(),'foreign evidence')
            self.assertEqual(json.loads((parent/'original/result.json').read_text()),{'state':'failed'})
