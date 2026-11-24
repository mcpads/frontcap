"""Validate resource units and unavailable observations on owned processes."""
import errno
import os
import subprocess
import sys
import time
import unittest
from unittest.mock import patch

from native_process_resources import process_resources


class ProcessResourcesTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == 'darwin', 'macOS process accounting required')
    def test_live_cpu_units_identity_and_retired_process(self):
        process_resources(os.getpid())  # Keep library initialization outside the CPU bracket.
        before = time.process_time_ns()
        first = process_resources(os.getpid())
        after = time.process_time_ns()
        until = after + 30_000_000
        while time.process_time_ns() < until:
            pass
        end_before = time.process_time_ns()
        last = process_resources(os.getpid())
        end_after = time.process_time_ns()
        self.assertEqual((first['status'], last['status']), ('available', 'available'))
        self.assertEqual(first['identity'], last['identity'])
        elapsed = last['cpu_user_ns'] + last['cpu_system_ns'] - first['cpu_user_ns'] - first['cpu_system_ns']
        self.assertGreaterEqual(elapsed, end_before - after - 2_000_000)
        self.assertLessEqual(elapsed, end_after - before + 2_000_000)
        child = subprocess.Popen(['/bin/sleep', '3'])
        try:
            live = process_resources(child.pid)
            self.assertEqual(live['status'], 'available')
            self.assertEqual(live['identity']['pid'], child.pid)
            self.assertNotEqual(live['identity'], first['identity'])
            self.assertEqual(live['raw']['ri_proc_exit_abstime'], 0)
        finally:
            child.terminate()
            child.wait(timeout=5)
        retired = process_resources(child.pid)
        self.assertEqual((retired['status'], retired['errno']), ('unavailable', errno.ESRCH))
        self.assertNotIn('raw', retired)
        self.assertNotIn('cpu_user_ns', retired)

    def test_denied_or_unsupported_query_does_not_invent_counters(self):
        for error in (PermissionError(errno.EPERM, 'denied'), OSError('unsupported platform')):
            with self.subTest(error=error), patch('native_process_resources.resource_query', side_effect=error):
                observed = process_resources(os.getpid())
                self.assertEqual(observed['status'], 'unavailable')
                self.assertIn(str(error), observed['error'])
                self.assertNotIn('identity', observed)
                self.assertNotIn('raw', observed)
                self.assertNotIn('cpu_user_ns', observed)
                self.assertGreaterEqual(observed['finished_monotonic_ns'], observed['started_monotonic_ns'])
        with patch('native_process_resources.resource_query', side_effect=InterruptedError('stop')):
            with self.assertRaises(InterruptedError):
                process_resources(os.getpid())


if __name__ == '__main__':
    unittest.main()
