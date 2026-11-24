"""Partial virtual-app evidence survives failure without replacing acceptance or its error."""
import json
from pathlib import Path
import runpy
import tempfile
import unittest
from unittest.mock import patch

exercise = runpy.run_path(str(Path(__file__).resolve().parents[1] /
    'spikes/s18-native-time/android/virtual_app_probe.py'))['exercise']


class VirtualAppProgressTests(unittest.TestCase):
    def fail_after_input(self, out):
        state = dict(sequence=0, count=0, fired=0)

        def command(*args):
            return json.dumps(state).encode()

        def tap(tree, target):
            if target == 'native-increment':
                state['count'] += 1
            return dict(target=target)

        class Control:
            def __init__(self, *args): pass
            def request(self, operation, delta=0):
                if operation == 'advance': raise RuntimeError('clock transport unavailable')
                state['sequence'] += 1
                return dict(observation=dict(state), request=dict(request_id=state['sequence']))

        with patch.dict(exercise.__globals__, ClockClient=Control), patch('time.sleep'), self.assertRaisesRegex(RuntimeError, 'clock transport unavailable'):
            exercise(out, command, lambda phase, count: ({}, dict(phase=phase)),
                     lambda tree, name: dict(text='state'), None, tap)

    def test_failure_retains_completed_phases_and_pending_input(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            self.fail_after_input(out)
            progress = json.loads((out / 'virtual-progress.json').read_text())
            self.assertEqual(progress['state'], 'failed')
            self.assertEqual(progress['active'], dict(phase='before-deadline', action='advance:4999000000'))
            self.assertEqual([row['phase'] for row in progress['observations']], ['frozen', 'held', 'input'])
            self.assertIn('clock transport unavailable', progress['error'])
            self.assertFalse((out / 'virtual.json').exists())

    def test_diagnostic_write_failure_does_not_mask_probe_error(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            (out / 'virtual-progress.json').mkdir()
            with patch('builtins.print'):
                self.fail_after_input(out)
            self.assertFalse((out / 'virtual.json').exists())


if __name__ == '__main__':
    unittest.main()
