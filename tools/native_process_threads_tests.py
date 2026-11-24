"""Thread evidence must preserve partial acquisition and process identity limits."""
import ctypes
import errno
import os
import sys
import time
import unittest
from unittest.mock import patch

from native_process_resources import process_resources
from native_process_threads import ThreadInfo, process_threads


class ProcessThreadsTests(unittest.TestCase):
    @unittest.skipUnless(sys.platform == 'darwin', 'macOS thread observations required')
    def test_current_thread_identity_and_cpu_units(self):
        identity = process_resources(os.getpid())['identity']
        first = process_threads(os.getpid(), identity, deadline=time.monotonic()+1)
        start = time.thread_time_ns()
        while time.thread_time_ns() - start < 20_000_000:
            pass
        last = process_threads(os.getpid(), identity, deadline=time.monotonic()+1)
        self.assertEqual((first['status'], last['status']), ('available', 'available'))
        system = ctypes.CDLL('/usr/lib/libSystem.B.dylib')
        tid = ctypes.c_uint64()
        self.assertEqual(system.pthread_threadid_np(None, ctypes.byref(tid)), 0)
        rows = [next(row for row in snapshot['threads'] if row['thread_id'] == tid.value)
                for snapshot in (first, last)]
        cpu = [row['raw']['pth_user_time'] + row['raw']['pth_system_time'] for row in rows]
        self.assertGreaterEqual(cpu[1] - cpu[0], 18_000_000)
        self.assertLessEqual(cpu[1] - cpu[0], time.thread_time_ns() - start + 2_000_000)
        self.assertTrue(all(snapshot['identity_verified'] for snapshot in (first, last)))

    def test_unavailable_partial_and_changed_identity_cannot_be_complete(self):
        identity = dict(pid=42, start_abstime=100, executable_uuid='fixture')
        for mode in ('complete', 'denied', 'unsupported', 'retired_thread', 'short', 'overflow', 'duplicate', 'limit', 'identity', 'deadline'):
            with self.subTest(mode=mode):
                def query(pid, flavor, tid, pointer, size):
                    if mode in ('denied', 'unsupported'):
                        ctypes.set_errno(errno.EPERM if mode == 'denied' else errno.EINVAL)
                        return 0
                    if flavor == 28:
                        ids = ctypes.cast(pointer, ctypes.POINTER(ctypes.c_uint64))
                        ids[0], ids[1] = 1001, (1001 if mode == 'duplicate' else 1002)
                        return size+8 if mode == 'overflow' else 16
                    if mode == 'retired_thread' and tid == 1002:
                        ctypes.set_errno(errno.ESRCH); return 0
                    info = ctypes.cast(pointer, ctypes.POINTER(ThreadInfo)).contents
                    info.pth_run_state = 3
                    return size-1 if mode == 'short' else size
                after = dict(status='available', identity=dict(identity, start_abstime=101) if mode == 'identity' else identity)
                with patch('native_process_threads.thread_query', return_value=query), \
                     patch('native_process_threads.process_resources', return_value=after):
                    report = process_threads(42, identity, deadline=time.monotonic()+(-1 if mode == 'deadline' else 1),
                                             limit=1 if mode == 'limit' else 64)
                self.assertEqual(report['status'] == 'available', mode == 'complete', report)
                if mode in ('denied', 'unsupported', 'overflow', 'duplicate', 'deadline'):
                    self.assertEqual(report['threads'], [])
                    self.assertFalse(report['identity_verified'])
                if mode == 'identity': self.assertFalse(report['identity_verified'])
                if mode == 'limit': self.assertTrue(report['truncated'])
                if mode in ('retired_thread', 'short'):
                    unavailable = [row for row in report['threads'] if row['status'] == 'unavailable']
                    self.assertTrue(unavailable)
                    self.assertTrue(all('raw' not in row for row in unavailable))

    def test_interruption_propagates_without_inventing_thread_data(self):
        with patch('native_process_threads.thread_query', side_effect=InterruptedError('stop')):
            with self.assertRaisesRegex(InterruptedError, 'stop'):
                process_threads(42, dict(pid=42,start_abstime=100), deadline=time.monotonic()+1)

    def test_budget_expiry_keeps_rows_unverified_and_stops_new_queries(self):
        clock = [0.0]; calls = []
        def query(pid, flavor, tid, pointer, size):
            calls.append((flavor, tid))
            if flavor == 28:
                ids = ctypes.cast(pointer, ctypes.POINTER(ctypes.c_uint64))
                ids[0], ids[1] = 1001, 1002
                return 16
            clock[0] = 1.0
            return size
        with patch('native_process_threads.thread_query', return_value=query), \
             patch('native_process_threads.time.monotonic', side_effect=lambda: clock[0]), \
             patch('native_process_threads.process_resources', side_effect=AssertionError('query after deadline')):
            result = process_threads(42, dict(pid=42,start_abstime=100), deadline=.5)
        self.assertEqual(calls, [(28, 0), (15, 1001)])
        self.assertTrue(result['budget_exhausted'])
        self.assertFalse(result['identity_verified'])
        self.assertEqual(result['status'], 'partial')
        self.assertEqual(len(result['threads']), 1)


if __name__ == '__main__':
    unittest.main()
