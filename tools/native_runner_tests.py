"""A completed probe must not leave a live child after its leader exits."""
import os
import io
import json
from contextlib import nullcontext, redirect_stderr
from unittest.mock import patch
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest

from native_host_process import execute, signal_owned_group


class ProbeCleanupTests(unittest.TestCase):
    def test_group_signals_retain_the_original_unreaped_child(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder = Path(temporary)
            observations = []

            def checked_signal(group, signum, log):
                # ECHILD would mean this numeric group is no longer pinned by
                # the runner's original child, even if no PID reuse occurred.
                observed = os.waitid(os.P_PID, group, os.WEXITED | os.WNOHANG | os.WNOWAIT)
                self.assertIsNotNone(observed)
                self.assertEqual(observed.si_pid, group)
                observations.append(signum)
                signal_owned_group(group, signum, log)

            with patch('native_host_process.signal_owned_group', side_effect=checked_signal):
                code = execute([sys.executable, '-c', 'raise SystemExit(7)'],
                               folder / 'run.log', os.environ.copy(), 5, folder)
            self.assertEqual(code, 7)
            self.assertEqual(observations, [signal.SIGTERM, signal.SIGKILL])

    def test_uncertain_child_observation_never_signals_the_group(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder = Path(temporary)
            created = []
            create = subprocess.Popen

            def launch(*args, **kwargs):
                child = create(*args, **kwargs)
                created.append(child)
                return child

            try:
                for primary in (None, RuntimeError('original output failure')):
                    with self.subTest(primary=primary), \
                         patch('native_host_process.subprocess.Popen', side_effect=launch), \
                         patch('native_host_process.os.waitid', side_effect=ChildProcessError('child ownership lost')), \
                         patch('native_host_process.OutputLog.check', side_effect=primary), \
                         patch('native_host_process.signal_owned_group') as send:
                        expected = RuntimeError if primary else ChildProcessError
                        message = 'original output failure' if primary else 'child ownership lost'
                        with self.assertRaisesRegex(expected, message) as raised:
                            execute([sys.executable, '-c', 'pass'], folder / 'run.log', os.environ.copy(), 5, folder)
                        send.assert_not_called()
                        report = json.loads((folder / 'run.log.output.json').read_text())
                        self.assertIn('child ownership lost', report['cleanup_error'])
                        if primary:
                            self.assertIs(raised.exception, primary)
                            self.assertIn('cleanup unproven', primary.__notes__[0])
            finally:
                for child in created:
                    child.wait(timeout=5)

    def test_output_flood_is_bounded_and_owned_group_is_retired(self):
        with tempfile.TemporaryDirectory() as temporary:
            folder = Path(temporary)
            log = folder / 'run.log'
            code = 'import os,time; os.write(1,b"prefix"); os.write(2,b"x"*1048576); time.sleep(30)'
            with self.assertRaisesRegex(RuntimeError, 'output limit'):
                execute([sys.executable, '-c', code], log, os.environ.copy(), 5, folder,
                        output_limit=65536)
            report = json.loads(log.with_name('run.log.output.json').read_text())
            self.assertEqual(log.stat().st_size, 65536)
            self.assertGreater(report['observed_bytes'], report['retained_bytes'])
            self.assertTrue(report['eof'])
            self.assertTrue(log.read_bytes().startswith(b'prefix'))

    @unittest.skipUnless(sys.platform == 'darwin', 'Darwin zombie-group signal semantics')
    def test_terminated_group_is_accepted_before_parent_reaps_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / 'run.log'
            child = subprocess.Popen([sys.executable, '-c', 'pass'], start_new_session=True)
            try:
                deadline = time.monotonic() + 2
                while True:
                    state = subprocess.check_output(['ps', '-p', str(child.pid), '-o', 'stat='], text=True).strip()
                    if state.startswith('Z'):
                        break
                    self.assertLess(time.monotonic(), deadline, 'child did not exit')
                    time.sleep(.01)
                # Do not reap yet: a real, owned, zombie-only group causes Darwin EPERM.
                with self.assertRaises(PermissionError):
                    os.killpg(child.pid, signal.SIGTERM)
                signal_owned_group(child.pid, signal.SIGTERM, log)
                signal_owned_group(child.pid, signal.SIGKILL, log)
                reports = [json.loads(line) for line in log.with_name('run-cleanup-signals.jsonl').read_text().splitlines()]
                self.assertTrue(all(row['no_live_members'] for row in reports))
                self.assertTrue(all(row['processes'][0]['pid'] == child.pid for row in reports))
            finally:
                child.wait(timeout=2)

    def test_signal_denial_remains_failure_when_live_or_unknown(self):
        observations = ([dict(state='S')], [dict(state='')], OSError('ps unavailable'))
        with tempfile.TemporaryDirectory() as temporary:
            log = Path(temporary) / 'run.log'
            for observation in observations:
                with self.subTest(observation=observation):
                    denial = PermissionError(1, 'denied')
                    result = dict(side_effect=observation) if isinstance(observation, Exception) else dict(return_value=observation)
                    with patch('native_host_process.os.killpg', side_effect=denial), patch('native_host_process.group_processes', **result):
                        with self.assertRaises(PermissionError) as raised:
                            signal_owned_group(123, signal.SIGTERM, log)
                    self.assertIs(raised.exception, denial)
            reports = [json.loads(line) for line in log.with_name('run-cleanup-signals.jsonl').read_text().splitlines()]
            self.assertTrue(all(not row['no_live_members'] for row in reports))
            with patch('native_host_process.os.killpg', side_effect=PermissionError(1, 'denied')), \
                 patch('native_host_process.group_processes', side_effect=InterruptedError('signal 15')):
                with self.assertRaisesRegex(InterruptedError, 'signal 15'):
                    signal_owned_group(123, signal.SIGTERM, log)

    def test_timeout_removes_child_that_ignores_termination(self):
        self.assert_child_removed(None)

    def test_successful_exit_removes_child_and_preserves_exit_code(self):
        self.assert_child_removed(0)

    def test_nonzero_exit_removes_child_and_preserves_exit_code(self):
        self.assert_child_removed(7)

    def test_diagnostic_failure_preserves_timeout_and_cleanup(self):
        self.assert_child_removed(None, diagnostic_failure=True)

    def test_inventory_timeout_preserves_probe_timeout_and_cleanup(self):
        self.assert_child_removed(None, inventory_failure=True)

    def prepared_probe(self, command, ready):
        create = subprocess.Popen

        def launch(args, **kwargs):
            process = create(args, **kwargs)
            if args is command:
                try:
                    deadline = time.monotonic() + 10
                    while not ready.exists():
                        if process.poll() is not None:
                            raise AssertionError('probe exited before its child was ready')
                        if time.monotonic() >= deadline:
                            raise AssertionError('probe child setup exceeded 10 seconds')
                        time.sleep(.01)
                except BaseException:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                    raise
            return process

        # Fixture setup is separate from the two-second cleanup exercise. The real
        # runner still starts its deadline immediately after Popen returns.
        return patch('native_host_process.subprocess.Popen', side_effect=launch)

    def test_report_storage_failure_retains_inventory_and_cleanup(self):
        self.assert_child_removed(None, report_failure=True)

    def test_unreadable_thermal_output_preserves_inventory_and_cleanup(self):
        self.assert_child_removed(None, thermal_failure=True)

    def assert_child_removed(self, exit_code, diagnostic_failure=False, inventory_failure=False, report_failure=False, thermal_failure=False):
        with tempfile.TemporaryDirectory() as temporary:
            folder = Path(temporary)
            ready = folder / 'ready'
            fallback = io.StringIO()
            if report_failure:
                (folder / 'timeout-processes.json').mkdir()
            child = '''import os,signal,socket,sys,time
from pathlib import Path
signal.signal(signal.SIGTERM, signal.SIG_IGN)
s = socket.socket()
s.bind(('127.0.0.1', 0))
s.listen()
Path(sys.argv[1]).write_text(str(os.getpid()) + ' ' + str(s.getsockname()[1]))
while True: time.sleep(1)
'''
            leader = '''import subprocess,sys,time
from pathlib import Path
subprocess.Popen([sys.executable, '-c', sys.argv[1], sys.argv[2]])
while not Path(sys.argv[2]).exists(): time.sleep(.01)
if sys.argv[3] != 'None': sys.exit(int(sys.argv[3]))
while True: time.sleep(1)
'''
            try:
                command = [sys.executable, '-c', leader, child, str(ready), str(exit_code)]
                run = subprocess.run

                def inventory_timeout(args, **kwargs):
                    if inventory_failure and args[0] == 'ps':
                        raise subprocess.TimeoutExpired(args, kwargs['timeout'])
                    if thermal_failure and args[0] == '/usr/bin/pmset':
                        return run([sys.executable, '-c', "import os; os.write(1, bytes([255]))"], **kwargs)
                    return run(args, **kwargs)

                diagnostics = (patch('native_host_process.capture_timeout', side_effect=OSError('unavailable'))
                               if diagnostic_failure else nullcontext())
                inventory = (patch('native_process_diagnostics.subprocess.run', side_effect=inventory_timeout)
                             if inventory_failure or thermal_failure else nullcontext())
                with self.prepared_probe(command, ready), diagnostics, inventory, redirect_stderr(fallback):
                    if exit_code is None:
                        with self.assertRaisesRegex(RuntimeError, 'Timed out'):
                            execute(command, folder / 'run.log', os.environ.copy(), 2, folder)
                    else:
                        self.assertEqual(execute(command, folder / 'run.log', os.environ.copy(), 2, folder), exit_code)
                self.assertTrue(ready.exists(), 'child must be listening before leader termination')
                pid, port = map(int, ready.read_text().split())
                deadline = time.monotonic() + 2
                while True:
                    with socket.socket() as probe:
                        listening = probe.connect_ex(('127.0.0.1', port)) == 0
                    if not listening or time.monotonic() >= deadline:
                        break
                    time.sleep(0.02)
                self.assertFalse(listening, 'completed probe child still owns its listening socket')
                if exit_code is None and not diagnostic_failure:
                    if report_failure:
                        report = json.loads(fallback.getvalue().splitlines()[-1])
                        self.assertIn('report_error', report)
                        self.assertTrue((folder / 'timeout-processes.json').is_dir())
                    else:
                        report = json.loads((folder / 'timeout-processes.json').read_text())
                    if inventory_failure:
                        self.assertEqual(report['inventory']['state'], 'failed')
                        self.assertEqual(report['inventory']['error_type'], 'TimeoutExpired')
                        self.assertNotIn('members', report['inventory'])
                        self.assertIn('error', report)
                    else:
                        self.assertEqual(report['inventory']['state'], 'complete', report)
                        self.assertIn(pid, [row['pid'] for row in report['processes']], report)
                    self.assertNotIn(os.getpid(), [row['pid'] for row in report['processes']])
                    if thermal_failure:
                        self.assertIn('error', report['host_context']['thermal'])
                        self.assertNotIn('stdout', report['host_context']['thermal'])
                    storage = report['host_context']['storage']
                    self.assertEqual(storage['path'], str(folder))
                    self.assertGreater(storage['total_bytes'], 0)
                    self.assertGreaterEqual(storage['free_bytes'], 0)
            finally:
                if ready.exists():
                    pid = int(ready.read_text().split()[0])
                    try:
                        command = subprocess.check_output(["ps", "-p", str(pid), "-o", "command="], text=True)
                        if str(ready) in command:
                            os.kill(pid, signal.SIGKILL)
                    except (ProcessLookupError, subprocess.CalledProcessError):
                        pass


if __name__ == '__main__':
    unittest.main()
