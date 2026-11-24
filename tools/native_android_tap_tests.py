"""Tap diagnostics retain actual exit status and distinguish process reuse from state changes."""
import json
import itertools
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest.mock import patch

from native_android_tap import host_processes, matching_processes, retire_process, run_tap, run_suspended_tap, wait_for_exit


class AndroidTapTests(unittest.TestCase):
    def test_live_child_with_changed_command_is_not_mistaken_for_exit_or_signalled(self):
        identity = dict(pid=41, ppid=40, group=39, started='original start', state='S', command='(adb)')
        current = {**identity, 'command': '/sdk/platform-tools/adb shell run-as fixture cat files/receipt'}
        with patch('native_android_tap.host_processes', return_value=[current]), \
             patch('native_android_tap.time.monotonic', side_effect=lambda: next(ticks)), \
             patch('native_android_tap.os.kill') as kill:
            ticks = itertools.count(0, .5)
            result = retire_process(identity)
        self.assertFalse(result['absent'])
        self.assertTrue(result['observations'])
        self.assertTrue(all(row['processes'] == [current] for row in result['observations']))
        kill.assert_not_called()

    def test_exit_inventory_uses_remaining_budget_and_rejects_late_absence(self):
        identity = dict(pid=41, started='Mon Oct 12 12:00:00 2026')
        for outcome in ('on-time', 'late', 'timeout', 'cancelled'):
            with self.subTest(outcome=outcome):
                clock = [10.0]
                observations, budgets = [], []
                failure = InterruptedError('cancel inventory')
                def inventory(argv, **options):
                    budgets.append(options['timeout'])
                    if len(budgets) == 1:
                        clock[0] += .5
                        return subprocess.CompletedProcess(argv, 0,
                            f"41 40 39 S {identity['started']} adb input tap", '')
                    clock[0] += .25 if outcome == 'on-time' else 2
                    if outcome == 'timeout':
                        raise subprocess.TimeoutExpired(argv, options['timeout'])
                    if outcome == 'cancelled':
                        raise failure
                    return subprocess.CompletedProcess(argv, 0, '', '')
                with patch('native_android_tap.time.monotonic', side_effect=lambda: clock[0]), \
                     patch('native_android_tap.time.sleep', side_effect=lambda value: clock.__setitem__(0, clock[0]+value)), \
                     patch('native_android_tap.subprocess.run', side_effect=inventory):
                    if outcome == 'cancelled':
                        with self.assertRaises(InterruptedError) as raised:
                            wait_for_exit(identity, observations)
                        self.assertIs(raised.exception, failure)
                    else:
                        self.assertEqual(wait_for_exit(identity, observations), outcome == 'on-time')
                self.assertEqual(len(budgets), 2)
                self.assertAlmostEqual(budgets[0], 2)
                self.assertAlmostEqual(budgets[1], 1.48)
                self.assertEqual(len(observations), 2)
                if outcome in ('timeout', 'cancelled'):
                    self.assertIn('error', observations[-1])
                else:
                    self.assertEqual(observations[-1]['processes'], [])

    def test_process_identity_allows_state_changes_but_rejects_reuse(self):
        identity = dict(pid=41, ppid=40, group=39, started='original start', state='R', command='adb input tap')
        for state, command in (('S', identity['command']), ('Z', '<defunct>')):
            row = {**identity, 'ppid': 1, 'state': state, 'command': command}
            self.assertEqual(matching_processes([row], identity), [row])
        for field, value in (('pid', 42), ('group', 40), ('started', 'later start'), ('command', 'other command')):
            with self.subTest(field=field):
                self.assertEqual(matching_processes([{**identity, field: value}], identity), [])

    @unittest.skipUnless(sys.platform == 'darwin', 'Darwin suspended tap launch')
    def test_suspended_transport_records_actual_signal_failure_without_running_child(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            state = dict(pending=dict(request_id=1), delivery_phase='dispatched')
            (directory/'input-client.json').write_text(json.dumps(state))
            effect = directory/'effect'
            observed, errors = [], []
            def interrupt():
                try:
                    deadline = time.monotonic()+2
                    while time.monotonic() < deadline:
                        path = directory/'input-transport.json'
                        if path.exists():
                            row = json.loads(path.read_text())
                            if row['phase'] == 'suspended':
                                observed.append(row)
                                os.kill(row['pid'], 9)
                                return
                        time.sleep(.01)
                    raise RuntimeError('Suspended child unavailable')
                except BaseException as error: errors.append(error)
            observer = threading.Thread(target=interrupt)
            observer.start()
            try:
                with self.assertRaises(subprocess.CalledProcessError) as failure:
                    run_suspended_tap([sys.executable,'-c',
                                      'import pathlib,sys; pathlib.Path(sys.argv[1]).touch()',str(effect)], directory)
                self.assertEqual(failure.exception.returncode,-9)
            finally:
                observer.join(2)
            self.assertFalse(observer.is_alive())
            self.assertFalse(errors)
            self.assertEqual(len(observed),1)
            row = json.loads((directory/'input-transport.json').read_text())
            self.assertEqual(row['exit_code'],-9)
            self.assertEqual(row['identity'],observed[0]['identity'])
            self.assertEqual(row['phase'],'finished')
            self.assertFalse(effect.exists())
            self.assertEqual(json.loads((directory/'input-client.json').read_text()),state)
            self.assertFalse(matching_processes(host_processes(),row['identity']))

    def test_observer_records_real_child_success_and_failure_then_reaps_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            request = dict(request_id=1, generation='test generation')
            (directory/'input-client.json').write_text(json.dumps(dict(pending=request)))
            for code in (0, 7):
                command = [sys.executable, '-c', f'import sys; print("tap"); sys.exit({code})']
                with self.subTest(exit_code=code):
                    if code:
                        with self.assertRaises(subprocess.CalledProcessError) as failure:
                            run_tap(command, directory)
                        self.assertEqual(failure.exception.returncode, code)
                    else:
                        self.assertEqual(run_tap(command, directory), b'tap\n')
                    row = json.loads((directory/'input-transport.json').read_text())
                    self.assertEqual(row['request'], request)
                    self.assertEqual(row['command'], command)
                    self.assertEqual((row['phase'], row['exit_code'], row['parent_pid']), ('finished', code, os.getpid()))
                    self.assertLess(row['started_monotonic_ns'], row['finished_monotonic_ns'])
                    self.assertEqual(row['identity']['pid'], row['pid'])
                    self.assertEqual(matching_processes(host_processes(), row['identity']), [])


if __name__ == '__main__':
    unittest.main()
