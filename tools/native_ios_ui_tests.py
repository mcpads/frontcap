"""Unknown input outcomes must survive controller recreation without another tap."""
from dataclasses import asdict
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch, Mock

from native_ios_app import run, find_ui, scenario
from native_ios_ui import UIClient, UIRequest, validate_reply
from native_clock_control import recorded_outcome


class IOSUITests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name)
        self.endpoint=dict(generation='33557076-a359-4a02-a088-d946cd77e7c3',port=12345,status='ready',last_id=0)
        self.descriptor=self.root/'ui.json';self.descriptor.write_text(json.dumps(self.endpoint))
        self.calls=[];self.effects=[];self.cache={}

    def transport(self,endpoint,request,**options):
        self.calls.append(request)
        if request.request_id in self.cache:
            return {**self.cache[request.request_id],'replayed':True}
        self.effects.append(asdict(request))
        result=dict(input='xctest-tap',target=request.arguments['target'])
        reply=dict(status='ok',generation=self.endpoint['generation'],last_id=request.request_id,
                   request=asdict(request),result=result,replayed=False)
        self.cache[request.request_id]=reply
        return reply

    def client(self,transport=None):
        return UIClient(self.descriptor,self.root/'control',transport or self.transport)

    def state(self):
        return json.loads((self.root/'control/ui-client.json').read_text())

    def test_lost_tap_reply_blocks_new_input_and_resumes_only_the_saved_request(self):
        def lose(endpoint,request,**options):
            self.transport(endpoint,request)
            raise EOFError('lost after tap')
        with self.assertRaises(EOFError): self.client(lose).request('tap',target='native-hold-work')
        with self.assertRaisesRegex(RuntimeError,'Pending UI command'): self.client().request('tap',target='native-hold-work')
        reply=self.client().resume()
        self.assertTrue(reply['replayed'])
        self.assertEqual(self.calls[0],self.calls[1])
        self.assertEqual(len(self.effects),1)
        self.assertIsNone(self.state()['pending'])
        self.client().request('tap',target='native-hold-work')
        self.assertEqual(len(self.effects),2)

    def test_wrong_reply_then_replaced_bridge_keeps_old_input_unresolved(self):
        def wrong(endpoint,request,**options):
            reply=self.transport(endpoint,request).copy();reply['result']=dict(input='xctest-tap',target='native-increment')
            return reply
        with self.assertRaisesRegex(RuntimeError,'does not confirm'): self.client(wrong).request('tap',target='native-hold-work')
        saved=self.state()['pending']
        self.endpoint['generation']='00000000-0000-0000-0000-000000000000'
        self.descriptor.write_text(json.dumps(self.endpoint))
        with self.assertRaisesRegex(RuntimeError,'generation changed'): self.client().resume()
        self.assertEqual(self.state()['pending'],saved)
        self.assertEqual(len(self.calls),1)

    def test_terminal_receipt_recovers_pending_input_without_transport(self):
        def lose(endpoint, request, **options):
            reply = self.transport(endpoint, request)
            self.descriptor.write_text(json.dumps(reply)) # No live port is needed for recovery.
            raise EOFError('XCTest died after saving its receipt')
        with self.assertRaises(EOFError): self.client(lose).request('tap', target='native-increment')
        unavailable = Mock(side_effect=AssertionError('Recovery must not connect'))
        reply = self.client(unavailable).recover()
        self.assertEqual(reply, self.cache[1])
        self.assertIsNone(self.state()['pending'])
        self.assertEqual(recorded_outcome(self.root/'control', validate_reply, request_type=UIRequest, prefix='ui'), reply)
        unavailable.assert_not_called()
        self.assertEqual(len(self.effects), 1)

    def test_unconfirmed_receipts_preserve_pending_input_and_never_retransmit(self):
        with self.assertRaises(EOFError):
            self.client(Mock(side_effect=EOFError())).request('tap', target='native-increment')
        original = self.state()
        terminal = self.transport(self.endpoint, UIRequest(**original['pending']))
        for damage in ('executing', 'rejected', 'generation', 'request', 'target', 'last_id'):
            with self.subTest(damage=damage):
                receipt = json.loads(json.dumps(terminal))
                if damage in ('executing', 'rejected'): receipt['status'] = damage
                elif damage == 'generation': receipt['generation'] = '00000000-0000-0000-0000-000000000000'
                elif damage == 'request': receipt['request']['request_id'] += 1
                elif damage == 'target': receipt['result']['target'] = 'native-hold-work'
                else: receipt['last_id'] = True
                self.descriptor.write_text(json.dumps(receipt))
                unavailable = Mock(side_effect=AssertionError('Do not connect'))
                with self.assertRaises(RuntimeError): self.client(unavailable).recover()
                self.assertEqual(self.state(), original)
                unavailable.assert_not_called()

    def test_invalid_input_or_capture_path_is_rejected_before_transmission(self):
        for operation,arguments in [('tap',dict(target='unowned')),('tap',dict(target='native-increment',extra=1)),
                                    ('capture',dict(name='../overwrite',clock='Time: 0',text='Count: 0')),
                                    ('finish',dict(force=True)), ('terminate',dict(force=True)),
                                    ('tap',dict(target='native-increment',stop_at='unknown')),
                                    ('launch',dict(clock_stop_at='unknown'))]:
            with self.subTest(operation=operation,arguments=arguments), self.assertRaises(ValueError):
                self.client().request(operation,**arguments)
        self.assertEqual(self.calls,[])

    def test_committed_tap_result_survives_endpoint_loss_without_repeating_input(self):
        reply=self.client().request('tap',target='native-increment')
        self.descriptor.unlink()
        command=[sys.executable,str(Path(__file__).with_name('native_ios_ui.py')),
                 '--descriptor',str(self.descriptor),'--directory',str(self.root/'control'),'outcome']
        result=subprocess.run(command,env={**os.environ,'PATH':''},capture_output=True,text=True,timeout=5)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertEqual(json.loads(result.stdout),reply)
        self.assertEqual(len(self.effects),1)
        self.assertEqual(self.state()['last_reply'],reply)
        original=self.state()
        for defect in ('pending','target','number','generation'):
            with self.subTest(defect=defect):
                state=json.loads(json.dumps(original))
                if defect=='pending': state['pending']=reply['request']
                if defect=='target': state['last_reply']['result']['target']='native-hold-work'
                if defect=='number': state['next_id']+=1
                if defect=='generation': state['generation']='00000000-0000-0000-0000-000000000000'
                (self.root/'control/ui-client.json').write_text(json.dumps(state))
                with self.assertRaises(RuntimeError):
                    recorded_outcome(self.root/'control',validate_reply,request_type=UIRequest,prefix='ui')

    def test_host_failure_keeps_original_error_and_terminates_its_xcodebuild(self):
        for evidence_fails in (False,True):
            with self.subTest(evidence_fails=evidence_fails):
                process=Mock(pid=42);process.poll.return_value=None;process.wait.return_value=-15
                with patch('native_ios_app.subprocess.Popen',return_value=process), \
                     patch('native_ios_app.find_ui',return_value=self.descriptor), \
                     patch('native_ios_app.scenario',side_effect=RuntimeError('unresolved tap')):
                    if evidence_fails:
                        with patch('native_ios_app.atomic_json',side_effect=OSError('storage unavailable')), \
                             self.assertRaisesRegex(RuntimeError,'unresolved tap'):
                            run('owned',self.root,['xcodebuild'],200000)
                    else:
                        with self.assertRaisesRegex(RuntimeError,'unresolved tap'):
                            run('owned',self.root,['xcodebuild'],200000)
                        self.assertIn('unresolved tap',json.loads((self.root/'host-workflow-error.json').read_text())['error'])
                process.terminate.assert_called_once()
                process.wait.assert_called_once_with(timeout=10)

    def test_delayed_ui_endpoint_uses_resolved_directory_when_discovery_becomes_unavailable(self):
        descriptor=self.root/'Documents/native-proof/ui-control.json'
        descriptor.parent.mkdir(parents=True)
        process=Mock();process.poll.return_value=None
        def publish(_): descriptor.write_text(json.dumps(self.endpoint))
        with patch('native_ios_app.app_container',side_effect=[self.root,OSError('discovery unavailable')]), \
             patch('native_ios_app.time.sleep',side_effect=publish):
            self.assertEqual(find_ui('owned',process,self.root/'evidence'),descriptor)

    def test_workflow_reuses_installation_directory_but_reads_new_endpoint_contents(self):
        for installation in ('first', 'second'):
            with self.subTest(installation=installation):
                app = self.root/installation
                documents = app/'Documents'
                documents.mkdir(parents=True)
                endpoint = documents/'clock-control.json'
                endpoint.write_text('old generation')

                def lifecycle(simulator, out, ui, container):
                    resolved = container(simulator, 'dev.frontcap.s18.probe')/'Documents/clock-control.json'
                    self.assertEqual(resolved.read_text(), 'old generation')
                    endpoint.write_text('replacement generation')
                    again = container(simulator, 'dev.frontcap.s18.probe')/'Documents/clock-control.json'
                    self.assertEqual(again.read_text(), 'replacement generation')
                    raise RuntimeError('stop after directory reuse control')

                with patch('native_ios_app.app_container',side_effect=[app,OSError('discovery unavailable')]), \
                     patch('native_ios_app.lifecycle',side_effect=lifecycle), \
                     self.assertRaisesRegex(RuntimeError,'stop after directory reuse control'):
                    scenario('owned',self.root,self.descriptor,200000)

    def test_replacement_discovery_waits_for_a_new_generation(self):
        descriptor=self.root/'Documents/native-proof/ui-control.json'
        descriptor.parent.mkdir(parents=True)
        descriptor.write_text(json.dumps(dict(self.endpoint, status='executing', last_id=1)))
        process=Mock();process.poll.return_value=None
        replacement=dict(self.endpoint,generation='00000000-0000-0000-0000-000000000001')
        with patch('native_ios_app.app_container',return_value=self.root), \
             patch('native_ios_app.time.sleep',side_effect=lambda _: descriptor.write_text(json.dumps(replacement))):
            self.assertEqual(find_ui('owned',process,self.root, self.endpoint['generation']),descriptor)

    def test_reinstalled_runner_does_not_leave_discovery_polling_the_old_container(self):
        old=self.root/'old/Documents/native-proof/ui-control.json'
        new=self.root/'new/Documents/native-proof/ui-control.json'
        for path in (old,new): path.parent.mkdir(parents=True)
        old.write_text(json.dumps(dict(self.endpoint,status='executing')))
        new.write_text(json.dumps(dict(self.endpoint,generation='00000000-0000-0000-0000-000000000001')))
        process=Mock();process.poll.return_value=None
        with patch('native_ios_app.app_container',side_effect=[self.root/'old', self.root/'new']) as lookup, \
             patch('native_ios_app.time.monotonic',side_effect=range(100)), patch('native_ios_app.time.sleep'):
            self.assertEqual(find_ui('owned',process,self.root,self.endpoint['generation']),new)
        self.assertEqual(lookup.call_count,2)


if __name__=='__main__': unittest.main()
