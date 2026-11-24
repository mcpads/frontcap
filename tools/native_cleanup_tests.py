"""Cleanup acceptance uses actual ownership evidence, never successful exit alone."""
import errno
import io
from contextlib import redirect_stderr
import json
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

from native_cleanup import require_retired_native_session, inspect_cleanup, wait_for_android_cleanup, retire_simulator, recover_simulator_cleanup


class NativeCleanupTests(unittest.TestCase):
    def test_explicitly_retired_session_allows_next_worker_without_rewriting_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            record = root/'native-session.json'
            record.write_text(json.dumps(dict(directory='/retained/session', retired=True)))
            before = record.read_bytes()
            require_retired_native_session(root)
            self.assertEqual(record.read_bytes(), before)

    def test_runner_recovery_preserves_first_failure_and_requires_confirmed_survivor(self):
        device = '12345678-1234-1234-1234-123456789ABC'
        original = dict(passed=False, checks=[dict(name='simulator_deleted', passed=False,
                        detail=dict(udid=device, remaining=[dict(udid=device)]))])
        retired = dict(passed=True, checks=[dict(name='simulator_deleted', passed=True,
                       detail=dict(udid=device, remaining=[]))])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root/'shutdown.log').write_text('original shutdown evidence')
            with patch('native_cleanup.inspect_cleanup', side_effect=[original, retired]), \
                 patch('native_cleanup.retire_simulator', return_value=[{'action':'delete','exit_code':0}]) as retire:
                report = recover_simulator_cleanup(root)
            self.assertFalse(report['passed'])
            self.assertFalse(report['checks'][0]['passed'])
            self.assertTrue(report['recovery']['observation']['passed'])
            self.assertEqual(retire.call_args.args, (device, root/'runner-cleanup'))
            self.assertTrue(callable(retire.call_args.kwargs['progress']))
            self.assertEqual((root/'shutdown.log').read_text(), 'original shutdown evidence')
            for observation in (retired, dict(passed=False, checks=[dict(name='cleanup_observed', passed=False, detail='inventory unavailable')])):
                with patch('native_cleanup.inspect_cleanup', return_value=observation), \
                     patch('native_cleanup.retire_simulator') as retire:
                    self.assertEqual(recover_simulator_cleanup(root), observation)
                retire.assert_not_called()

    def test_interrupted_recovery_retains_initial_failure_and_unknown_delete_effect(self):
        from native_host_process import WorkerInterrupted
        device = '12345678-1234-1234-1234-123456789ABC'
        initial = dict(passed=False, checks=[dict(name='simulator_deleted', passed=False,
                       detail=dict(udid=device, remaining=[dict(udid=device)]))])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            def command(args, **kwargs):
                saved = json.loads((root/'runner-cleanup.json').read_text())
                self.assertFalse(saved['passed'])
                self.assertEqual(saved['checks'], initial['checks'])
                self.assertEqual(saved['recovery']['actions'][-1]['state'], 'started')
                if args[2] == 'delete':
                    raise WorkerInterrupted('signal during delete')
                return subprocess.CompletedProcess(args, 0, stdout='shutdown complete', stderr='')
            with patch('native_cleanup.inspect_cleanup', return_value=initial), \
                 patch('native_cleanup.subprocess.run', side_effect=command), \
                 self.assertRaises(WorkerInterrupted):
                recover_simulator_cleanup(root)
            saved = json.loads((root/'runner-cleanup.json').read_text())
            self.assertFalse(saved['passed'])
            self.assertEqual(saved['recovery']['state'], 'aborted')
            actions = saved['recovery']['actions']
            self.assertEqual(actions[0]['exit_code'], 0)
            self.assertEqual(actions[1]['action'], 'delete')
            self.assertEqual(actions[1]['state'], 'started')
            self.assertNotIn('exit_code', actions[1])
            self.assertNotIn('observation', saved['recovery'])

    def test_simulator_delete_survives_shutdown_and_log_storage_failure(self):
        device = '12345678-1234-1234-1234-123456789ABC'
        for failure in ('storage', 'timeout', 'launch', 'exit'):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                if failure == 'storage':
                    (root/'shutdown.log').mkdir()
                    (root/'delete.log').mkdir()
                commands = []
                def run(command, **kwargs):
                    commands.append(command)
                    self.assertEqual(kwargs['timeout'], 20)
                    if command[2] == 'shutdown':
                        if failure == 'timeout': raise subprocess.TimeoutExpired(command, 20)
                        if failure == 'launch': raise OSError('unavailable')
                    return subprocess.CompletedProcess(command, 7 if failure == 'exit' and command[2] == 'shutdown' else 0,
                                                       stdout='retained output', stderr='')
                fallback = io.StringIO()
                with patch('native_cleanup.subprocess.run', side_effect=run), redirect_stderr(fallback):
                    records = retire_simulator(device, root)
                self.assertEqual(commands, [['xcrun', 'simctl', action, device] for action in ('shutdown', 'delete')])
                self.assertEqual(records[-1]['exit_code'], 0)
                if failure == 'storage':
                    saved = [json.loads(line.split(': ', 1)[1]) for line in fallback.getvalue().splitlines()]
                    self.assertEqual(saved, records)
                    self.assertTrue(all('report_error' in row for row in saved))
                    self.assertTrue((root/'shutdown.log').is_dir())
                else:
                    self.assertEqual((root/'delete.log').read_text(), 'retained output')
        with patch('native_cleanup.subprocess.run') as run, self.assertRaises(ValueError):
            retire_simulator('all', '.')
        run.assert_not_called()

    def test_android_retirement_waits_for_real_resources_and_keeps_live_resources_failed(self):
        child_code = '''import socket,sys,time
s=socket.socket();s.bind(('127.0.0.1',0));s.listen()
print(s.getsockname()[1],flush=True)
sys.stdin.readline();time.sleep(.05)
'''
        for retire in (True, False):
            with self.subTest(retire=retire), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                child = subprocess.Popen([sys.executable, '-u', '-c', child_code],
                                         stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
                reaper = threading.Thread(target=child.wait)
                reaper.start()
                try:
                    import select
                    self.assertTrue(select.select([child.stdout], [], [], 5)[0], 'listener startup timed out')
                    port = int(child.stdout.readline())
                    self.assertLess(port, 65535)
                    (root / 'emulator-pid').write_text(str(child.pid))
                    (root / 'emulator-port').write_text(str(port))
                    released = False
                    def observe(*args, **kwargs):
                        nonlocal released
                        result = inspect_cleanup(*args, **kwargs)
                        if retire and not released:
                            self.assertFalse(result['passed'])
                            child.stdin.write('retire\n'); child.stdin.flush()
                            released = True
                        return result
                    with patch('native_cleanup.inspect_cleanup', side_effect=observe):
                        result = wait_for_android_cleanup(root, timeout=2 if retire else .15)
                    self.assertEqual(result['passed'], retire)
                    self.assertFalse(result['observations'][0]['passed'])
                    self.assertEqual(result['state'], 'retired' if retire else 'deadline')
                    if retire:
                        self.assertTrue(all(c['passed'] for c in result['checks']))
                    else:
                        self.assertIsNone(child.poll())
                        with socket.socket() as probe:
                            self.assertEqual(probe.connect_ex(('127.0.0.1', port)), 0)
                finally:
                    if child.poll() is None:
                        child.kill()
                    reaper.join(timeout=5)
                    child.stdin.close(); child.stdout.close()
                    self.assertFalse(reaper.is_alive())

    def test_android_retirement_does_not_retry_an_unknown_inventory(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'emulator-pid').write_text('12345')
            (root / 'emulator-port').write_text('5580')
            with patch('native_cleanup.subprocess.run', side_effect=OSError('inventory unavailable')):
                result = wait_for_android_cleanup(root)
            self.assertFalse(result['passed'])
            self.assertEqual(result['state'], 'unavailable')
            self.assertEqual(len(result['observations']), 1)
            self.assertIn('inventory unavailable', result['checks'][0]['detail'])

    def test_ios_requires_owned_device_absence_not_an_empty_machine(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            owned = "9D0802F9-0D9F-4398-8113-52275FAB9E89"
            (root / "simulator-id").write_text(owned)
            for present in (True, False):
                devices = [dict(udid="9D0802F9-0D9F-4398-8113-52275FAB9E88", state="Booted")]
                if present:
                    devices.append(dict(udid=owned, state="Shutdown"))
                with self.subTest(present=present), patch("native_cleanup.subprocess.run", return_value=
                        subprocess.CompletedProcess([], 0, json.dumps(dict(devices=dict(runtime=devices))), "")):
                    result = inspect_cleanup("ios", root)
                self.assertEqual(result["passed"], not present)

    def test_missing_identity_and_failed_inventory_cannot_pass(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for platform in ("ios", "android"):
                self.assertFalse(inspect_cleanup(platform, root)["passed"])
            (root / "simulator-id").write_text("9D0802F9-0D9F-4398-8113-52275FAB9E89")
            with patch("native_cleanup.subprocess.run", side_effect=subprocess.TimeoutExpired("simctl", 5)):
                self.assertFalse(inspect_cleanup("ios", root)["passed"])

    def test_android_listener_prevents_success_even_after_process_disappears(self):
        with tempfile.TemporaryDirectory() as temporary, socket.socket() as listener:
            root = Path(temporary)
            listener.bind(("127.0.0.1", 0))
            listener.listen()
            (root / "emulator-pid").write_text("12345")
            port = listener.getsockname()[1]
            (root / "emulator-port").write_text(str(min(port, 65534)))
            with patch("native_cleanup.subprocess.run", return_value=subprocess.CompletedProcess([], 1, "", "")):
                result = inspect_cleanup("android", root)
            self.assertFalse(result["passed"])
            self.assertTrue(result["checks"][0]["passed"])
            observed = next(row for row in result["checks"] if row["detail"].get("port") == port)
            self.assertFalse(observed["passed"])
            self.assertEqual(observed["detail"]["connect_result"], 0)

    def test_android_requires_process_absence_and_refused_ports(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "emulator-pid").write_text("12345")
            (root / "emulator-port").write_text("5580")
            for code, output, error, connection, expected in (
                (0, "12345 S owned-emulator", "", errno.ECONNREFUSED, False),
                (1, "", "", errno.ETIMEDOUT, False),
                (1, "", "ps: process query failed", errno.ECONNREFUSED, False),
                (1, "", "", errno.ECONNREFUSED, True),
            ):
                with self.subTest(code=code, connection=connection), \
                     patch("native_cleanup.subprocess.run", return_value=subprocess.CompletedProcess([], code, output, error)), \
                     patch("native_cleanup.socket.socket") as create:
                    create.return_value.__enter__.return_value.connect_ex.return_value = connection
                    result = inspect_cleanup("android", root)
                self.assertEqual(result["passed"], expected)


if __name__ == "__main__":
    unittest.main()
