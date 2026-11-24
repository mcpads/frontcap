"""Controlled sampler lifecycle; native accounting is tested separately."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from native_process_resources import process_resources
from native_sampler_inspection import SamplerInspection, blocked_symbol_threads
from native_ui_diagnostics import capture_sample


class Cancelled(BaseException):
    pass


class SamplerResources:
    """Fixture identities with real, non-reaping owned-child exit observations."""

    def __init__(self):
        self.identities = {}

    def register(self, child):
        self.identities[child.pid] = dict(pid=child.pid, start_abstime=len(self.identities) + 1,
                                          executable_uuid='00' * 16)

    def __call__(self, pid):
        identity = self.identities[pid]
        started = time.monotonic_ns()
        # This models the resource provider's identity/exit fields, not macOS
        # counters. WNOWAIT keeps the real primary PID reserved for its inspector.
        exited = os.waitid(os.P_PID, pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
        return dict(status='available', api='controlled-owned-child', identity=dict(identity),
                    raw=dict(ri_proc_exit_abstime=started if exited else 0),
                    started_monotonic_ns=started, finished_monotonic_ns=time.monotonic_ns())


class NativeSamplerInspectionTests(unittest.TestCase):
    def run_capture(self, directory, primary_delay, secondary_mode, timeout, cancel=False,
                    resource_error=None):
        script = Path(directory) / 'controlled_sampler.py'
        script.write_text('''import json,pathlib,sys,time
role,delay,mode,pid,path=sys.argv[1:];path=pathlib.Path(path)
if role=='primary':
 print('Sampling completed, processing symbols...',flush=True)
 if float(delay)==1.6:
  metadata=path.with_suffix('').with_suffix('.sampler.json');deadline=time.monotonic()+10
  while True:
   if metadata.exists():
    state=json.loads(metadata.read_text())['state']
    if state in (('finished','unavailable') if mode=='partial' else ('running','finished','unavailable')):break
   if time.monotonic()>=deadline:sys.exit(8)
   time.sleep(.01)
  time.sleep(.1)
 else:time.sleep(float(delay))
 path.write_text('Call graph:\\n    3 Thread_42\\n')
 if mode=='complete_failed':sys.exit(9)
else:
 if mode.startswith('complete'):
  metadata=path.with_suffix('').with_suffix('.json');deadline=time.monotonic()+10
  while True:
   if metadata.exists() and 'primary_exit_observed_monotonic_ns' in json.loads(metadata.read_text()):break
   if time.monotonic()>=deadline:sys.exit(8)
   time.sleep(.01)
 path.write_text('Process:         sample ['+pid+']\\nCall graph:\\n    3 Thread_43\\n')
 if mode.startswith('complete'):sys.exit(0)
 if mode=='partial':sys.exit(7)
 time.sleep(30)
''')
        create = subprocess.Popen
        children = []
        primary_reaps = []
        resources = SamplerResources()

        def observe_resources(pid):
            if resource_error is not None:
                with patch('native_process_resources.resource_query', side_effect=resource_error):
                    return process_resources(pid)
            return resources(pid)

        def launch(command, **kwargs):
            if command[0] != '/usr/bin/sample':
                return create(command, **kwargs)
            role = 'secondary' if children else 'primary'
            if role == 'secondary' and secondary_mode == 'denied':
                raise OSError('Inspection launch denied')
            child = create([sys.executable, str(script), role, str(primary_delay), secondary_mode,
                            command[1], command[-1]], **kwargs)
            children.append(child)
            resources.register(child)
            if role == 'primary':
                # Wait for the real child's marker before starting the lifecycle
                # under test; interpreter startup is not a sampler symbol phase.
                deadline = time.monotonic() + 3
                while 'processing symbols' not in Path(kwargs['stdout'].name).read_text():
                    if child.poll() is not None or time.monotonic() >= deadline:
                        child.kill()
                        child.wait()
                        self.fail('Controlled sampler did not become ready')
                    time.sleep(.01)
                def guarded(reap):
                    def call(*args, **kwargs):
                        if len(children) > 1:
                            primary_reaps.append(children[1].returncode)
                            self.assertIsNotNone(children[1].returncode,
                                                 'never reap the primary before its inspector')
                        return reap(*args, **kwargs)
                    return call
                child.wait = guarded(child.wait)
                child.poll = guarded(child.poll)
            return child

        def threads(pid, identity, **kwargs):
            if cancel is True and len(children) > 1:
                raise Cancelled('owned cancellation')
            return dict(status='available', identity_verified=True,
                        threads=[dict(status='available', thread_id=42, raw=dict(pth_run_state=4))])

        prefix = Path(directory) / 'capture'
        primary_exited = SamplerInspection.primary_exited

        def observe_exit(inspection, primary):
            exited = primary_exited(inspection, primary)
            if cancel == 'after_exit' and exited:
                raise Cancelled('owned cancellation after primary exit')
            return exited

        try:
            with patch('native_ui_diagnostics.subprocess.Popen', side_effect=launch), \
                 patch('native_ui_diagnostics.process_threads', side_effect=threads), \
                 patch('native_ui_diagnostics.process_resources', side_effect=observe_resources), \
                 patch('native_sampler_inspection.process_resources', side_effect=observe_resources), \
                 patch.dict(os.environ, {'FC_SAMPLER_FILES': ''}), \
                 patch.object(SamplerInspection, 'primary_exited', observe_exit):
                if cancel:
                    with self.assertRaises(Cancelled):
                        capture_sample(os.getpid(), prefix, timeout)
                    report = None
                else:
                    report = capture_sample(os.getpid(), prefix, timeout)
            self.assertTrue(children)
            self.assertTrue(all(child.returncode is not None for child in children))
            for child in children:
                # A fixture resource response cannot prove retirement. Verify that
                # the actual child has already been reaped by the product path.
                with self.assertRaises(ChildProcessError):
                    os.waitpid(child.pid, os.WNOHANG)
            if len(children) > 1:
                self.assertTrue(primary_reaps)
            return report, children, prefix
        finally:
            # Failed assertions still retire only this fixture's owned children;
            # these fallback waits do not contribute to the assertions above.
            for child in reversed(children):
                if child.returncode is None:
                    child.kill()
                    create.wait(child, timeout=5)

    def test_partial_inspection_does_not_replace_primary_success_or_retry(self):
        with tempfile.TemporaryDirectory() as directory:
            report, children, prefix = self.run_capture(directory, 1.6, 'partial', 6)
            self.assertNotIn('error', report)
            self.assertEqual(report['exit_code'], 0)
            self.assertEqual(len(children), 2)
            nested = report['sampler_inspection']
            self.assertEqual(nested['exit_code'], 7)
            self.assertEqual(nested['sampled_threads'], 1)
            self.assertTrue(nested['identity_verified'])
            self.assertIn('error', nested)
            self.assertEqual(json.loads(prefix.with_suffix('.sampler.json').read_text()), nested)

    def test_inspection_can_publish_after_primary_exit_without_changing_primary_outcome(self):
        for mode, expected in (('complete', 0), ('complete_failed', 9)):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                report, children, prefix = self.run_capture(directory, 1.6, mode, 6)
                self.assertEqual(report['exit_code'], expected)
                self.assertEqual('error' in report, expected != 0)
                nested = report['sampler_inspection']
                self.assertNotIn('error', nested)
                self.assertNotIn('stopped_because', nested)
                self.assertEqual(nested['exit_code'], 0)
                self.assertEqual(nested['sampled_threads'], 1)
                self.assertTrue(nested['target_header_matches'])
                self.assertTrue(nested['identity_verified'])
                self.assertGreater(nested['target_after']['raw']['ri_proc_exit_abstime'], 0)
                self.assertLess(nested['primary_exit_observed_monotonic_ns'], nested['finished_monotonic_ns'])
                self.assertLess(nested['finished_monotonic_ns'], nested['deadline_monotonic_ns'])
                self.assertEqual(json.loads(prefix.with_suffix('.sampler.json').read_text()), nested)

    def test_primary_exit_keeps_hung_inspection_bounded_before_reaping_primary(self):
        with tempfile.TemporaryDirectory() as directory:
            report, children, prefix = self.run_capture(directory, 1.6, 'hang', 6)
            self.assertNotIn('error', report)
            self.assertEqual(report['sampler_inspection']['stopped_because'], 'inspection_deadline')
            self.assertEqual(report['sampler_inspection']['exit_code'], -9)
            self.assertTrue(report['sampler_inspection']['identity_verified'])
            self.assertLess(report['sampler_inspection']['primary_exit_observed_monotonic_ns'],
                            report['sampler_inspection']['finished_monotonic_ns'])
            self.assertLess(report['sample_finished_monotonic_ns'], report['sample_deadline_monotonic_ns'])

    def test_inspector_has_its_own_deadline_without_replacing_primary_timeout(self):
        with tempfile.TemporaryDirectory() as directory:
            report, children, prefix = self.run_capture(directory, 30, 'hang', 4.6)
            self.assertIn('timed out', report['error'])
            self.assertEqual(report['exit_code'], -9)
            self.assertEqual(len(children), 2)
            nested = report['sampler_inspection']
            self.assertEqual(nested['stopped_because'], 'inspection_deadline')
            self.assertLess(nested['finished_monotonic_ns'], report['sample_finished_monotonic_ns'])
            self.assertEqual(nested['sampled_threads'], 1)
            self.assertIn('error', nested)
            self.assertLessEqual(nested['deadline_monotonic_ns'],
                                 report['sample_deadline_monotonic_ns'])

    def test_insufficient_primary_budget_never_starts_an_inspector(self):
        with tempfile.TemporaryDirectory() as directory:
            report, children, prefix = self.run_capture(directory, 30, 'hang', 1.4)
            self.assertEqual(len(children), 1)
            self.assertIn('timed out', report['error'])
            self.assertEqual(report['sampler_inspection']['state'], 'unavailable')
            self.assertIn('Fewer than three seconds', report['sampler_inspection']['error'])

    def test_cancellation_retires_both_children_and_keeps_inspection_evidence(self):
        for cancel, delay in ((True, 30), ('after_exit', 1.6)):
            with self.subTest(cancel=cancel), tempfile.TemporaryDirectory() as directory:
                report, children, prefix = self.run_capture(directory, delay, 'hang', 6, cancel=cancel)
                self.assertEqual(len(children), 2)
                nested = json.loads(prefix.with_suffix('.sampler.json').read_text())
                self.assertEqual(nested['exit_code'], -9)
                self.assertEqual(nested['stopped_because'], 'primary_finished_or_interrupted')
                if cancel == 'after_exit':self.assertIn('primary_exit_observed_monotonic_ns', nested)

    def test_denied_inspection_launch_preserves_primary_success(self):
        with tempfile.TemporaryDirectory() as directory:
            report, children, prefix = self.run_capture(directory, 1.6, 'denied', 6)
            self.assertNotIn('error', report)
            self.assertEqual(len(children), 1)
            self.assertIn('Inspection launch denied', report['sampler_inspection']['error'])

    def test_lost_exit_observation_stops_inspector_and_preserves_primary_success(self):
        from native_sampler_inspection import unreaped_exit
        calls = []

        def observe(primary):
            calls.append(primary.pid)
            if len(calls) > 1:
                raise OSError('Exit observation unavailable')
            return unreaped_exit(primary)

        with tempfile.TemporaryDirectory() as directory, \
             patch('native_sampler_inspection.unreaped_exit', side_effect=observe):
            report, children, prefix = self.run_capture(directory, 1.6, 'hang', 6)
            self.assertNotIn('error', report)
            self.assertEqual(len(children), 2)
            self.assertEqual(report['sampler_inspection']['stopped_because'], 'primary_observation_unavailable')

    def test_changed_identity_or_unavailable_exit_observation_prevents_launch(self):
        with tempfile.TemporaryDirectory() as directory:
            primary = subprocess.Popen(['/bin/sleep', '30'])
            resources = SamplerResources()
            resources.register(primary)
            try:
                identity = resources(primary.pid)['identity']
                wrong = dict(identity, start_abstime=identity['start_abstime'] + 1)
                for index, condition in enumerate(('identity', 'waitid')):
                    inspection = SamplerInspection(Path(directory)/str(index), time.monotonic()+10)
                    with patch('native_sampler_inspection.subprocess.Popen') as launch, \
                         patch('native_sampler_inspection.process_resources', side_effect=resources):
                        if condition == 'identity':
                            inspection.start(primary, wrong, {})
                        else:
                            with patch('native_sampler_inspection.unreaped_exit', side_effect=OSError('unavailable')):
                                inspection.start(primary, identity, {})
                        launch.assert_not_called()
                        self.assertIn('error', inspection.report)
                        self.assertIsNone(primary.poll())
            finally:
                primary.kill()
                primary.wait()

    def test_unsupported_resources_preserve_primary_success_without_inspection(self):
        with tempfile.TemporaryDirectory() as directory:
            report, children, _ = self.run_capture(directory, 1.25, 'hang', 4,
                resource_error=OSError('proc_pid_rusage requires macOS'))
            self.assertEqual(len(children), 1)
            self.assertEqual(report['exit_code'], 0)
            self.assertNotIn('error', report)
            self.assertNotIn('sampler_inspection', report)
            self.assertTrue(report['sampler_resources'])
            for observation in report['sampler_resources']:
                self.assertEqual(observation['status'], 'unavailable')
                self.assertIn('requires macOS', observation['error'])
                self.assertNotIn('identity', observation)
                self.assertNotIn('raw', observation)

    def test_partial_or_unverified_thread_state_cannot_trigger_inspection(self):
        valid = dict(status='available', identity_verified=True,
                     threads=[dict(status='available', thread_id=42, raw=dict(pth_run_state=4))])
        self.assertEqual(blocked_symbol_threads(dict(status='available', thread_snapshot=valid)), [42])
        for change in (dict(status='partial'), dict(identity_verified=False),
                       dict(threads=[dict(status='available', thread_id=42, raw=dict(pth_run_state=3))])):
            self.assertEqual(blocked_symbol_threads(dict(status='available', thread_snapshot=dict(valid, **change))), [])
        self.assertEqual(blocked_symbol_threads(dict(status='identity_mismatch', thread_snapshot=valid)), [])


if __name__ == '__main__':
    unittest.main()
