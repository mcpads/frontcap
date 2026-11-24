"""Discovery failures retain command evidence and reap the owned command."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from native_ios_discovery import app_container


class IOSDiscoveryTests(unittest.TestCase):
    def test_success_nonzero_exit_and_timeout_keep_output_and_process_identity(self):
        create = subprocess.Popen
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for mode in ('success', 'error', 'timeout', 'timeout-diagnostic-error'):
                with self.subTest(mode=mode):
                    children=[]
                    def launch(args, **kwargs):
                        script='import sys,time;print(sys.argv[1],flush=True);print("diagnostic",file=sys.stderr,flush=True);'
                        script+= 'time.sleep(10)' if mode.startswith('timeout') else 'sys.exit(7)' if mode=='error' else ''
                        process=create([sys.executable,'-c',script,str(root)],**kwargs)
                        children.append(process)
                        return process
                    folder=root/mode
                    with patch('native_ios_discovery.subprocess.Popen',side_effect=launch), \
                         patch('native_ios_discovery.capture_sample',return_value=dict(sampled_threads=1),
                               side_effect=OSError('sample unavailable') if mode.endswith('error') and mode.startswith('timeout') else None) as sample, \
                         patch('native_ios_discovery.host_context',return_value=dict(observed=True)):
                        if mode=='success': self.assertEqual(app_container('owned','bundle',folder,2),root)
                        else:
                            error=subprocess.TimeoutExpired if mode.startswith('timeout') else subprocess.CalledProcessError
                            with self.assertRaises(error): app_container('owned','bundle',folder,2)
                    events=[json.loads(line) for line in (folder/'simulator-commands.jsonl').read_text().splitlines()]
                    first,final=events[0],events[-1]
                    self.assertEqual(first['phase'],'started');self.assertEqual(final['phase'],'finished')
                    self.assertEqual(first['attempt'],final['attempt'])
                    self.assertEqual(final['pid'],children[0].pid)
                    self.assertEqual(final['exit_code'],0 if mode=='success' else 7 if mode=='error' else -9)
                    self.assertEqual(final['stdout'],str(root)+'\n');self.assertEqual(final['stderr'],'diagnostic\n')
                    self.assertGreaterEqual(final['finished_monotonic_ns'],first['started_monotonic_ns'])
                    self.assertTrue(all(child.poll() is not None for child in children))
                    if mode!='success': self.assertEqual(final['error_type'],error.__name__)
                    if mode.startswith('timeout'):
                        self.assertEqual([row['phase'] for row in events],['started','spawned','timed_out','finished'])
                        self.assertEqual(sample.call_args.args[0],children[0].pid)
                        self.assertEqual(sample.call_args.args[2],3)
                        if mode=='timeout':
                            diagnostic=json.loads((folder/final['diagnostic']).read_text())
                            self.assertEqual(diagnostic['attempt'],first['attempt'])
                            self.assertEqual(diagnostic['pid'],children[0].pid)
                            self.assertEqual(diagnostic['sample']['sampled_threads'],1)
                        else:
                            self.assertIn('sample unavailable',final['diagnostic_error'])
                    else: sample.assert_not_called()


if __name__=='__main__': unittest.main()
