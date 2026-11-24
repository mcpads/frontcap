"""A lost pause reply must not strand the guest or turn recovery into success."""
import json
import os
import subprocess
import signal
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from native_android_pause import pause_guest


class AndroidPauseTests(unittest.TestCase):
    def test_clock_trace_preserves_partial_failure_and_does_not_trace_mutations(self):
        with tempfile.TemporaryDirectory() as directory:
            reads = []
            state = ['running']

            def run(args, **options):
                self.assertEqual(options['timeout'], 5)
                if args[1] == 'shell':
                    self.assertEqual(options['env']['ADB_TRACE'], 'adb,shell')
                    reads.append(args)
                    if len(reads) == 2:
                        raise subprocess.TimeoutExpired(args, 5, output=b'',
                            stderr=b'adb client: shell service requested\n')
                    return subprocess.CompletedProcess(args, 0, b'12.34\n1791420000.1\n',
                        b'adb client: connected\nFRONTCAP_CLOCK shell-entered pid=41\n')
                self.assertNotIn('env', options)
                if args[-1] == 'pause': state[0] = 'stopped'
                if args[-1] == 'resume': state[0] = 'running'
                return subprocess.CompletedProcess(args, 0, ('virtual device is '+state[0]).encode(), b'')

            with patch.dict(os.environ, {'ADB_TRACE': 'all'}), \
                 patch('native_android_pause.subprocess.run', side_effect=run), \
                 patch('native_android_pause.time.sleep'):
                result = pause_guest(['adb'], directory)
                self.assertEqual(os.environ['ADB_TRACE'], 'all')
            self.assertFalse(result['passed'])
            self.assertEqual(len(reads), 2)
            self.assertIn('clock-after failed', result['error'])
            last = result['commands'][-1]
            self.assertEqual(last['adb_trace'], 'adb,shell')
            self.assertEqual(last['stdout'], '')
            self.assertIn('shell service requested', last['stderr'])
            self.assertNotIn('shell-entered', last['stderr'])

    def test_real_sigint_after_pause_effect_resumes_and_reaps_transport(self):
        with tempfile.TemporaryDirectory() as directory:
            root=Path(directory);(root/'state').write_text('running')
            adb=root/'adb.py'
            adb.write_text('''import os,pathlib,signal,sys,time
root=pathlib.Path(sys.argv[1]);args=sys.argv[2:]
with (root/'calls').open('a') as out:out.write(' '.join(args)+'\\n')
if args==['emu','avd','status']:print('virtual device is '+(root/'state').read_text())
elif args==['emu','avd','pause']:
 (root/'state').write_text('stopped');(root/'pause-pid').write_text(str(os.getpid()))
 os.kill(os.getppid(),signal.SIGINT);time.sleep(30)
elif args==['emu','avd','resume']:(root/'state').write_text('running');print('resumed')
elif args[0]=='shell':print('12.34\\n1791420000.123456789')
else:raise AssertionError(args)
''')
            driver='''import pathlib,sys
sys.path.insert(0,sys.argv[1])
from native_android_pause import pause_guest
try:pause_guest([sys.executable,sys.argv[2],sys.argv[3]],pathlib.Path(sys.argv[3]))
except KeyboardInterrupt:print('original keyboard interrupt propagated',flush=True)
else:raise AssertionError('cancellation was consumed')
'''
            process=subprocess.Popen([sys.executable,'-c',driver,str(Path(__file__).parent.resolve()),str(adb),str(root)],
                                     stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,start_new_session=True)
            try:
                stdout,stderr=process.communicate(timeout=10)
                self.assertEqual(process.returncode,0,stderr)
                self.assertIn('original keyboard interrupt propagated',stdout)
                self.assertEqual((root/'state').read_text(),'running')
                self.assertEqual((root/'calls').read_text().splitlines().count('emu avd resume'),1)
                report=json.loads((root/'guest-pause.json').read_text())
                self.assertFalse(report['passed'])
                self.assertIn('KeyboardInterrupt',report['error'])
                self.assertEqual(report['commands'][-1]['name'],'state-recovery')
                with self.assertRaises(ProcessLookupError):os.kill(int((root/'pause-pid').read_text()),0)
            finally:
                try:os.killpg(process.pid,signal.SIGKILL)
                except ProcessLookupError:pass
                process.wait(timeout=5)
                process.stdout.close();process.stderr.close()

    def test_cancellation_resumes_uncertain_pause_and_preserves_original_exception(self):
        for boundary in ('before-pause', 'pause', 'hold', 'recovery', 'hold-and-recovery'):
            with self.subTest(boundary=boundary), tempfile.TemporaryDirectory() as directory:
                cancellation=InterruptedError('owner cancelled')
                second=InterruptedError('cancel recovery')
                state=['running'];calls=[]
                def run(args,**options):
                    name=args[-1];calls.append(name)
                    if boundary=='before-pause':raise cancellation
                    if name=='status':output='virtual device is '+state[0]
                    elif name=='pause':
                        state[0]='stopped';output='paused'
                        if boundary=='pause':raise cancellation
                    elif name=='resume':
                        if boundary=='recovery':raise cancellation
                        if boundary=='hold-and-recovery':raise second
                        state[0]='running';output='resumed'
                    else:output='12.34\n1791420000.123456789'
                    return subprocess.CompletedProcess(args,0,output.encode(),b'')
                def hold(_seconds):
                    if boundary in ('hold','hold-and-recovery'):raise cancellation
                    if boundary=='recovery':raise RuntimeError('hold failed')
                with patch('native_android_pause.subprocess.run',side_effect=run), \
                     patch('native_android_pause.time.sleep',side_effect=hold):
                    with self.assertRaises(InterruptedError) as caught:
                        pause_guest(['adb'],directory)
                self.assertIs(caught.exception,cancellation)
                report=json.loads((Path(directory)/'guest-pause.json').read_text())
                self.assertFalse(report['passed'])
                self.assertIn('finished',report)
                if boundary=='before-pause':self.assertEqual(calls,['status'])
                else:self.assertEqual(calls.count('resume'),1)
                self.assertEqual(state[0],'stopped' if boundary in ('recovery','hold-and-recovery') else 'running')
                if boundary in ('recovery','hold-and-recovery'):self.assertIn('InterruptedError',report['recovery_error'])

    def exercise(self, directory, failure):
        root = Path(directory)
        state = root / 'state'
        state.write_text('running')
        adb = root / 'adb.py'
        adb.write_text('''import pathlib, sys, time
state = pathlib.Path(sys.argv[1])
failure = sys.argv[2]
args = sys.argv[3:]
if args == ['emu', 'avd', 'status']:
    print('virtual device is ' + state.read_text())
    print('OK')
elif args == ['emu', 'avd', 'pause']:
    state.write_text('stopped')
    print('pause applied', flush=True)
    if failure == 'pause-timeout': time.sleep(30)
    if failure == 'pause-error': sys.exit(7)
elif args == ['emu', 'avd', 'resume']:
    if failure == 'resume-error':
        print('resume refused')
        sys.exit(9)
    state.write_text('running')
    print('OK')
elif args[0] == 'shell':
    print('12.34\\n1791420000.123456789')
else:
    sys.exit(2)
''')

        def hold(_seconds):
            # The stopped state and start record must survive before the hold begins.
            self.assertEqual(state.read_text(), 'stopped')
            saved = json.loads((root / 'guest-pause.json').read_text())
            self.assertIn('hold_started_monotonic_ns', saved)
            self.assertFalse(saved['passed'])

        # Replace only this module's clock reference. Patching time.sleep itself also
        # changes subprocess polling and can invoke hold during resume/recovery.
        with patch('native_android_pause.time', wraps=time) as clock:
            clock.sleep.side_effect = hold
            result = pause_guest([sys.executable, str(adb), str(state), failure], root)
        self.assertEqual(json.loads((root / 'guest-pause.json').read_text()), result)
        return result, state.read_text()

    def test_clock_shell_progress_retains_separate_streams_and_original_failure(self):
        run_shell = subprocess.run
        for boundary in ('success', 'uptime', 'realtime', 'timeout'):
            with self.subTest(boundary=boundary), tempfile.TemporaryDirectory() as directory:
                state = ['running']
                reads = []
                def run(args, **options):
                    self.assertEqual(options['timeout'], 5)
                    if args[1] == 'shell':
                        reads.append(args[-1])
                        failure = boundary if len(reads) == 2 else 'success'
                        if failure == 'timeout':
                            raise subprocess.TimeoutExpired(args, 5, output=b'12.34\n',
                                stderr=b'FRONTCAP_CLOCK shell-entered pid=41\nFRONTCAP_CLOCK uptime-read\n')
                        prefix = ('cut() { return 7; }; ' if failure == 'uptime' else
                                  'cut() { printf "12.34\\n"; }; ')
                        prefix += ('date() { return 8; }; ' if failure == 'realtime' else
                                   'date() { printf "1791420000.123456789\\n"; }; ')
                        return run_shell(['sh', '-c', prefix+args[-1]], **options)
                    if args[-1] == 'pause':state[0] = 'stopped'
                    if args[-1] == 'resume':state[0] = 'running'
                    return subprocess.CompletedProcess(args, 0,
                        ('virtual device is '+state[0]).encode(), b'')
                with patch('native_android_pause.subprocess.run', side_effect=run), \
                     patch('native_android_pause.time.sleep'):
                    result = pause_guest(['adb'], directory)
                self.assertEqual(len(reads), 2)
                self.assertEqual(result['passed'], boundary == 'success')
                self.assertEqual(state[0], 'running')
                self.assertEqual(result['commands'][-1]['name'], 'clock-after')
                last = result['commands'][-1]
                self.assertIn('FRONTCAP_CLOCK shell-entered pid=', last['stderr'])
                self.assertEqual('FRONTCAP_CLOCK uptime-read' in last['stderr'], boundary != 'uptime')
                self.assertEqual('FRONTCAP_CLOCK realtime-read' in last['stderr'], boundary == 'success')
                self.assertEqual(last['stdout'].splitlines(), [] if boundary == 'uptime' else
                                 ['12.34', '1791420000.123456789'] if boundary == 'success' else ['12.34'])
                if boundary != 'success':self.assertIn('clock-after failed', result['error'])

    def test_success_requires_confirmed_stop_and_resume(self):
        with tempfile.TemporaryDirectory() as directory:
            result, state = self.exercise(directory, '')
        self.assertTrue(result['passed'])
        self.assertEqual(state, 'running')
        self.assertNotIn('error', result)
        self.assertIn('clock-after', [row['name'] for row in result['commands']])

    def test_failed_or_lost_pause_reply_recovers_without_hiding_failure(self):
        for failure in ('pause-error', 'pause-timeout'):
            with self.subTest(failure=failure), tempfile.TemporaryDirectory() as directory:
                result, state = self.exercise(directory, failure)
            self.assertFalse(result['passed'])
            self.assertEqual(state, 'running')
            self.assertIn('pause failed', result['error'])
            self.assertNotIn('recovery_error', result)
            pause = next(row for row in result['commands'] if row['name'] == 'pause')
            self.assertIn('pause applied', pause['output'])
            self.assertEqual(result['commands'][-1]['name'], 'state-recovery')

    def test_failed_resume_retains_original_and_recovery_errors(self):
        with tempfile.TemporaryDirectory() as directory:
            result, state = self.exercise(directory, 'resume-error')
        self.assertFalse(result['passed'])
        self.assertEqual(state, 'stopped')
        self.assertIn('resume failed', result['error'])
        self.assertIn('resume-recovery failed', result['recovery_error'])
        resumes = [row for row in result['commands'] if row['name'].startswith('resume')]
        self.assertEqual([row['exit_code'] for row in resumes], [9, 9])


if __name__ == '__main__':
    unittest.main()
