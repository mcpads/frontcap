"""Prelaunch authority and failure boundaries; no SDK/device acquisition."""
import json
from pathlib import Path
import queue
import tempfile
import time
import unittest

from flutter_prelaunch import wait_for_initial_data
from native_host_process import WorkerInterrupted, write_json


class Controls:
    def __init__(self, steps):
        self.steps = iter(steps)

    def get(self, timeout):
        assert timeout > 0
        return next(self.steps)()


class PrelaunchTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)/'prelaunch'
        self.installation = dict(device='owned', app='org.example.client', inode=12)
        self.calls = []

    def identity(self):
        return json.loads((self.directory/'ready.json').read_text())

    def acknowledge(self, number=0):
        identity = self.identity()
        write_json(self.directory/'applied.json', dict(identity, number=number))
        return dict(command='initial_data_applied', token=identity['token'], number=number)

    def request(self, number=1):
        destination = self.directory/str(number)
        destination.mkdir()
        write_json(destination/'request.json', dict(number=number,
                   arguments=['shell', 'owned-file-application'], timeout_ms=500))
        return dict(command='initial_data_command', token=self.identity()['token'], number=number)

    def execute(self, arguments, directory, timeout):
        self.calls.append(arguments)
        self.assertGreater(timeout, 0)
        self.assertLessEqual(timeout, .5)
        return b'checked bytes\n'

    def run_gate(self, steps, execute=None, verify=None, deadline=None):
        wait_for_initial_data(Controls(steps), self.directory, self.installation,
                              deadline or time.monotonic()+5,
                              execute or self.execute, verify or (lambda: self.installation))

    def test_receipt_alone_cannot_release_then_live_ack_checks_installation(self):
        checks = []

        def verify():
            checks.append(True)
            return self.installation

        def first():
            # A file left by the controller is not permission to launch.
            self.acknowledge()
            self.assertFalse((self.directory/'completed.json').exists())
            return self.request()

        def last():
            self.assertEqual((self.directory/'1/command.log').read_bytes(), b'checked bytes\n')
            self.assertFalse((self.directory/'completed.json').exists())
            return self.acknowledge(1)

        self.run_gate([first, last], verify=verify)
        self.assertEqual(len(checks), 2)
        self.assertEqual(len(self.calls), 1)
        self.assertEqual(json.loads((self.directory/'completed.json').read_text()),
                         dict(self.identity(), number=1))

    def test_uncertain_command_is_terminal_without_consuming_later_ack(self):
        def failed(*args):
            (self.directory/'effect').write_text('may have happened')
            raise RuntimeError('SDK response lost')

        with self.assertRaisesRegex(RuntimeError, 'response lost'):
            self.run_gate([self.request, lambda: self.fail('consumed command after failure')], execute=failed)
        self.assertTrue((self.directory/'effect').exists())
        self.assertIn('response lost', (self.directory/'1/reply.json').read_text())
        self.assertFalse((self.directory/'completed.json').exists())

    def test_changed_installation_cannot_launch(self):
        identities = iter([self.installation, dict(self.installation, inode=13)])
        with self.assertRaisesRegex(ValueError, 'changed after'):
            self.run_gate([self.acknowledge], verify=lambda: next(identities))
        self.assertFalse((self.directory/'completed.json').exists())

    def test_stop_deadline_and_foreign_ack_do_not_release(self):
        cases = [({'command': 'stop'}, WorkerInterrupted),
                 (dict(command='initial_data_applied', token='other-session', number=0), ValueError),
                 (dict(command='initial_data_command', token='other-session', number=1), ValueError)]
        for index, (notification, error) in enumerate(cases):
            with self.subTest(notification=notification):
                self.directory = self.directory.parent/str(index)
                with self.assertRaises(error):
                    self.run_gate([lambda: notification])
                self.assertFalse((self.directory/'completed.json').exists())
        self.directory = self.directory.parent/'expired'
        with self.assertRaises(TimeoutError):
            self.run_gate([], deadline=time.monotonic()-1)
        self.assertFalse((self.directory/'ready.json').exists())
        self.directory = self.directory.parent/'missing'

        def missing():
            raise queue.Empty

        with self.assertRaises(TimeoutError):
            self.run_gate([missing])
        self.assertFalse((self.directory/'completed.json').exists())

    def test_sequence_and_receipt_must_match_completed_commands(self):
        for case in ['skip', 'stale', 'missing']:
            with self.subTest(case=case):
                self.directory = self.directory.parent/case

                def invalid():
                    if case == 'skip':
                        return self.request(2)
                    notification = self.acknowledge()
                    if case == 'missing':
                        (self.directory/'applied.json').unlink()
                    else:
                        write_json(self.directory/'applied.json', dict(self.identity(), number=1))
                    return notification

                with self.assertRaises((ValueError, FileNotFoundError)):
                    self.run_gate([invalid])
                self.assertFalse((self.directory/'completed.json').exists())
        self.assertEqual(self.calls, [])
