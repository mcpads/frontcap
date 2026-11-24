import copy
import json
import hashlib
import tempfile
from dataclasses import asdict
from pathlib import Path
import unittest
from native_ios_generic_ui import UIKitUIRequest,validate_generic_result
from native_ios_scroll import allows,validate_scroll
from native_ios_ui_bridge import Bridge

class ScrollTests(unittest.TestCase):
    def setUp(self):
        self.observation='abcd0000-0000-0000-0000-000000000001';self.reference=self.observation+'-0'
        self.before=dict(token='abcd0000-0000-0000-0000-000000000007',role='table',identifier='list',label='',
            offset_mpt=[0,-10000],minimum_mpt=[0,-10000],maximum_mpt=[0,1000000],viewport_mpt=[300000,600000],
            enabled=True,dragging=False,decelerating=False)
        self.request=UIKitUIRequest('abcd0000-0000-0000-0000-000000000002',3,'scroll_target',
            dict(reference=self.reference,observation=self.observation,direction='down'))
        self.result=dict(input='xctest_scroll',reference=self.reference,observation=self.observation,now_ns=570000000,
            app=dict(generation='7cef9126-b295-4785-82a9-9cd28af35aa0',pid=124,port=63758),direction='down',
            before=self.before,after=dict(self.before,offset_mpt=[0,600000]))

    def test_source_bounds_reject_motion_and_preserve_zero_movement(self):
        validate_generic_result(self.result,self.request)
        self.assertTrue(allows(self.before,'down'));self.assertFalse(allows(self.before,'up'))
        self.assertFalse(allows(dict(self.before,decelerating=True),'down'))
        self.assertFalse(allows(dict(self.before,offset_mpt=[0,1000000]),'down'))
        validate_generic_result(dict(self.result,after=self.before),self.request)
        with self.assertRaises(ValueError):validate_scroll(dict(self.before,minimum_mpt=[0,1000001]))
        with self.assertRaises(ValueError):validate_scroll(dict(self.before,enabled=1))
        with self.assertRaises(ValueError):validate_generic_result(dict(self.result,direction='up'),self.request)

    def test_bridge_recovers_published_swipe_once_and_rejects_changed_source_basis(self):
        for changed,unavailable in ((False,False),(True,False),(False,True)):
            with self.subTest(changed_basis=changed,unavailable=unavailable),tempfile.TemporaryDirectory() as temporary:
                root=Path(temporary);descriptor=root/'ui.json';calls=[]
                runner=dict(simulator='abcd0000-0000-0000-0000-000000000005',generation=self.request.generation,pid=123)
                app=dict(self.result['app'],simulator=runner['simulator']);app.pop('port')
                descriptor.write_text(json.dumps(dict(generation=runner['generation'],pid=123,port=1234,status='ready',last_id=0)))
                artifacts=[]
                for name in ('screen.png','hierarchy.txt'):
                    relative='observations/'+self.observation+'/'+name;p=root/relative
                    p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(name.encode())
                    artifacts.append(dict(path=relative,bytes=len(name),sha256=hashlib.sha256(name.encode()).hexdigest()))
                catalog=dict(observation=self.observation,keyboard=False,transitioning=False,scrolls=[self.before],text_input=dict(state='unfocused'),
                    targets=[dict(role='table',type=26,identifier='list',label='',value=None,selector_key='identifier',selector_value='list',
                        unique=True,enabled=True,selected=False,hittable=True,intersects_screen=True,frame_mpt=[0,0,1000,1000],reference=self.reference)])
                def transport(endpoint,request,**options):
                    calls.append(request.request_id)
                    if request.operation=='launch':result=dict(state='launched',reads=dict(endpoint=self.result['app'],now_ns=570000000))
                    elif request.operation=='observe_targets':
                        result=(dict(state='unavailable',app=self.result['app'],now_ns=570000000,reason='scroll_in_motion',artifacts=[])
                            if unavailable and request.request_id==2 else dict(state='observed',app=self.result['app'],now_ns=570000000,catalog=catalog,artifacts=artifacts))
                    else:
                        result=copy.deepcopy(self.result)
                        if changed:
                            for state in (result['before'],result['after']):
                                state['token']='abcd0000-0000-0000-0000-000000000006'
                    reply=dict(request=asdict(request),generation=runner['generation'],pid=123,port=1234,last_id=request.request_id,status='ok',result=result,replayed=False)
                    descriptor.write_text(json.dumps(reply))
                    if request.operation=='scroll_target':raise EOFError('published response lost')
                    return reply
                bridge=Bridge(descriptor,root/'client',transport,generic=True)
                bridge.handle(dict(call='initialize',identity=runner,source_sha256='a'*64))
                for number,operation in ((1,'launch'),(2,'observe_targets')):
                    bridge.handle(dict(call='execute',identity=runner,app=None if number==1 else app,
                        request=dict(generation=runner['generation'],request_id=number,operation=operation,arguments={})))
                request=asdict(self.request)
                if unavailable:
                    denied=dict(call='execute',identity=runner,app=app,request=request)
                    with self.assertRaisesRegex(RuntimeError,'usable catalog'):bridge.handle(denied)
                    self.assertEqual(calls,[1,2])
                    read=dict(call='execute',identity=runner,app=app,request=dict(generation=runner['generation'],request_id=2,operation='observe_targets',arguments={}))
                    self.assertEqual(bridge.handle(read)['completion']['outcome']['outcome'],'observation_unavailable')
                    read['request']['request_id']=3
                    self.assertEqual(bridge.handle(read)['completion']['outcome']['outcome'],'observed_targets')
                    request['request_id']=4
                call=dict(call='execute',identity=runner,app=app,request=request)
                if changed:
                    with self.assertRaisesRegex(RuntimeError,'retained observation basis'):bridge.handle(call)
                    self.assertTrue((root/'client/pending.json').exists())
                else:
                    result=bridge.handle(call)
                    self.assertEqual(result['completion']['outcome']['outcome'],'scrolled')
                    reopened=Bridge(descriptor,root/'client',transport,generic=True)
                    self.assertEqual(reopened.handle(dict(call,call='recover')),result)
                    self.assertEqual(reopened.handle(call),result)
                self.assertEqual(calls,[1,2,3,4] if unavailable else [1,2,3])


if __name__=='__main__':unittest.main()
