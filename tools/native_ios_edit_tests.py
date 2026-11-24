import copy
import json
import hashlib
import tempfile
from dataclasses import asdict
from pathlib import Path
import unittest
from native_ios_generic_ui import UIKitUIRequest,validate_generic_result
from native_ios_edit import validate_target
from native_ios_ui_bridge import Bridge


class EditTests(unittest.TestCase):
    def setUp(self):
        self.receipt=json.loads((Path(__file__).resolve().parents[1]/'crates/core/tests/fixtures/uikit-edit.json').read_text())
        self.observation='abcd0000-0000-0000-0000-000000000001';self.reference=self.observation+'-0'
        self.request=UIKitUIRequest('abcd0000-0000-0000-0000-000000000002',3,'replace_text',
            dict(reference=self.reference,observation=self.observation,start=0,end=0,text='Field 🌟'))
        self.result=dict(input='uikit_text_control',reference=self.reference,observation=self.observation,now_ns=570000000,
            app=dict(generation='abcdef00-0000-4000-8000-000000000001',pid=self.receipt['pid'],port=41001),receipt=self.receipt)

    def test_source_receipt_binds_owner_request_and_exact_before_state(self):
        validate_generic_result(self.result,self.request)
        for path,value in [(('pid',),1),(('replayed',),True),(('last_id',),2),
                           (('request','arguments','text'),'different'),(('result','before','text'),'changed')]:
            with self.subTest(path=path),self.assertRaises(ValueError):
                bad=copy.deepcopy(self.result);node=bad['receipt']
                for key in path[:-1]:node=node[key]
                node[path[-1]]=value;validate_generic_result(bad,self.request)
        self.result['receipt']['result']['after']=dict(state='unfocused')
        validate_generic_result(self.result,self.request)

    def test_observed_range_is_utf16_and_requires_the_same_committed_focus(self):
        catalog=dict(transitioning=False,text_input=self.receipt['result']['after'],targets=[dict(reference=self.reference,
            role='text_field',unique=True,enabled=True,hittable=True,selector_key='label',selector_value='Field')])
        args=dict(reference=self.reference,observation=self.observation,start=3,end=5,text='주말 여행')
        validate_target(catalog,args)
        for change in (dict(start=7,end=7,text='x'),dict(start=8,end=8,text='가'*16384),dict(start=True)):
            with self.assertRaises(ValueError):validate_target(catalog,dict(args,**change))
        for changes in (dict(label='Body'),dict(marked=True)):
            bad=copy.deepcopy(catalog);bad['text_input'].update(changes)
            with self.assertRaises(ValueError):validate_target(bad,args)

    def test_bridge_recovers_published_edit_once_and_rejects_a_different_source_basis(self):
        for changed in (False,True):
            with self.subTest(changed_basis=changed),tempfile.TemporaryDirectory() as temporary:
                root=Path(temporary);descriptor=root/'ui.json';calls=[]
                runner=dict(simulator='abcd0000-0000-0000-0000-000000000005',generation=self.request.generation,pid=123)
                app=dict(self.result['app'],simulator=runner['simulator']);app.pop('port')
                descriptor.write_text(json.dumps(dict(generation=runner['generation'],pid=123,port=1234,status='ready',last_id=0)))
                artifacts=[]
                for name in ('screen.png','hierarchy.txt'):
                    relative='observations/'+self.observation+'/'+name;p=root/relative
                    p.parent.mkdir(parents=True,exist_ok=True);p.write_bytes(name.encode())
                    artifacts.append(dict(path=relative,bytes=len(name),sha256=hashlib.sha256(name.encode()).hexdigest()))
                catalog=dict(observation=self.observation,keyboard=False,transitioning=False,scrolls=[],text_input=self.receipt['request']['arguments']['basis'],
                    targets=[dict(role='text_field',type=49,identifier='',label='Field',value='',selector_key='label',selector_value='Field',
                        unique=True,enabled=True,selected=False,hittable=True,intersects_screen=True,frame_mpt=[0,0,1000,1000],reference=self.reference)])
                def transport(endpoint,request,**options):
                    calls.append(request.request_id)
                    if request.operation=='launch':result=dict(state='launched',reads=dict(endpoint=self.result['app'],now_ns=570000000))
                    elif request.operation=='observe_targets':result=dict(state='observed',app=self.result['app'],now_ns=570000000,catalog=catalog,artifacts=artifacts)
                    else:
                        result=copy.deepcopy(self.result)
                        if changed:
                            for state in (result['receipt']['request']['arguments']['basis'],result['receipt']['result']['before']):
                                state['token']='abcd0000-0000-0000-0000-000000000006'
                    reply=dict(request=asdict(request),generation=runner['generation'],pid=123,port=1234,last_id=request.request_id,status='ok',result=result,replayed=False)
                    descriptor.write_text(json.dumps(reply))
                    if request.operation=='replace_text':raise EOFError('published response lost')
                    return reply
                bridge=Bridge(descriptor,root/'client',transport,generic=True)
                bridge.handle(dict(call='initialize',identity=runner,source_sha256='a'*64))
                for number,operation in ((1,'launch'),(2,'observe_targets')):
                    bridge.handle(dict(call='execute',identity=runner,app=None if number==1 else app,
                        request=dict(generation=runner['generation'],request_id=number,operation=operation,arguments={})))
                call=dict(call='execute',identity=runner,app=app,request=asdict(self.request))
                if changed:
                    with self.assertRaisesRegex(RuntimeError,'retained observation basis'):bridge.handle(call)
                    self.assertTrue((root/'client/pending.json').exists())
                else:
                    result=bridge.handle(call)
                    self.assertEqual(result['completion']['outcome']['outcome'],'text_edit')
                    reopened=Bridge(descriptor,root/'client',transport,generic=True)
                    self.assertEqual(reopened.handle(dict(call,call='recover')),result)
                    self.assertEqual(reopened.handle(call),result)
                self.assertEqual(calls,[1,2,3])


if __name__=='__main__':unittest.main()
