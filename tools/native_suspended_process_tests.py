"""Suspended launch defers real child effects and preserves continuation or signal termination."""
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import unittest

from native_android_tap import host_processes
from native_suspended_process import spawn_suspended, wait_child


@unittest.skipUnless(sys.platform == 'darwin', 'Darwin suspended launch capability')
class SuspendedProcessTests(unittest.TestCase):
    def test_child_effects_wait_for_continue_and_kill_keeps_them_absent(self):
        for action in ('continue', 'kill'):
            with self.subTest(action=action), tempfile.TemporaryDirectory() as temporary:
                marker = Path(temporary)/'effect'
                output = Path(temporary)/'stdout'
                pid = None
                code = ('import pathlib,sys; pathlib.Path(sys.argv[1]).write_text("delivered"); '
                        'print("child output"); raise SystemExit(7)')
                try:
                    with output.open('wb') as stream:
                        pid = spawn_suspended([sys.executable, '-c', code, str(marker)], stream.fileno())
                    child, = [p for p in host_processes() if p['pid'] == pid]
                    self.assertEqual(child['ppid'], os.getpid())
                    self.assertEqual(child['group'], os.getpgrp())
                    self.assertIn('T', child['state'])
                    self.assertFalse(marker.exists())
                    self.assertEqual(output.read_bytes(), b'')
                    with self.assertRaises(subprocess.TimeoutExpired): wait_child(pid, .05)
                    self.assertFalse(marker.exists())
                    os.kill(pid, signal.SIGCONT if action == 'continue' else signal.SIGKILL)
                    result = wait_child(pid, 2)
                    retired = pid; pid = None
                    self.assertEqual(result, 7 if action == 'continue' else -9)
                    self.assertEqual(marker.exists(), action == 'continue')
                    self.assertEqual(output.read_bytes(), b'child output\n' if action == 'continue' else b'')
                    self.assertFalse([p for p in host_processes() if p['pid'] == retired])
                finally:
                    if pid is not None:
                        os.kill(pid, signal.SIGKILL)
                        wait_child(pid, 2)

    def test_missing_executable_preserves_spawn_error(self):
        with tempfile.TemporaryDirectory() as temporary:
            with (Path(temporary)/'stdout').open('wb') as stream:
                with self.assertRaises(FileNotFoundError):
                    spawn_suspended([temporary+'/missing-program'], stream.fileno())


if __name__ == '__main__': unittest.main()
