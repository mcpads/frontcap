"""Recovery must preserve whether iOS applied a clock change before a drain failure."""
from dataclasses import asdict
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

from native_ios_control import ClockClient, exchange
from native_clock_control import Request, recorded_outcome
from native_ios_control import validate_reply


class IOSControlTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.descriptor = self.root/'endpoint.json'
        self.generation = '33557076-a359-4a02-a088-d946cd77e7c3'
        self.endpoint = dict(status='ready', last_id=0, generation=self.generation, port=12345)
        self.descriptor.write_text(json.dumps(self.endpoint))
        self.calls = []
        self.effects = []
        self.cache = {}
        self.advance_result = 0
        self.drain_result = 0

    def transport(self, endpoint, request):
        self.calls.append(request)
        if request.request_id in self.cache:
            return {**self.cache[request.request_id], 'replayed': True}
        before = sum(self.effects)
        applied = request.operation=='advance' and not self.advance_result
        if applied: self.effects.append(request.delta_ns)
        advance = self.advance_result if request.operation=='advance' else None
        drain = self.drain_result if applied else None
        reply = dict(request=asdict(request), generation=self.generation, last_id=request.request_id,
                     status='failed' if advance or drain else 'ok', applied=applied, before_ns=before,
                     now_ns=sum(self.effects), advance_result=advance, drain_result=drain, replayed=False)
        self.cache[request.request_id] = reply
        return reply

    def client(self, transport=None):
        return ClockClient(self.descriptor, self.root/'client', transport or self.transport)

    def state(self):
        return json.loads((self.root/'client/clock-client.json').read_text())

    def test_lost_reply_recovers_success_busy_and_applied_drain_failure_without_reexecution(self):
        for advance,drain in [(0,0),(60,0),(0,60)]:
            with self.subTest(advance=advance,drain=drain):
                self.advance_result,self.drain_result=advance,drain
                def lose(endpoint, request):
                    self.transport(endpoint,request)
                    raise EOFError('reply lost')
                with self.assertRaises(EOFError): self.client(lose).request('advance',1000000)
                pending=self.state()['pending']
                with self.assertRaisesRegex(RuntimeError,'Pending clock command'): self.client().request('advance',1)
                reply=self.client().resume()
                self.assertTrue(reply['replayed'])
                self.assertEqual(reply['applied'],not advance)
                self.assertEqual(reply['advance_result'],advance)
                self.assertEqual(reply['drain_result'],None if advance else drain)
                self.assertEqual(self.calls[-2],self.calls[-1])
                self.assertEqual(self.state()['next_id'],pending['request_id']+1)
                self.assertIsNone(self.state()['pending'])
        self.assertEqual(self.effects,[1000000,1000000])

    def test_new_generation_keeps_old_request_pending_without_transmission(self):
        self.client().request('read')
        self.endpoint['generation']='00000000-0000-0000-0000-000000000000'
        self.descriptor.write_text(json.dumps(self.endpoint))
        with self.assertRaisesRegex(RuntimeError,'generation changed'): self.client().request('advance',1)
        with self.assertRaisesRegex(RuntimeError,'generation changed'): self.client().resume()
        self.assertEqual(len(self.calls),1)
        self.assertEqual(self.state()['pending']['generation'],self.generation)

    def test_unconfirmed_or_inconsistent_outcomes_stay_pending(self):
        for defect in ('wrong_request','wrong_generation','drain_labeled_unapplied','changed_on_rejection','numeric_boolean'):
            with self.subTest(defect=defect):
                def corrupt(endpoint,request):
                    reply=self.transport(endpoint,request).copy()
                    if defect=='numeric_boolean': reply['applied']=1
                    if defect=='wrong_request': reply['request']={}
                    if defect=='wrong_generation': reply['generation']='other'
                    if defect=='drain_labeled_unapplied': reply.update(applied=False,status='failed',drain_result=60)
                    if defect=='changed_on_rejection': reply.update(applied=False,status='failed',advance_result=60,drain_result=None)
                    return reply
                # The first uncertain command must remain the exact request in all later attempts.
                client=self.client(corrupt)
                with self.assertRaises(RuntimeError):
                    client.resume() if self.state_exists() else client.request('advance',1)
                self.assertEqual(self.state()['pending']['request_id'],1)
        self.assertEqual(self.effects,[1])

    def state_exists(self):
        return (self.root/'client/clock-client.json').exists()

    def test_committed_outcome_is_local_and_rejects_pending_or_inconsistent_state(self):
        self.drain_result=60
        reply=self.client().request('advance',1000000)
        self.descriptor.unlink()
        self.assertEqual(recorded_outcome(self.root/'client',validate_reply),reply)
        result=subprocess.run([sys.executable,str(Path(__file__).with_name('native_ios_control.py')),
            '--directory',str(self.root/'client'),'outcome'],
            env={**os.environ,'PATH':''},capture_output=True,text=True,timeout=5)
        self.assertEqual(result.returncode,1)
        self.assertEqual(json.loads(result.stdout),reply)
        self.assertEqual(result.stderr,'')
        self.assertEqual(len(self.calls),1)
        original=self.state()
        for defect in ('pending','generation','instant'):
            state=json.loads(json.dumps(original))
            if defect=='pending': state['pending']=dict(generation=self.generation,request_id=2,operation='read',delta_ns=0)
            if defect=='generation': state['generation']='00000000-0000-0000-0000-000000000000'
            if defect=='instant': state['last_reply']['now_ns']+=1
            (self.root/'client/clock-client.json').write_text(json.dumps(state))
            with self.assertRaises(RuntimeError): recorded_outcome(self.root/'client',validate_reply)

    def test_receipt_recovery_never_transmits_and_requires_the_exact_terminal_outcome(self):
        def lost(endpoint, request):
            self.transport(endpoint, request)
            raise EOFError('target died')
        with self.assertRaises(EOFError): self.client(lost).request('advance', 1000000)
        pending = self.state()
        reply = self.cache[1]
        for changes in (dict(status='executing', applied=False), dict(request={}),
                        dict(generation='00000000-0000-0000-0000-000000000000'),
                        dict(applied=False), dict(drain_result=None)):
            with self.subTest(changes=changes):
                self.descriptor.write_text(json.dumps({**reply, **changes}))
                with self.assertRaises(RuntimeError): self.client().recover()
                self.assertEqual(self.state(), pending)
        self.descriptor.write_text(json.dumps(reply))
        self.assertEqual(self.client().recover(), reply)
        self.assertIsNone(self.state()['pending'])
        self.assertEqual(self.state()['next_id'], 2)
        with self.assertRaisesRegex(RuntimeError, 'No pending'): self.client().resume()
        self.assertEqual(len(self.calls), 1)
        attempts = [json.loads(line) for line in (self.root/'client/clock-commands.jsonl').read_text().splitlines()]
        self.assertEqual(attempts[-1]['source'], 'receipt')

    def test_descriptor_cli_needs_no_simulator_tool_and_still_fences_generation(self):
        with socket.socket() as server:
            server.bind(('127.0.0.1',0));server.listen(2);server.settimeout(5)
            self.endpoint['port']=server.getsockname()[1]
            self.descriptor.write_text(json.dumps(self.endpoint))
            errors=[]
            def serve():
                try:
                    for _ in range(2):
                        with server.accept()[0] as stream:
                            stream.settimeout(5)
                            def read(n):
                                data=b''
                                while len(data)<n:
                                    chunk=stream.recv(n-len(data))
                                    if not chunk: raise EOFError('request truncated')
                                    data+=chunk
                                return data
                            size,=struct.unpack('!I',read(4))
                            request=Request(**json.loads(read(size)))
                            reply=json.dumps(self.transport(self.endpoint,request)).encode()
                            stream.sendall(struct.pack('!I',len(reply))+reply)
                except BaseException as error: errors.append(error)
            thread=threading.Thread(target=serve,daemon=True);thread.start()
            command=[sys.executable,str(Path(__file__).with_name('native_ios_control.py')),
                     '--descriptor',str(self.descriptor),'--directory',str(self.root/'client')]
            for args in (['read'],['advance','--delta-ns','1']):
                result=subprocess.run(command+args,env={**os.environ,'PATH':''},capture_output=True,text=True,timeout=5)
                self.assertEqual(result.returncode,0,result.stderr)
                self.assertEqual(json.loads(result.stdout)['status'],'ok')
            thread.join(timeout=5)
            self.assertFalse(thread.is_alive());self.assertEqual(errors,[])
            self.endpoint['generation']='00000000-0000-0000-0000-000000000000'
            self.descriptor.write_text(json.dumps(self.endpoint))
            result=subprocess.run(command+['advance','--delta-ns','1'],env={**os.environ,'PATH':''},
                                  capture_output=True,text=True,timeout=5)
            self.assertNotEqual(result.returncode,0)
            self.assertIn('generation changed',result.stderr)
            self.assertEqual(self.state()['pending']['generation'],self.generation)
            self.assertEqual(self.effects,[1]);self.assertEqual(len(self.calls),2)

    def test_transport_deadline_includes_connection_send_and_final_reply(self):
        for delayed in ('connect', 'send', 'body', 'none'):
            with self.subTest(delayed=delayed):
                self.effects.clear(); self.calls.clear(); self.cache.clear()
                self.descriptor.write_text(json.dumps(self.endpoint))
                now = [0.0]; budgets = []; closed = []; sent = []
                owner = self
                class Stream:
                    def __enter__(self): return self
                    def __exit__(self, *args): closed.append(True)
                    def settimeout(self, timeout): budgets.append(timeout)
                    def sendall(self, frame):
                        sent.append(frame)
                        reply = owner.transport(owner.endpoint, Request(**json.loads(frame[4:])))
                        owner.descriptor.write_text(json.dumps(reply))
                        payload = json.dumps(reply).encode()
                        self.parts = [struct.pack('!I', len(payload)), payload]
                        now[0] = max(now[0], 11 if delayed == 'send' else 5)
                    def recv(self, length):
                        part = self.parts.pop(0)
                        now[0] = max(now[0], 6 if self.parts else 11 if delayed == 'body' else 7)
                        return part
                def connect(*args, **kwargs):
                    now[0] = 11 if delayed == 'connect' else 3
                    return Stream()
                client = ClockClient(self.descriptor, self.root/delayed)
                with patch('native_ios_transport.time.monotonic', side_effect=lambda: now[0]), \
                     patch('native_ios_transport.socket.create_connection', side_effect=connect) as connection:
                    if delayed == 'none':
                        self.assertEqual(client.request('advance', 1)['now_ns'], 1)
                    else:
                        with self.assertRaises(TimeoutError): client.request('advance', 1)
                        pending = json.loads(client.path.read_text())['pending']
                        self.assertEqual(pending['request_id'], 1)
                        if delayed == 'connect':
                            with self.assertRaises(RuntimeError): client.recover()
                        else:
                            self.assertEqual(client.recover()['request'], pending)
                    self.assertEqual(connection.call_count, 1)
                self.assertEqual(closed, [True])
                self.assertEqual(len(sent), 0 if delayed == 'connect' else 1)
                self.assertEqual(self.effects, [] if delayed == 'connect' else [1])
                if delayed != 'connect':
                    self.assertTrue(budgets, 'Send must receive the remaining budget')
                    self.assertEqual(budgets[0], 7)
                    self.assertTrue(all(0 < b <= 7 for b in budgets))

    def test_loopback_transport_handles_fragmentation_and_rejects_truncated_reply(self):
        request=Request(self.generation,1,'read')
        reply=self.transport(self.endpoint,request)
        for truncate in (False,True):
            with self.subTest(truncate=truncate), socket.socket() as server:
                server.bind(('127.0.0.1',0));server.listen(1)
                errors=[]
                def serve():
                    try:
                        with server.accept()[0] as stream:
                            def read(n):
                                result=b''
                                while len(result)<n: result+=stream.recv(n-len(result))
                                return result
                            size,=struct.unpack('!I',read(4))
                            self.assertEqual(json.loads(read(size)),asdict(request))
                            payload=json.dumps(reply).encode()
                            frame=struct.pack('!I',len(payload))+payload
                            for chunk in (frame[:2],frame[2:9],frame[9:-1 if truncate else None]): stream.sendall(chunk)
                    except BaseException as error: errors.append(error)
                thread=threading.Thread(target=serve,daemon=True);thread.start()
                endpoint=dict(port=server.getsockname()[1])
                if truncate:
                    with self.assertRaises(EOFError): exchange(endpoint,request)
                else: self.assertEqual(exchange(endpoint,request),reply)
                thread.join(timeout=5)
                self.assertFalse(thread.is_alive())
                self.assertEqual(errors,[])


if __name__=='__main__': unittest.main()
