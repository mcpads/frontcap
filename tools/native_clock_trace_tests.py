"""The timestamp reader owns producer launch and preserves command failure."""
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from native_clock_trace import trace


class NativeClockTraceTests(unittest.TestCase):
    def test_blocked_output_or_evidence_does_not_hold_receipts_or_producer_retirement(self):
        create, open_path = subprocess.Popen, Path.open
        for destination in ('output', 'evidence'):
            with self.subTest(destination=destination), tempfile.TemporaryDirectory() as temporary:
                events = Path(temporary)/'reader.jsonl'
                release = Path(temporary)/'release'
                children, blocked = [], []
                def launch(*args, **kwargs):
                    child = create(*args, **kwargs); children.append(child); return child
                def wait_for_reader():
                    before = time.monotonic_ns()
                    release.touch()
                    deadline = time.monotonic()+2
                    # Only the reader may reap this real producer. Do not poll/wait here.
                    while children[0].returncode is None:
                        if time.monotonic() >= deadline:
                            raise RuntimeError('Forwarding prevented producer retirement')
                        time.sleep(.005)
                    blocked.append((before, time.monotonic_ns()))
                class Output(io.StringIO):
                    def write(self, text):
                        if destination == 'output' and 'START' in text:
                            wait_for_reader()
                        return super().write(text)
                class Evidence:
                    def __init__(self, stream): self.stream = stream
                    def __enter__(self): self.stream.__enter__(); return self
                    def __exit__(self, *args): return self.stream.__exit__(*args)
                    def write(self, text):
                        if '"sequence": 0' in text:
                            wait_for_reader()
                        return self.stream.write(text)
                def open_evidence(path, *args, **kwargs):
                    stream = open_path(path, *args, **kwargs)
                    return Evidence(stream) if destination == 'evidence' and path == events else stream
                output = Output()
                with patch('native_clock_trace.subprocess.Popen', side_effect=launch), \
                     patch('native_clock_trace.Path.open', open_evidence):
                    code = trace([sys.executable, '-u', '-c',
                                  'import pathlib,sys,time; print("START")\n'
                                  'while not pathlib.Path(sys.argv[1]).exists(): time.sleep(.005)\n'
                                  'print("END"); raise SystemExit(7)', str(release)],
                                 output, events=events)
                rows = [json.loads(line) for line in events.read_text().splitlines()]
                receipts = [row for row in rows if row['state'] == 'line']
                self.assertEqual(code, 7)
                self.assertEqual([r['line'] for r in receipts], ['START\n', 'END\n'])
                self.assertLess(blocked[0][0], receipts[1]['received_monotonic_ns'])
                self.assertLess(receipts[1]['received_monotonic_ns'], blocked[0][1])
                self.assertNotEqual(receipts[0]['reader_thread_id'], receipts[0]['thread_id'])
                self.assertTrue(all(r['forwarder_cpu_after_ns'] >= r['forwarder_cpu_before_ns'] for r in receipts))
                self.assertFalse(rows[-1]['reader_alive'])
                self.assertEqual(rows[-1]['received_lines'], rows[-1]['forwarded_lines'])
                self.assertTrue(children[0].stdout.closed)

    def test_buffer_limits_fail_instead_of_silently_dropping_output(self):
        create = subprocess.Popen
        for limit, producer, message in (
                ('MAX_PENDING_LINES', 'print("line\\n"*1000,flush=True)', 'buffer limit'),
                ('MAX_LINE_CHARS', 'print("x"*1000,flush=True)', 'line limit')):
            with self.subTest(limit=limit), tempfile.TemporaryDirectory() as temporary:
                children = []
                def launch(*args, **kwargs):
                    child = create(*args, **kwargs); children.append(child); return child
                class SlowOutput(io.StringIO):
                    def write(self, text):
                        time.sleep(.05)
                        return super().write(text)
                events = Path(temporary)/'reader.jsonl'
                with patch('native_clock_trace.subprocess.Popen', side_effect=launch), \
                     patch('native_clock_trace.'+limit, 2):
                    with self.assertRaisesRegex(RuntimeError, message):
                        trace([sys.executable, '-u', '-c', producer+'; import time; time.sleep(60)'], SlowOutput(), events=events)
                row = json.loads(events.read_text().splitlines()[-1])
                self.assertFalse(row['reader_alive'])
                self.assertIn(message, row['reader_error'])
                self.assertIsNotNone(children[0].returncode)
                self.assertTrue(children[0].stdout.closed)

    def test_incremental_decoding_preserves_newlines_and_final_partial_line(self):
        output = io.StringIO()
        code = trace([sys.executable, '-c',
                      'import os,time; os.write(1,b"A\\r"); time.sleep(.02); '
                      'os.write(1,b"\\n\\xe2"); time.sleep(.02); os.write(1,b"\\x82\\xac\\rZ")'], output)
        self.assertEqual(code, 0)
        self.assertEqual([line.split(' s  ', 1)[1] for line in output.getvalue().splitlines(keepends=True)],
                         ['A\n', '€\n', 'Z'])

    def test_output_failure_cancels_reader_even_when_descendant_keeps_pipe_open(self):
        from native_android_tap import host_processes, retire_process
        with tempfile.TemporaryDirectory() as temporary:
            pid_path = Path(temporary)/'child.pid'
            code = ('import pathlib,subprocess,sys,time; '
                    'child=subprocess.Popen([sys.executable,"-c","import time; time.sleep(60)"]); '
                    'pathlib.Path(sys.argv[1]).write_text(str(child.pid)); print("START",flush=True); time.sleep(60)')
            class BrokenOutput:
                def write(self, text): raise OSError('output unavailable with inherited pipe')
            try:
                with self.assertRaisesRegex(OSError, 'inherited pipe'):
                    trace([sys.executable, '-u', '-c', code, str(pid_path)], BrokenOutput())
                descendants = [p for p in host_processes() if p['pid'] == int(pid_path.read_text())]
                self.assertEqual(len(descendants), 1)
                self.assertNotIn('Z', descendants[0]['state'])
            finally:
                if pid_path.exists():
                    for child in host_processes():
                        if child['pid'] == int(pid_path.read_text()):
                            os.kill(child['pid'], 15)
                            self.assertTrue(retire_process(child)['absent'])

    def test_ready_reader_preserves_intervals_and_child_failure(self):
        # The child emits two independently timed boundaries; startup precedes both.
        output = io.StringIO()
        with tempfile.TemporaryDirectory() as temporary:
            events = Path(temporary)/'reader.jsonl'
            code = trace([sys.executable, '-u', '-c',
                          'import sys,time; print("START"); time.sleep(.2); print("END",file=sys.stderr); sys.exit(7)'],
                         output, events=events)
            rows = [json.loads(line) for line in events.read_text().splitlines()]
        receipts = [row for row in rows if row['state']=='line']
        self.assertEqual([row['line'].strip() for row in receipts], ['START', 'END'])
        self.assertGreaterEqual(receipts[1]['received_monotonic_ns']-receipts[0]['received_monotonic_ns'], 150000000)
        self.assertTrue(all(row['forwarded_monotonic_ns']>=row['received_monotonic_ns'] for row in receipts))
        self.assertEqual(rows[-1]['exit_code'], 7)
        lines = output.getvalue().splitlines()
        self.assertEqual(code, 7)
        self.assertEqual([line.split()[-1] for line in lines], ['START', 'END'])
        self.assertEqual(float(lines[0].split()[0]), 0)
        self.assertGreaterEqual(float(lines[1].split()[0]), .15)

    def test_unavailable_observation_storage_preserves_child_output_and_exit(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = io.StringIO()
            code = trace([sys.executable, '-c', 'print("kept"); raise SystemExit(7)'], output,
                         events=Path(temporary)/'missing/reader.jsonl')
        self.assertEqual(code, 7)
        self.assertTrue(output.getvalue().rstrip().endswith('kept'))

    def test_output_failure_reaps_the_command(self):
        create = subprocess.Popen
        children = []
        def launch(*args, **kwargs):
            child = create(*args, **kwargs)
            children.append(child)
            return child
        class BrokenOutput:
            def write(self, text):
                raise OSError('output unavailable')
        with patch('native_clock_trace.subprocess.Popen', side_effect=launch):
            with self.assertRaisesRegex(OSError, 'output unavailable'):
                trace([sys.executable, '-u', '-c', 'import time; print("START"); time.sleep(60)'], BrokenOutput())
        self.assertIsNotNone(children[0].returncode)
        self.assertTrue(children[0].stdout.closed)


if __name__ == '__main__':
    unittest.main()
