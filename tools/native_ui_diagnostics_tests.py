import json
import io
import os
from pathlib import Path
import tempfile
import unittest
import subprocess
import sys
from unittest.mock import Mock, patch

from native_ui_diagnostics import capture_sample, failed_advances, owned_app, sampled_threads, host_context, watch


class NativeUiDiagnosticsTests(unittest.TestCase):
    def sampler(self, directory, mode):
        script = Path(directory) / 'sampler.py'
        script.write_text('''import json,pathlib,sys,time
mode=sys.argv[1];prefix=pathlib.Path(sys.argv[2])
if mode=='delayed':time.sleep(.15)
print('Sampling process 123 for 1 second with 10 milliseconds of run time between samples',flush=True)
print('Sampling completed, processing symbols...',flush=True)
if mode=='invalid':
    prefix.with_suffix('.sample.txt').write_bytes(b'Call graph:\\n    3 Thread_42\\n\\xff')
    sys.exit(7)
if mode in ('error','success'):
    prefix.with_suffix('.sample.txt').write_text('Call graph:\\n    3 Thread_42\\n      3 blocked\\n')
    sys.exit(7 if mode=='error' else 0)
while True:
    events=prefix.with_suffix('.events.jsonl')
    if events.exists() and 'processing_symbols' in events.read_text():
        prefix.with_suffix('.observed').touch()
    resources=prefix.with_suffix('.resources.jsonl')
    if resources.exists() and resources.read_text().strip():
        prefix.with_suffix('.resources-observed').touch()
    time.sleep(.02)
''')
        prefix = Path(directory) / 'capture'
        create = subprocess.Popen
        children = []

        def launch(args, **kwargs):
            if args[0] == '/usr/bin/sample':
                child = create([sys.executable, str(script), mode, str(prefix)], **kwargs)
                children.append(child)
                return child
            return create(args, **kwargs)

        return prefix, children, patch('native_ui_diagnostics.subprocess.Popen', side_effect=launch)

    def test_retirement_progress_is_durable_before_signal_and_reap(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix, children, launch = self.sampler(directory, 'hang')
            observed = []
            with launch as factory:
                create = factory.side_effect
                def traced_launch(args, **kwargs):
                    child = create(args, **kwargs)
                    if args[0] == '/usr/bin/sample':
                        kill, wait = child.kill, child.wait
                        def last_phase():
                            return json.loads(prefix.with_suffix('.events.jsonl').read_text().splitlines()[-1])['phase']
                        def traced_kill():
                            observed.append(('kill', last_phase()))
                            return kill()
                        def traced_wait(timeout=None):
                            if timeout is None: observed.append(('reap', last_phase()))
                            return wait(timeout=timeout)
                        child.kill, child.wait = traced_kill, traced_wait
                    return child
                factory.side_effect = traced_launch
                report = capture_sample(os.getpid(), prefix, .3)
            self.assertEqual(observed, [('kill', 'retirement_requested'), ('reap', 'retirement_signalled')])
            rows = [json.loads(line) for line in prefix.with_suffix('.events.jsonl').read_text().splitlines()]
            events = [row for row in rows if row['phase'].startswith('retirement_')]
            self.assertEqual([r['phase'] for r in events], ['retirement_requested', 'retirement_signalled', 'retirement_reaped'])
            self.assertEqual([r['host_monotonic_ns'] for r in events], sorted(r['host_monotonic_ns'] for r in events))
            self.assertTrue(all(r['sampler_pid'] == children[0].pid for r in events))
            self.assertEqual(events[-1]['exit_code'], -9)
            self.assertEqual(report['exit_code'], -9)
            self.assertIsNotNone(children[0].poll())

    def test_retirement_event_interruption_still_reaps_owned_child(self):
        for phase in ('retirement_requested', 'retirement_signalled'):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as directory:
                prefix, children, launch = self.sampler(directory, 'hang')
                encode = json.dumps
                def interrupted(value, *args, **kwargs):
                    if isinstance(value, dict) and value.get('phase') == phase:
                        raise InterruptedError('retirement publication interrupted')
                    return encode(value, *args, **kwargs)
                with launch, patch('native_ui_diagnostics.json.dumps', side_effect=interrupted):
                    with self.assertRaises(InterruptedError): capture_sample(os.getpid(), prefix, .3)
                self.assertEqual(len(children), 1)
                self.assertEqual(children[0].poll(), -9)

    def test_timeout_keeps_progress_and_reaps_only_the_sampler(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix, children, launch = self.sampler(directory, 'hang')
            with launch:
                report = capture_sample(os.getpid(), prefix, 1)
            self.assertIn('timed out', report['error'])
            self.assertEqual(report['exit_code'], -9)
            self.assertTrue(prefix.with_suffix('.observed').exists(), 'progress must be durable while the sampler is live')
            self.assertIn('processing symbols', prefix.with_suffix('.log').read_text())
            self.assertEqual(report['target_after_sample']['exit_code'], 0)
            self.assertTrue(all(child.poll() is not None for child in children))
            self.assertFalse(prefix.with_suffix('.sample.txt').exists())
            self.assertTrue(prefix.with_suffix('.resources-observed').exists(), 'resources must survive while the sampler is live')
            rows = [json.loads(line) for line in prefix.with_suffix('.resources.jsonl').read_text().splitlines()]
            self.assertEqual(rows, report['sampler_resources'])
            self.assertTrue(all(row['pid'] == children[0].pid for row in rows))
            self.assertTrue(any(row['reason'] == 'processing_symbols' for row in rows))

    def test_partial_stacks_do_not_hide_sampler_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix, children, launch = self.sampler(directory, 'error')
            with launch:
                report = capture_sample(os.getpid(), prefix, 2)
            self.assertEqual(report['sampled_threads'], 1)
            self.assertEqual(report['exit_code'], 7)
            self.assertIn('error', report)
            self.assertTrue(all(child.poll() is not None for child in children))

    def test_unreadable_sample_preserves_exit_and_raw_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix, children, launch = self.sampler(directory, 'invalid')
            with launch:
                report = capture_sample(os.getpid(), prefix, 2)
            self.assertEqual(report['exit_code'], 7)
            self.assertIn('decode', report['error'])
            self.assertNotIn('sampled_threads', report)
            self.assertTrue(prefix.with_suffix('.sample.txt').read_bytes().endswith(b'\xff'))
            self.assertEqual(report['target_after_sample']['exit_code'], 0)
            self.assertTrue(all(child.poll() is not None for child in children))

    def test_unreadable_target_query_preserves_sampler_outcome(self):
        # A real child produces invalid UTF-8, exercising subprocess text decoding.
        run = subprocess.run

        def unreadable_query(args, **kwargs):
            self.assertEqual(args[0], 'ps')
            return run([sys.executable, '-c', 'import os;os.write(1,bytes([255]))'], **kwargs)

        for mode, expected_exit in (('success', 0), ('error', 7), ('hang', -9)):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                prefix, children, launch = self.sampler(directory, mode)
                with launch, patch('native_ui_diagnostics.subprocess.run', side_effect=unreadable_query):
                    report = capture_sample(os.getpid(), prefix, .5 if mode == 'hang' else 2)
                self.assertEqual(report['exit_code'], expected_exit)
                for observation in ('target_at_processing', 'target_after_sample'):
                    self.assertIn('decode', report[observation]['error'])
                    self.assertIn('finished_monotonic_ns', report[observation])
                self.assertTrue(all(child.poll() is not None for child in children))
                self.assertIn('processing_symbols', prefix.with_suffix('.events.jsonl').read_text())
                if mode == 'hang':
                    self.assertIn('timed out', report['error'])
                else:
                    self.assertEqual(report['sampled_threads'], 1)
                    self.assertIn('Thread_42', prefix.with_suffix('.sample.txt').read_text())
                    if mode == 'success':
                        self.assertNotIn('error', report)
                    else:
                        self.assertEqual(report['error'], 'Sampler failed or returned no thread stacks')

    def test_unreadable_ownership_query_keeps_failure_and_continues_watching(self):
        run = subprocess.run
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            container = root / 'container'
            (container / 'Documents').mkdir(parents=True)
            advances = container / 'Documents/clock-advances.jsonl'
            original = ''.join(json.dumps(dict(pid=os.getpid(), result=60)) + '\n' for _ in range(2))
            advances.write_text(original)
            queries = 0

            def query(args, **kwargs):
                nonlocal queries
                if args[0] == 'xcrun':
                    return subprocess.CompletedProcess(args, 0, str(container) + '\n', '')
                self.assertEqual(args[0], 'ps')
                queries += 1
                if queries == 1:
                    return run([sys.executable, '-c', 'import os;os.write(1,bytes([255]))'], **kwargs)
                command = '/Library/Developer/CoreSimulator/Devices/owned/data/Containers/Bundle/Application/id/NativeProbe.app/NativeProbe'
                return subprocess.CompletedProcess(args, 0, command + '\n', '')

            with patch('native_ui_diagnostics.subprocess.run', side_effect=query), \
                 patch('native_ui_diagnostics.capture_sample', return_value={'exit_code': 7, 'error': 'sampler failed'}) as sample, \
                 patch('native_ui_diagnostics.host_context', return_value={'finished': 1}), \
                 patch('native_ui_diagnostics.signal.signal'), \
                 patch('native_ui_diagnostics.time.sleep', side_effect=StopIteration):
                with self.assertRaises(StopIteration):
                    watch('owned', root)
            first = json.loads((root / f'advance-failure-{os.getpid()}-0.json').read_text())
            self.assertIn('decode', first['error'])
            self.assertEqual(first['advance']['result'], 60)
            second = json.loads((root / f'advance-failure-{os.getpid()}-1.json').read_text())
            self.assertEqual(second['advance']['result'], 60)
            self.assertEqual(second['exit_code'], 7)
            self.assertEqual(second['error'], 'sampler failed')
            sample.assert_called_once_with(os.getpid(), root / f'advance-failure-{os.getpid()}-1', 10)
            self.assertEqual(advances.read_text(), original)

    def test_watcher_publication_failure_retains_outcome_without_resampling(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            container = root / 'container'
            (container / 'Documents').mkdir(parents=True)
            advances = container / 'Documents/clock-advances.jsonl'
            original = ''.join(json.dumps(dict(pid=os.getpid(), result=60)) + '\n' for _ in range(2))
            advances.write_text(original)
            first = root / f'advance-failure-{os.getpid()}-0.json'
            first.mkdir()  # Real publication failure without making the whole log directory unavailable.
            command = '/Library/Developer/CoreSimulator/Devices/owned/data/Containers/Bundle/Application/id/NativeProbe.app/NativeProbe'

            def query(args, **kwargs):
                return subprocess.CompletedProcess(args, 0, str(container) if args[0] == 'xcrun' else command, '')

            def capture(pid, prefix, timeout):
                prefix.with_suffix('.log').write_text('partial sampler output\n')
                return dict(exit_code=7, error='sampler failed', sampled_threads=1)

            log = io.StringIO()
            with patch('native_ui_diagnostics.subprocess.run', side_effect=query), \
                 patch('native_ui_diagnostics.capture_sample', side_effect=capture) as sample, \
                 patch('native_ui_diagnostics.host_context', return_value={'finished': 1}), \
                 patch('native_ui_diagnostics.signal.signal'), \
                 patch('native_ui_diagnostics.time.sleep', side_effect=[None, StopIteration]), \
                 patch('sys.stderr', log):
                with self.assertRaises(StopIteration):
                    watch('owned', root)
            reports = [json.loads(line) for line in log.getvalue().splitlines() if line.startswith('{')]
            self.assertEqual(len(reports), 1)
            report = reports[0]
            self.assertEqual(report['advance'], dict(pid=os.getpid(), result=60))
            self.assertEqual(report['index'], 0)
            self.assertEqual(report['exit_code'], 7)
            self.assertEqual(report['sampled_threads'], 1)
            self.assertEqual(report['error'], 'sampler failed')
            self.assertIn('report_error', report)
            self.assertEqual(first.with_suffix('.log').read_text(), 'partial sampler output\n')
            second = json.loads((root / f'advance-failure-{os.getpid()}-1.json').read_text())
            self.assertEqual(second['exit_code'], 7)
            self.assertNotIn('report_error', second)
            self.assertEqual(sample.call_count, 2, 'Later polls must not resample either failed advance')
            self.assertEqual(advances.read_text(), original)

    def test_log_read_intervals_bind_observed_bytes_without_inventing_write_times(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix, children, launch = self.sampler(directory, 'delayed')
            with launch:
                report = capture_sample(os.getpid(), prefix, 1)
            raw = prefix.with_suffix('.log').read_bytes()
            observations = report['sample_log_reads']
            self.assertTrue(observations)
            self.assertIn('previous_empty_read', observations[0])
            offset = 0
            import hashlib
            for read in observations:
                self.assertEqual(read['offset'], offset)
                chunk = raw[offset:offset + read['length']]
                self.assertEqual(hashlib.sha256(chunk).hexdigest(), read['sha256'])
                self.assertLessEqual(read['started_monotonic_ns'], read['finished_monotonic_ns'])
                if 'previous_empty_read' in read:
                    empty = read['previous_empty_read']
                    self.assertEqual(empty['offset'], offset)
                    self.assertLessEqual(empty['finished_monotonic_ns'], read['started_monotonic_ns'])
                offset += read['length']
            self.assertEqual(offset, len(raw))
            self.assertEqual([json.loads(line) for line in prefix.with_suffix('.log-reads.jsonl').read_text().splitlines()], observations)
            self.assertIn('timed out', report['error'])
            self.assertTrue(all(child.poll() is not None for child in children))

    def test_log_read_publication_failure_keeps_primary_result_and_memory_evidence(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix, children, launch = self.sampler(directory, 'error')
            original = Path.open

            def open_path(path, *args, **kwargs):
                if path == prefix.with_suffix('.log-reads.jsonl'):
                    raise OSError('read observation storage unavailable')
                return original(path, *args, **kwargs)

            with launch, patch.object(Path, 'open', open_path):
                report = capture_sample(os.getpid(), prefix, 2)
            self.assertEqual(report['exit_code'], 7)
            self.assertEqual(report['sampled_threads'], 1)
            self.assertTrue(report['sample_log_reads'])
            self.assertIn('storage unavailable', report['log_read_observation_error'])
            self.assertTrue(all(child.poll() is not None for child in children))

    def test_short_symbol_phase_has_multiple_resource_observations_before_reaping(self):
        # Control the process/clock boundary so host scheduling cannot extend this
        # short phase until a sparse observer accidentally sees a second read.
        with tempfile.TemporaryDirectory() as directory:
            prefix = Path(directory) / 'capture'
            clock = [0.0]
            process = Mock(pid=12345, returncode=None)
            output = []

            def launch(command, **kwargs):
                output.append(kwargs['stdout'])
                output[0].write(b'Sampling completed, processing symbols...\n')
                output[0].flush()
                return process

            def wait(timeout):
                clock[0] = min(clock[0] + timeout, .2)
                if clock[0] < .2:
                    raise subprocess.TimeoutExpired('sample', timeout)
                prefix.with_suffix('.sample.txt').write_text('Call graph:\n    3 Thread_42\n')
                output[0].write(b'Sample analysis of process 123 written to file capture.sample.txt\n')
                output[0].flush()
                process.returncode = 0
                return 0

            def resources(pid):
                self.assertIsNone(process.returncode, 'a reaped PID must never be queried')
                self.assertLess(clock[0], .4, 'a resource query must not start after the deadline')
                stamp = round(clock[0] * 1e9)
                return dict(pid=pid, status='available', identity=dict(pid=pid, start_abstime=1),
                            started_monotonic_ns=stamp, finished_monotonic_ns=stamp)

            process.wait.side_effect = wait
            process.poll.side_effect = lambda: process.returncode
            with patch('native_ui_diagnostics.subprocess.Popen', side_effect=launch), \
                 patch('native_ui_diagnostics.subprocess.run',
                       return_value=subprocess.CompletedProcess([], 0, stdout='owned', stderr='')), \
                 patch('native_ui_diagnostics.time.monotonic', side_effect=lambda: clock[0]), \
                 patch('native_ui_diagnostics.time.monotonic_ns', side_effect=lambda: round(clock[0] * 1e9)), \
                 patch('native_ui_diagnostics.process_resources', side_effect=resources):
                report = capture_sample(os.getpid(), prefix, .4)
            self.assertNotIn('error', report)
            self.assertEqual(report['sampled_threads'], 1)
            phases = {e['phase']: e['host_monotonic_ns'] for e in report['sample_events'] if 'phase' in e}
            rows = [row for row in report['sampler_resources']
                    if phases['processing_symbols'] <= row['started_monotonic_ns']
                    <= row['finished_monotonic_ns'] < phases['output_written']]
            self.assertGreaterEqual(len(rows), 2, 'phase CPU growth requires at least two contained reads')
            self.assertEqual([json.loads(line) for line in prefix.with_suffix('.resources.jsonl').read_text().splitlines()],
                             report['sampler_resources'])
            process.kill.assert_not_called()

    def test_interruption_during_progress_observation_reaps_sampler(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix, children, launch = self.sampler(directory, 'hang')
            with launch, patch('native_ui_diagnostics.subprocess.run', side_effect=InterruptedError('signal 15')):
                with self.assertRaisesRegex(InterruptedError, 'signal 15'):
                    capture_sample(os.getpid(), prefix, 2)
            self.assertTrue(children)
            self.assertTrue(all(child.poll() is not None for child in children))
            self.assertIn('processing_symbols', prefix.with_suffix('.events.jsonl').read_text())

    def test_resource_identity_change_is_retained_without_joining_processes(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix, children, launch = self.sampler(directory, 'hang')
            identities = iter((1, 2))

            def observe(pid):
                return dict(pid=pid, status='available', identity=dict(pid=pid, start_abstime=next(identities)),
                            raw={'ri_user_time': 0})

            with launch, patch('native_ui_diagnostics.process_resources', side_effect=observe):
                report = capture_sample(os.getpid(), prefix, .5)
            self.assertIn('timed out', report['error'])
            rows = report['sampler_resources']
            self.assertEqual([row['status'] for row in rows], ['available', 'identity_mismatch'])
            self.assertEqual(rows[-1]['expected_identity'], rows[0]['identity'])
            self.assertNotEqual(rows[-1]['identity'], rows[0]['identity'])
            self.assertTrue(all(child.poll() is not None for child in children))

    def test_unavailable_resources_preserve_original_sampler_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            prefix, children, launch = self.sampler(directory, 'error')
            with launch, patch('native_ui_diagnostics.process_resources',
                               side_effect=lambda pid: dict(pid=pid, status='unavailable', error='denied')):
                report = capture_sample(os.getpid(), prefix, 2)
            self.assertEqual(report['exit_code'], 7)
            self.assertEqual(report['sampled_threads'], 1)
            self.assertIn('error', report)
            self.assertTrue(report['sampler_resources'])
            self.assertTrue(all(row['status'] == 'unavailable' for row in report['sampler_resources']))
            self.assertTrue(all(child.poll() is not None for child in children))

    def test_host_context_preserves_unavailable_observations(self):
        with patch('native_ui_diagnostics.os.getloadavg', side_effect=OSError('unavailable')), \
             patch('native_ui_diagnostics.shutil.disk_usage', side_effect=OSError('storage unavailable')), \
             patch('native_ui_diagnostics.subprocess.run', side_effect=subprocess.TimeoutExpired('pmset', 2)):
            report = host_context(Path('/artifacts'))
        self.assertIn('unavailable', report['load_error'])
        self.assertEqual(report['storage'], dict(path='/artifacts', error='storage unavailable'))
        self.assertIn('error', report['thermal'])
        self.assertNotIn('load_1_5_15', report)
        self.assertGreaterEqual(report['finished'], report['started'])
        for observation in ('os.getloadavg', 'shutil.disk_usage', 'subprocess.run'):
            with self.subTest(observation=observation), \
                 patch(f'native_ui_diagnostics.{observation}', side_effect=InterruptedError('signal 15')):
                with self.assertRaisesRegex(InterruptedError, 'signal 15'):
                    host_context(Path.cwd())

    def test_only_complete_failure_records_trigger_sampling(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "advances.jsonl"
            self.assertEqual(failed_advances(path), [])
            failed = dict(pid=123, result=60)
            path.write_text(json.dumps(dict(pid=123, result=0)) + "\n" + json.dumps(failed) + '\n{"pid":')
            self.assertEqual(failed_advances(path), [(1, failed)])

    def test_binary_inventory_without_thread_stacks_is_not_a_sample(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "sample.txt"
            path.write_text("Call graph:\n\nBinary Images:\n")
            self.assertEqual(sampled_threads(path), 0)
            path.write_text("Call graph:\n    92 Thread_123 DispatchQueue_1: com.apple.main-thread\n      92 start (in dyld)\n")
            self.assertEqual(sampled_threads(path), 1)

    def test_sampling_requires_the_owned_simulator_app(self):
        command = "/Users/test/Library/Developer/CoreSimulator/Devices/owned/data/Containers/Bundle/Application/id/NativeProbe.app/NativeProbe"
        self.assertTrue(owned_app(command + "\n", "owned"))
        self.assertFalse(owned_app(command, "other"))
        self.assertFalse(owned_app(command + "Tests", "owned"))
        self.assertFalse(owned_app("python3 -c " + command, "owned"))


if __name__ == "__main__":
    unittest.main()
