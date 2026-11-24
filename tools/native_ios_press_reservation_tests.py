"""Only the bound, committed UI rejection can release an unused clock number."""
from dataclasses import asdict
import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from native_ios_clock_bridge import Bridge as ClockBridge
from native_ios_ui_bridge import Bridge as UiBridge


class PressReservationTests(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name);self.calls=[]
        self.runner=dict(simulator='abcd0000-0000-0000-0000-000000000001',
            generation='abcd0000-0000-0000-0000-000000000002',pid=123)
        self.app=dict(simulator=self.runner['simulator'],generation='abcd0000-0000-0000-0000-000000000003',pid=124)
        self.endpoint=dict(generation=self.app['generation'],pid=124,port=1234)
        self.clock_descriptor=self.root/'clock.json'
        self.clock_descriptor.write_text(json.dumps(dict(self.endpoint,status='ready',last_id=0)))
        self.ui_descriptor=self.root/'ui.json'
        self.ui_descriptor.write_text(json.dumps(dict(generation=self.runner['generation'],pid=123,port=1235,status='ready',last_id=0)))
        self.observation='abcd0000-0000-0000-0000-000000000004'
        self.catalog=dict(observation=self.observation,keyboard=False,transitioning=False,scrolls=[],text_input=dict(state='unfocused'),
            targets=[dict(role='text_field',type=49,identifier='title',label='Title',value='',selector_key='identifier',
                selector_value='title',unique=True,enabled=True,selected=False,hittable=True,intersects_screen=True,
                frame_mpt=[0,0,1000,1000],reference=self.observation+'-0')])
        self.artifacts=[]
        for name in ('screen.png','hierarchy.txt'):
            relative='observations/'+self.observation+'/'+name
            path=self.root/relative;path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(name.encode())
            self.artifacts.append(dict(path=relative,bytes=len(name),sha256=hashlib.sha256(name.encode()).hexdigest()))
        self.clock=ClockBridge(self.clock_descriptor,self.root/'clock-client',transport=self.clock_transport)
        self.ui=UiBridge(self.ui_descriptor,self.root/'ui-client',self.ui_transport,generic=True)
        self.clock.handle(dict(call='initialize',identity=self.app,source_sha256='a'*64))
        self.ui.handle(dict(call='initialize',identity=self.runner,source_sha256='a'*64))
        self.ui_call(1,'launch',{},app=None);self.ui_call(2,'observe_targets',{})
        self.request=dict(generation=self.app['generation'],request_id=1,operation='advance',delta_ns=70000000)
        self.arguments=dict(reference=self.observation+'-0',observation=self.observation,clock_request=self.request)
        self.reserve=dict(call='reserve_ui',identity=self.app,request=self.request,ui_identity=self.runner,
            ui_request=dict(generation=self.runner['generation'],request_id=3,operation='press_target',arguments=self.arguments))

    def clock_transport(self,*args,**kwargs):
        raise AssertionError('Reservation/release must not contact the clock')

    def ui_transport(self,endpoint,request,**options):
        self.calls.append(request.request_id)
        if request.operation=='launch':result=dict(state='launched',reads=dict(endpoint=self.endpoint,now_ns=0))
        elif request.operation=='observe_targets':
            result=dict(state='observed',app=self.endpoint,now_ns=0,catalog=self.catalog,artifacts=self.artifacts)
        else:result=dict(input='not_applied',app=self.endpoint,**request.arguments,reason='observation_changed')
        reply=dict(request=asdict(request),generation=self.runner['generation'],pid=123,port=1235,
            last_id=request.request_id,status='ok',result=result,replayed=False)
        self.ui_descriptor.write_text(json.dumps(reply));return reply

    def ui_call(self,number,operation,arguments,**options):
        return self.ui.handle(dict(call='execute',identity=self.runner,app=options.get('app',self.app),
            request=dict(generation=self.runner['generation'],request_id=number,operation=operation,arguments=arguments)))

    def reject(self):
        result=self.ui_call(3,'press_target',self.arguments)
        return dict(self.reserve,call='release_ui',receipt_sha256=result['completion']['receipt_sha256'])

    def state(self):
        return (self.root/'clock-client/clock/clock-client.json').read_bytes()

    def test_rejection_releases_without_consuming_clock_and_old_proof_cannot_release_new_ui(self):
        reserved=self.clock.handle(self.reserve)
        self.assertEqual(reserved['reply'],'ui_reserved')
        self.assertEqual(self.clock.handle(self.reserve),reserved)
        with self.assertRaises(RuntimeError):
            self.clock.handle(dict(call='reserve',identity=self.app,request=self.request))
        release=self.reject();reply=self.clock.handle(release)
        state=json.loads(self.state());self.assertIsNone(state['pending']);self.assertEqual(state['next_id'],1)
        self.assertEqual(reply['reply'],'ui_released');self.assertEqual(self.clock.handle(release),reply)
        with self.assertRaises(RuntimeError):self.clock.handle(self.reserve)
        self.ui_call(4,'observe_targets',{})
        later=dict(self.reserve,ui_request=dict(self.reserve['ui_request'],request_id=5))
        self.clock.handle(later);before=self.state()
        with self.assertRaises(RuntimeError):self.clock.handle(release)
        self.assertEqual(self.state(),before)
        self.assertEqual(self.calls,[1,2,3,4])

    def test_lost_release_publication_reply_recovers_without_reusing_proof_for_another_reservation(self):
        self.clock.handle(self.reserve);release=self.reject()
        def lost(directory,phase,request,**options):
            if phase=='after-release':raise EOFError('release reply lost')
        with patch('native_ios_control.checkpoint',side_effect=lost),self.assertRaises(EOFError):
            self.clock.handle(release)
        reopened=ClockBridge(self.clock_descriptor,self.root/'clock-client',transport=self.clock_transport)
        self.assertEqual(reopened.handle(release)['reply'],'ui_released')
        self.assertIsNone(json.loads(self.state())['pending']);self.assertEqual(self.calls,[1,2,3])

    def test_release_failure_before_publication_keeps_the_original_fence(self):
        self.clock.handle(self.reserve);release=self.reject();before=self.state()
        def failed(directory,phase,request,**options):
            if phase=='before-release':raise OSError('storage unavailable')
        with patch('native_ios_control.checkpoint',side_effect=failed),self.assertRaises(OSError):
            self.clock.handle(release)
        self.assertEqual(self.state(),before)
        self.assertEqual(self.clock.handle(release)['reply'],'ui_released')

    def test_missing_changed_or_uncommitted_proof_and_clock_progress_keep_reservation(self):
        self.clock.handle(self.reserve);release=self.reject();before=self.state()
        proof=self.root/'ui-client/receipts'/(release['receipt_sha256']+'.json');original=proof.read_bytes()
        cursor=self.root/'ui-client/ui/ui-client.json';ui_state=json.loads(cursor.read_text())
        for defect in ('bytes','pending','owner','clock_progress'):
            with self.subTest(defect=defect):
                if defect=='bytes':proof.write_bytes(original+b' ')
                if defect=='pending':cursor.write_text(json.dumps(dict(ui_state,pending=self.reserve['ui_request'])))
                call=release
                if defect=='owner':call=dict(release,ui_identity=dict(self.runner,pid=999))
                if defect=='clock_progress':
                    self.clock_descriptor.write_text(json.dumps(dict(self.endpoint,status='executing',last_id=1)))
                with self.assertRaises(RuntimeError):self.clock.handle(call)
                self.assertEqual(self.state(),before)
                proof.write_bytes(original);cursor.write_text(json.dumps(ui_state))
        self.assertEqual(self.calls,[1,2,3])

    def test_preflight_rejection_cannot_create_a_reservation(self):
        call=dict(self.reserve,ui_request=dict(self.reserve['ui_request'],request_id=4))
        before=self.state()
        with self.assertRaises(RuntimeError):self.clock.handle(call)
        self.assertEqual(self.state(),before)

    def test_successful_press_receipt_is_never_unused_clock_proof(self):
        self.clock.handle(self.reserve);release=self.reject();before=self.state()
        fixture=Path(__file__).resolve().parents[1]/'crates/core/tests/fixtures/uikit-press.json'
        result=json.loads(fixture.read_text())
        result.update(self.arguments,app=self.endpoint)
        for name in ('before','began','ended'):result[name]['generation']=self.app['generation']
        result['clock_reply'].update(request=self.request,generation=self.app['generation'],
            pid=self.app['pid'],port=self.endpoint['port'],last_id=1)
        cursor=self.root/'ui-client/ui/ui-client.json';state=json.loads(cursor.read_text())
        state['last_reply']['result']=result
        raw=json.dumps(state['last_reply']).encode();digest=hashlib.sha256(raw).hexdigest()
        (self.root/'ui-client/receipts'/(digest+'.json')).write_bytes(raw)
        cursor.write_text(json.dumps(state))
        # The receipt itself is valid; it simply proves delivery, not a no-effect rejection.
        from native_ios_generic_ui import UIKitUIRequest
        from native_ios_ui import validate_reply
        validate_reply(state['last_reply'],UIKitUIRequest(**self.reserve['ui_request']))
        with self.assertRaisesRegex(RuntimeError,'no-effect proof'):
            self.clock.handle(dict(release,receipt_sha256=digest))
        self.assertEqual(self.state(),before)


if __name__=='__main__':unittest.main()
