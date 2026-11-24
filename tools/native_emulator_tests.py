"""Ownership checks for the shared Python emulator lifecycle."""
import os
from contextlib import nullcontext
import json
from pathlib import Path
import socket
import tempfile
import time
import unittest
from unittest.mock import patch

from native_emulator import OwnedEmulator


class EmulatorOwnershipTests(unittest.TestCase):
    def test_owned_live_process_retires_and_releases_both_ports(self):
        self.run_owned_process(False)

    def test_replaced_temporary_directory_fails_cleanup_without_touching_foreign_files(self):
        self.run_owned_process(True)

    def test_output_overflow_still_retires_writer_ports_and_disks(self):
        self.run_owned_process(False, flood=True)

    def test_cleanup_inspection_failure_still_finishes_output(self):
        self.run_owned_process(False, inspection_failure=True)

    def run_owned_process(self, replace_directory, flood=False, inspection_failure=False):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            sdk, out = root/'sdk', root/'out'
            (sdk/'emulator').mkdir(parents=True)
            (sdk/'platform-tools').mkdir()
            out.mkdir()
            emulator = sdk/'emulator/emulator'
            emulator.write_text('''#!/usr/bin/env python3
import socket,sys,time,os
from pathlib import Path
scratch=Path(os.environ['ANDROID_TMP'])
(scratch/'overlay.qcow2').write_bytes(b'owned disk')
port=int(sys.argv[sys.argv.index('-port')+1])
sockets=[]
for p in (port,port+1):
 s=socket.socket();s.bind(('127.0.0.1',p));s.listen();sockets.append(s)
if os.environ.get('EMULATOR_FLOOD'):
 os.write(1,b'x'*(5*1024*1024))
 (scratch/'flood-complete').touch()
time.sleep(60)
''')
            emulator.chmod(0o755)
            adb = sdk/'platform-tools/adb'
            adb.write_text('#!/bin/sh\necho 1\n')
            adb.chmod(0o755)
            # Reserve a free adjacent pair for this isolated process test.
            for port in range(18000, 19000, 2):
                try:
                    with socket.socket() as a, socket.socket() as b:
                        a.bind(('127.0.0.1', port)); b.bind(('127.0.0.1', port+1))
                    break
                except OSError:
                    continue
            shared = root/'shared-avd'
            shared.mkdir()
            (shared/'base.qcow2').write_bytes(b'shared backing')
            env = dict(os.environ, ANDROID_TMP=str(shared))
            if flood:env['EMULATOR_FLOOD']='1'
            target = OwnedEmulator(sdk, out, env, avd='fixture', port=port)
            try:
                target.start()
                deadline = time.monotonic()+2
                while True:
                    with socket.socket() as a, socket.socket() as b:
                        ready = (a.connect_ex(('127.0.0.1', port)) == 0 and
                                 b.connect_ex(('127.0.0.1', port+1)) == 0)
                    if ready:
                        break
                    self.assertLess(time.monotonic(), deadline)
                    time.sleep(.01)
                if flood:
                    while not (Path(target.temporary['path'])/'flood-complete').exists():
                        self.assertLess(time.monotonic(), deadline)
                        time.sleep(.01)
                    while not target.log_capture.failed.is_set():
                        self.assertLess(time.monotonic(), deadline)
                        time.sleep(.01)
                    with self.assertRaisesRegex(RuntimeError, 'output limit exceeded'):
                        target.wait_for_boot(timeout=2)
                else:
                    target.wait_for_boot(timeout=2)
                self.assertIsNone(target.process.poll())
                temporary = Path(target.temporary['path'])
                self.assertEqual((temporary/'overlay.qcow2').read_bytes(), b'owned disk')
                (temporary/'backing-link').symlink_to(shared, target_is_directory=True)
                if replace_directory:
                    temporary.rename(root/'displaced-temporary')
                    temporary.symlink_to(shared, target_is_directory=True)
            finally:
                with (patch('native_emulator.wait_for_android_cleanup', side_effect=OSError('inspection failed'))
                      if inspection_failure else nullcontext()):
                    cleanup = target.retire()
            self.assertEqual(cleanup['passed'], not (replace_directory or flood or inspection_failure), cleanup)
            output = json.loads((out/'emulator.log.output.json').read_text())
            self.assertTrue(output['eof'])
            self.assertFalse(target.log_capture.thread.is_alive())
            self.assertLessEqual(target.log.stat().st_size, 4*1024*1024)
            if flood:
                self.assertEqual(output['observed_bytes'], 5*1024*1024)
                self.assertEqual(target.log.read_bytes(), b'x'*(4*1024*1024))
            self.assertEqual(target.retire(), cleanup)
            self.assertIsNotNone(target.process.returncode)
            self.assertEqual(env['ANDROID_TMP'], str(shared))
            self.assertEqual((shared/'base.qcow2').read_bytes(), b'shared backing')
            if inspection_failure:
                self.assertTrue(temporary.is_dir())
                self.assertIn('inspection failed', str(cleanup))
            elif replace_directory:
                self.assertTrue(temporary.is_symlink())
            else:
                self.assertFalse(temporary.exists())
                report = json.loads((out/'emulator-temporary-cleanup.json').read_text())
                self.assertTrue(report['removed'])
                self.assertTrue(any(row['path'] == 'overlay.qcow2' for row in report['entries']))

    def test_lost_launch_handle_cannot_claim_retirement(self):
        with tempfile.TemporaryDirectory() as temporary:
            target = OwnedEmulator(Path(temporary), Path(temporary), dict(os.environ),
                                   avd='fixture', port=19880)
            with patch('native_emulator.subprocess.Popen', side_effect=InterruptedError):
                with self.assertRaises(InterruptedError):
                    target.start()
            self.assertFalse(target.retire()['passed'])
            self.assertTrue(Path(target.temporary['path']).is_dir())

    def test_occupied_port_never_acquires_or_signals_a_process(self):
        with tempfile.TemporaryDirectory() as temporary, socket.socket() as listener:
            listener.bind(('127.0.0.1', 0)); listener.listen()
            port = listener.getsockname()[1]
            console = port if port % 2 == 0 else port-1
            target = OwnedEmulator(Path(temporary), Path(temporary), dict(os.environ),
                                   avd='fixture', port=console)
            with self.assertRaises(OSError):
                target.start()
            self.assertIsNone(target.process)
            self.assertTrue(target.retire()['passed'])
            self.assertEqual(listener.getsockname()[1], port)


if __name__ == '__main__':
    unittest.main()
