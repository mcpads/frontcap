import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import subprocess
from native_volume_uikit import register, retired, register_acquisition, retired_acquisition


class UIKitVolumeTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.mount = Path(self.temp.name).resolve();self.session = self.mount / 'g-test';self.session.mkdir()
        self.diagnostic = self.session / 'worker.log.diagnostics/attempt';self.diagnostic.mkdir(parents=True)
        self.worker = dict(pid=12345, started='original');self.logger = dict(pid=12346, started='log')
        self.put(self.session/'worker.json',self.worker)
        self.put(self.session/'ready.json',dict(app=dict(pid=12347),runner=dict(pid=12348)))
        self.put(self.diagnostic/'config.json',dict(log=str(self.session/'worker.log')))
        self.put(self.diagnostic/'ready.json',self.logger)
        self.request = dict(operation='watch_uikit',session=str(self.session),worker=self.worker,
            ready_sha256=hashlib.sha256((self.session/'ready.json').read_bytes()).hexdigest())
        self.binding = register(self.mount,self.request,process_absent=lambda _:False)

    def put(self,path,value):path.write_text(json.dumps(value))

    def terminal(self):
        self.put(self.session/'result.json',dict(error=None,retirement=dict(passed=True,errors=[],signal_error=None)))
        self.put(self.diagnostic/'result.json',dict(eof=True,error='retained prefix exhausted'))

    def test_all_registered_writers_must_exit_and_original_results_must_pass(self):
        self.assertIsNone(retired(self.binding,process_absent=lambda _:True))
        self.terminal()
        for pid in [12345,12346,12347,12348]:
            self.assertIsNone(retired(self.binding,process_absent=lambda value:value!=pid))
        proof=retired(self.binding,process_absent=lambda _:True)
        self.assertEqual(proof['pids_absent'],[12345,12346,12347,12348])
        self.assertEqual(json.loads((self.diagnostic/'result.json').read_text())['error'],'retained prefix exhausted')

    def test_changed_identity_failed_retirement_and_missing_eof_never_authorize(self):
        for failure in ['worker','ready','logger','config','target','eof']:
            with self.subTest(failure=failure):
                self.setUp();self.terminal()
                if failure=='worker':self.put(self.session/'worker.json',dict(pid=999,started='replacement'))
                if failure=='ready':self.put(self.session/'ready.json',dict(app=dict(pid=999),runner=dict(pid=12348)))
                if failure=='logger':self.put(self.diagnostic/'ready.json',dict(pid=999,started='replacement'))
                if failure=='config':self.put(self.diagnostic/'config.json',dict(log='/foreign'))
                if failure=='target':self.put(self.session/'result.json',dict(retirement=dict(passed=False,errors=['unretired'])))
                if failure=='eof':self.put(self.diagnostic/'result.json',dict(eof=False))
                with self.assertRaises(ValueError):retired(self.binding,process_absent=lambda _:True)

    def test_acquisition_watch_accepts_only_original_terminal_quiescence(self):
        (self.session/'ready.json').unlink()
        request=dict(operation='watch_uikit_acquisition',session=str(self.session),worker=self.worker)
        binding=register_acquisition(self.mount,request,process_absent=lambda _:False)
        self.assertIsNone(retired_acquisition(binding,process_absent=lambda _:True))
        self.put(self.diagnostic/'result.json',dict(eof=True))
        result=dict(error='control closed before Start',retirement=None,acquisition_not_started=True)
        self.put(self.session/'result.json',result)
        for pid in [12345,12346]:
            self.assertIsNone(retired_acquisition(binding,process_absent=lambda p:p!=pid))
        self.assertEqual(retired_acquisition(binding,process_absent=lambda _:True)['result'],result)
        (self.session/'target').mkdir()
        with self.assertRaises(ValueError):retired_acquisition(binding,process_absent=lambda _:True)
        self.terminal()
        self.put(self.session/'ready.json',dict(app=dict(pid=12347),runner=dict(pid=12348)))
        self.assertEqual(retired_acquisition(binding,process_absent=lambda _:True)['pids_absent'],[12345,12346,12347,12348])
        self.assertIsNone(retired_acquisition(binding,process_absent=lambda p:p!=12347))
        self.put(self.session/'result.json',dict(error='lost',retirement=None,acquisition_not_started=False))
        with self.assertRaises(ValueError):retired_acquisition(binding,process_absent=lambda _:True)

    def test_acquisition_watch_refuses_late_registration_and_changed_owner(self):
        request=dict(operation='watch_uikit_acquisition',session=str(self.session),worker=self.worker)
        with self.assertRaises(ValueError):register_acquisition(self.mount,request,process_absent=lambda _:False)
        (self.session/'ready.json').unlink()
        binding=register_acquisition(self.mount,request,process_absent=lambda _:False)
        self.put(self.session/'worker.json',dict(pid=999,started='replacement'))
        with self.assertRaises(ValueError):retired_acquisition(binding,process_absent=lambda _:True)

    def test_registered_reader_recovers_missing_worker_result_but_never_changed_inputs(self):
        reader=self.mount/'reader';reader.write_bytes(b'original executable')
        self.put(self.session/'launch.json',dict(config=dict(resource_lifetime_ms=30000)))
        request=dict(self.request,retirement_reader=str(reader))
        binding=register(self.mount,request,process_absent=lambda _:False)
        self.put(self.diagnostic/'result.json',dict(eof=True))
        group=self.session/'target-resources';group.mkdir();(group/'resources').mkdir()
        self.put(group/'owner.json',dict(pid=12349,started='group'))
        log=group/'group.log.diagnostics/attempt';log.mkdir(parents=True)
        self.put(log/'config.json',dict(log=str(group/'group.log')))
        self.put(log/'ready.json',dict(pid=12350,started='group logger'))
        self.put(log/'result.json',dict(eof=True))
        original={str(p):p.read_bytes() for p in self.session.rglob('*') if p.is_file()}
        reply=lambda retirement:subprocess.CompletedProcess([],0,json.dumps(dict(retirement=retirement)).encode(),b'')
        with patch('native_volume_uikit.subprocess.run',return_value=reply(None)):
            self.assertIsNone(retired(binding,process_absent=lambda _:True))
        with patch('native_volume_uikit.subprocess.run',return_value=reply(dict(passed=True,errors=[],signal_error=None))):
            for live in [12349,12350]:
                self.assertIsNone(retired(binding,process_absent=lambda pid:pid!=live))
            proof=retired(binding,process_absent=lambda _:True)
            self.assertIsNone(proof['session_result_sha256'])
            self.assertTrue(proof['retirement']['passed'])
            self.assertEqual(len(proof['independent_retirement_sha256']),64)
        self.assertEqual(original,{str(p):p.read_bytes() for p in self.session.rglob('*') if p.is_file()})
        with patch('native_volume_uikit.subprocess.run',return_value=reply(dict(passed=False,errors=['pending writer']))):
            with self.assertRaises(ValueError):retired(binding,process_absent=lambda _:True)
        for changed in [reader,self.session/'launch.json']:
            original_bytes=changed.read_bytes();changed.write_bytes(b'changed')
            with patch('native_volume_uikit.subprocess.run') as execute:
                with self.assertRaises(ValueError):retired(binding,process_absent=lambda _:True)
                execute.assert_not_called()
            changed.write_bytes(original_bytes)
        # Pre-ready registration uses the same original reader without requiring fabricated readiness.
        (self.session/'ready.json').unlink()
        request=dict(operation='watch_uikit_acquisition',session=str(self.session),worker=self.worker,retirement_reader=str(reader))
        binding=register_acquisition(self.mount,request,process_absent=lambda _:False)
        with patch('native_volume_uikit.subprocess.run',return_value=reply(dict(passed=True,errors=[]))):
            proof=retired_acquisition(binding,process_absent=lambda _:True)
            self.assertIsNone(proof['result'])
            self.assertIsNone(proof['session_result_sha256'])
            self.assertTrue(proof['retirement']['passed'])


    def test_registration_rejects_foreign_path_wrong_worker_and_missing_live_owner(self):
        for change in [dict(session=str(self.mount)),dict(worker=dict(pid=999,started='foreign')),dict(ready_sha256='0'*64)]:
            with self.assertRaises(ValueError):register(self.mount,dict(self.request,**change),process_absent=lambda _:False)
        with self.assertRaises(ValueError):register(self.mount,self.request,process_absent=lambda _:True)


if __name__=='__main__':unittest.main()
