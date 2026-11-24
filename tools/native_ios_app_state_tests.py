"""Host receipt and lost-reply checks; no Simulator behavior claim."""
from dataclasses import asdict
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest import TestCase, main
import copy, json
from native_ios_ui_bridge import Bridge
from native_ios_generic_ui import UIKitUIRequest as Request
from native_ios_app_state import validate_result

class AppStateTests(TestCase):
    def setUp(self):
        temp=TemporaryDirectory();self.addCleanup(temp.cleanup);self.root=Path(temp.name)
        self.descriptor=self.root/'endpoint.json';self.directory=self.root/'client'
        self.runner=dict(simulator='abcd0000-0000-0000-0000-000000000001',generation='abcd0000-0000-0000-0000-000000000002',pid=123)
        self.app={**self.runner,'generation':'abcd0000-0000-0000-0000-000000000003','pid':124}
        self.endpoint=dict(generation=self.app['generation'],pid=124,port=1234)
        self.raw=dict(state='app_state_changed',app=self.endpoint,change=dict(requested='background',before_state=4,app_state=2,now_ns=None))
        self.calls=[];self.lose=False
        self.descriptor.write_text(json.dumps(dict(generation=self.runner['generation'],pid=123,port=1,status='ready',last_id=0)))
        def transport(endpoint,request,**kwargs):
            self.calls.append(request)
            result=dict(state='launched',reads=dict(endpoint=self.endpoint,now_ns=0)) if request.operation=='launch' else self.raw
            reply=dict(generation=request.generation,pid=123,port=1,status='ok',last_id=request.request_id,request=asdict(request),result=result)
            self.descriptor.write_text(json.dumps(reply))
            if self.lose:raise EOFError('reply lost after host transition')
            return reply
        self.bridge=Bridge(self.descriptor,self.directory,transport,generic=True)
        self.bridge.handle(dict(call='initialize',identity=self.runner,source_sha256='a'*64))
        self.bridge.handle(dict(call='execute',identity=self.runner,app=None,request=dict(generation=self.runner['generation'],request_id=1,operation='launch',arguments={})))
    def call(self,number,state):
        return dict(call='execute',identity=self.runner,app=self.app,request=dict(generation=self.runner['generation'],request_id=number,operation='set_app_state',arguments=dict(app=self.app,state=state)))
    def test_lost_reply_recovers_original_transition_without_redispatch(self):
        self.lose=True;request=self.call(2,'background')
        result=self.bridge.handle(request)
        self.assertEqual(result['completion']['outcome'],dict(outcome='app_state_changed',app=self.app,change=self.raw['change']))
        self.descriptor.unlink()
        self.assertEqual(self.bridge.handle({**request,'call':'recover'}),result)
        self.assertEqual(len(self.calls),2)
        with self.assertRaises(RuntimeError):self.bridge.handle({**self.call(2,'foreground'),'call':'recover'})
        self.descriptor.write_text(json.dumps(dict(generation=self.runner['generation'],pid=123,port=1,status='ready',last_id=2)))
        self.raw['change']=dict(requested='foreground',before_state=3,app_state=4,now_ns=0)
        result=self.bridge.handle(self.call(3,'foreground'))
        self.assertEqual(result['completion']['outcome']['app'],self.app)
        self.assertEqual(len(self.calls),3)
    def test_invalid_identity_or_state_never_releases_pending_effect(self):
        request=self.call(2,'background');typed=Request(**request['request'])
        for path,value in [(('app','pid'),125),(('change','app_state'),4),(('change','before_state'),3),
                           (('change','now_ns'),0),(('change','requested'),'foreground')]:
            bad=copy.deepcopy(self.raw);bad[path[0]][path[1]]=value
            with self.subTest(path=path),self.assertRaises(ValueError):validate_result(bad,typed)
        self.raw['change']['app_state']=4
        with self.assertRaises(ValueError):self.bridge.handle(request)
        cursor=(self.directory/'ui/ui-client.json').read_bytes()
        with self.assertRaises(ValueError):self.bridge.handle({**request,'call':'recover'})
        with self.assertRaises(RuntimeError):self.bridge.handle(self.call(3,'foreground'))
        self.assertEqual((self.directory/'ui/ui-client.json').read_bytes(),cursor)
        self.assertEqual(len(self.calls),2)

if __name__=='__main__':main()
