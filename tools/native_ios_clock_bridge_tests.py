import json
from dataclasses import asdict
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from native_ios_clock_bridge import Bridge


class ClockBridgeTests(unittest.TestCase):
    def test_external_reservation_never_transmits_and_only_recovers_exact_receipt(self):
        from native_ios_control import ClockClient
        from native_clock_control import Request
        reserve = {**self.call, 'call':'reserve'}
        expected = dict(reply='reserved',identity=self.identity,request=self.call['request'])
        self.assertEqual(self.bridge.handle(reserve),expected)
        cursor = self.root/'bridge/clock/clock-client.json'
        before = cursor.read_bytes()
        self.assertEqual(self.bridge.handle(reserve),expected)
        self.assertEqual(cursor.read_bytes(),before)
        with self.assertRaises(RuntimeError): self.bridge.handle(self.call)
        with self.assertRaisesRegex(RuntimeError,'External clock delivery'):
            ClockClient(self.descriptor,cursor.parent,self.transport).resume()
        changed = {**reserve,'request':{**self.call['request'],'delta_ns':2000}}
        with self.assertRaises(RuntimeError): self.bridge.handle(changed)
        self.assertEqual(cursor.read_bytes(),before)
        self.assertEqual(self.calls,[])
        # The separately owned gesture publishes the clock outcome, not this bridge.
        request = Request(**self.call['request'])
        self.transport(self.ready,request)
        result = self.bridge.handle({**self.call,'call':'recover'})
        self.assertEqual(result['completion']['status'],'failed')
        self.assertTrue(result['completion']['applied'])
        self.assertEqual(len(self.calls),1)
        state = json.loads(cursor.read_text())
        self.assertEqual(state['next_id'],2)
        self.assertIsNone(state['pending'])
        self.assertNotIn('external',state)
        self.descriptor.unlink()
        self.assertEqual(self.bridge.handle(self.call),result)
        self.assertEqual(len(self.calls),1)

    def test_lost_reservation_reply_remains_fenced_after_reopen(self):
        reserve = {**self.call,'call':'reserve'}
        def lost(directory,phase,request,**options):
            if phase=='after-pending': raise EOFError('reservation reply lost')
        with patch('native_ios_control.checkpoint',side_effect=lost),self.assertRaises(EOFError):
            self.bridge.handle(reserve)
        reopened = Bridge(self.descriptor,self.root/'bridge',transport=self.transport)
        self.assertEqual(reopened.handle(reserve)['reply'],'reserved')
        cursor = self.root/'bridge/clock/clock-client.json';before=cursor.read_bytes()
        for endpoint in (self.ready,{**self.ready,'pid':124}):
            self.descriptor.write_text(json.dumps(endpoint))
            with self.assertRaises(RuntimeError): reopened.handle({**self.call,'call':'recover'})
            self.assertEqual(cursor.read_bytes(),before)
        self.assertEqual(self.calls,[])

    def test_external_reservation_refuses_unowned_target_progress(self):
        self.descriptor.write_text(json.dumps({**self.ready,'last_id':1,'status':'executing'}))
        cursor=self.root/'bridge/clock/clock-client.json';before=cursor.read_bytes()
        with self.assertRaisesRegex(RuntimeError,'cursor differs'):
            self.bridge.handle({**self.call,'call':'reserve'})
        self.assertEqual(cursor.read_bytes(),before)
        self.assertEqual(self.calls,[])

    def test_initialization_respects_existing_client_owner(self):
        import fcntl
        folder = self.root/'competing/clock'; folder.mkdir(parents=True)
        bridge = Bridge(self.descriptor, folder.parent, transport=self.transport)
        with (folder/'clock-client.lock').open('a') as owner:
            fcntl.flock(owner, fcntl.LOCK_EX | fcntl.LOCK_NB)
            with self.assertRaises(BlockingIOError): bridge.handle(self.initialize)
            self.assertFalse((folder/'clock-client.json').exists())
            self.assertFalse((folder.parent/'owner.json').exists())
        self.assertEqual(bridge.handle(self.initialize)['reply'], 'ready')

    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.addCleanup(self.temp.cleanup)
        self.root=Path(self.temp.name);self.descriptor=self.root/'endpoint.json'
        self.identity=dict(simulator='abcd0000-0000-0000-0000-000000000001',
                           generation='abcd0000-0000-0000-0000-000000000002',pid=123)
        self.ready=dict(generation=self.identity['generation'],pid=123,status='ready',last_id=0,port=1)
        self.descriptor.write_text(json.dumps(self.ready));self.calls=[]
        self.bridge=Bridge(self.descriptor,self.root/'bridge',transport=self.transport)
        self.initialize=dict(call='initialize',identity=self.identity,source_sha256='a'*64)
        self.bridge.handle(self.initialize)
        self.call=dict(call='execute',identity=self.identity,request=dict(generation=self.identity['generation'],
                        request_id=1,operation='advance',delta_ns=1000))
        self.lost=False

    def transport(self, endpoint, request, **options):
        self.calls.append(request)
        reply=dict(request=asdict(request),generation=request.generation,pid=123,last_id=request.request_id,
                   status='failed',before_ns=0,now_ns=1000,applied=True,advance_result=0,drain_result=60)
        self.descriptor.write_text(json.dumps(reply))
        if self.lost: raise EOFError('lost reply')
        return reply

    def leave_pending(self):
        def interrupted(directory,phase,request,**options):
            if phase=='after-reply':raise EOFError('controller reply lost')
        with patch('native_ios_control.checkpoint',side_effect=interrupted),self.assertRaises(EOFError):
            self.bridge.handle(self.call)

    def test_recovery_never_dispatches_an_unstarted_request(self):
        with self.assertRaises(RuntimeError): self.bridge.handle({**self.call,'call':'recover'})
        self.assertEqual(len(self.calls),0)
        self.leave_pending()
        reply=self.bridge.handle({**self.call,'call':'recover'})
        self.assertEqual(reply['reply'],'completed');self.assertEqual(len(self.calls),1)

    def test_applied_drain_failure_is_retained_and_repeat_does_not_transmit(self):
        result=self.bridge.handle(self.call)
        self.assertEqual(result['completion']['status'],'failed')
        self.assertTrue(result['completion']['applied'])
        self.descriptor.unlink()
        self.assertEqual(self.bridge.handle(self.call),result)
        self.assertEqual(len(self.calls),1)

    def test_lost_reply_uses_exact_saved_receipt_without_transport(self):
        self.leave_pending()
        self.assertEqual(self.bridge.handle(self.call)['reply'],'completed')
        self.assertEqual(len(self.calls),1)

    def test_missing_receipt_and_replacement_app_keep_original_request_pending(self):
        self.leave_pending()
        cursor=self.root/'bridge/clock/clock-client.json';before=cursor.read_bytes()
        for endpoint in [self.ready,{**self.ready,'pid':124}]:
            self.descriptor.write_text(json.dumps(endpoint))
            with self.assertRaises(RuntimeError): self.bridge.handle(self.call)
            self.assertEqual(cursor.read_bytes(),before)
        self.assertEqual(len(self.calls),1)

    def test_foreign_transport_reply_cannot_commit_cursor_or_publish_receipt(self):
        def foreign(endpoint, request, **options):
            return {**self.transport(endpoint, request, **options), 'pid':124}
        bridge=Bridge(self.descriptor,self.root/'bridge',transport=foreign)
        with self.assertRaisesRegex(RuntimeError, 'generation/PID'):
            bridge.handle(self.call)
        cursor=json.loads((self.root/'bridge/clock/clock-client.json').read_text())
        self.assertEqual(cursor['next_id'],1)
        self.assertEqual(cursor['pending'],self.call['request'])
        self.assertNotIn('last_reply',cursor)
        self.assertFalse((self.root/'bridge/receipts').exists())
        self.assertEqual(len(self.calls),1)

    def test_replacement_between_recovery_reads_leaves_pending_bytes_unchanged(self):
        self.leave_pending()
        cursor=self.root/'bridge/clock/clock-client.json';before=cursor.read_bytes()
        real_read=Path.read_text;reads=[]
        def replaced(path,*args,**kwargs):
            raw=real_read(path,*args,**kwargs)
            if path.resolve()==self.descriptor.resolve():
                reads.append(path)
                if len(reads)==2:return json.dumps({**json.loads(raw),'pid':124})
            return raw
        with patch.object(Path,'read_text',replaced), self.assertRaisesRegex(RuntimeError,'generation/PID'):
            self.bridge.handle({**self.call,'call':'recover'})
        self.assertEqual(len(reads),2)
        self.assertEqual(cursor.read_bytes(),before)
        self.assertEqual(len(self.calls),1)
        self.assertFalse((self.root/'bridge/receipts').exists())

    def test_lost_submission_reply_recovers_within_the_original_call(self):
        self.lost=True
        result=self.bridge.handle(self.call)
        self.assertEqual(result['reply'],'completed')
        self.assertEqual(result['completion']['now_ns'],1000)
        self.assertEqual(result['completion']['status'],'failed')  # Preserve the terminal drain failure.
        self.assertEqual(len(self.calls),1)
        rows=[json.loads(line) for line in (self.root/'bridge/clock/clock-commands.jsonl').read_text().splitlines()]
        self.assertEqual([row['source'] for row in rows],['transport','receipt'])
        self.assertIn('lost reply',rows[0]['error'])
        self.assertNotIn('error',rows[1])

    def test_unconfirmed_or_late_receipt_keeps_original_transport_error_and_pending(self):
        for mode in ('executing','expired'):
            with self.subTest(mode=mode):
                case=ClockBridgeTests();case.setUp()
                try:
                    now=[100.0]
                    def uncertain(endpoint,request,**options):
                        reply=case.transport(endpoint,request,**options)
                        if mode=='executing':case.descriptor.write_text(json.dumps({**reply,'status':'executing'}))
                        else:now[0]=111.0
                        raise EOFError('original wire failure')
                    bridge=Bridge(case.descriptor,case.root/'bridge',transport=uncertain)
                    with patch('native_ios_clock_bridge.time.monotonic',side_effect=lambda:now[0]),self.assertRaisesRegex(EOFError,'^original wire failure$'):
                        bridge.handle(case.call)
                    cursor=json.loads((case.root/'bridge/clock/clock-client.json').read_text())
                    self.assertEqual(cursor['next_id'],1)
                    self.assertEqual(cursor['pending'],case.call['request'])
                    self.assertNotIn('last_reply',cursor)
                    self.assertFalse((case.root/'bridge/receipts').exists())
                    self.assertEqual(len(case.calls),1)
                finally:case.doCleanups()

    def test_cursor_change_before_client_admission_cannot_dispatch_another_request(self):
        from native_ios_control import ClockClient
        original = ClockClient.request
        committed = []
        cursor = self.root/'bridge/clock/clock-client.json'

        def competing(client, *args, **kwargs):
            # A separate controller completes the first request after bridge preflight.
            original(client, 'advance', 1000)
            endpoint = json.loads(self.descriptor.read_text())
            self.descriptor.write_text(json.dumps({**endpoint, 'port': 1}))
            committed.append(cursor.read_bytes())
            return original(client, *args, **kwargs)

        with patch.object(ClockClient, 'request', competing), self.assertRaises(RuntimeError):
            self.bridge.handle(self.call)
        self.assertEqual(len(self.calls), 1, 'Stale bridge request must not dispatch with the next ID')
        self.assertEqual(cursor.read_bytes(), committed[0])
        self.assertFalse((self.root/'bridge/receipts').exists())
        self.assertEqual(self.bridge.handle(self.call)['reply'], 'completed')
        self.assertEqual(len(self.calls), 1)

    def test_damaged_cursor_and_different_source_never_reinitialize(self):
        cursor=self.root/'bridge/clock/clock-client.json';cursor.unlink()
        with self.assertRaises(RuntimeError): self.bridge.handle(self.initialize)
        with self.assertRaises(RuntimeError): self.bridge.handle({**self.initialize,'source_sha256':'b'*64})
        self.assertFalse(cursor.exists());self.assertFalse(self.calls)


if __name__=='__main__': unittest.main()
