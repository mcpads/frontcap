"""Transport diagnostics distinguish missing frames without resolving uncertain requests."""
import json
from pathlib import Path
import socket
import struct
import tempfile
import threading
import unittest
from unittest.mock import MagicMock, patch

from native_clock_control import Request
from native_ios_control import ClockClient
from native_ios_transport import exchange
from native_ios_ui import UIClient


class TransportObservationTests(unittest.TestCase):
    def test_real_socket_failures_retain_stage_bytes_and_pending_identity(self):
        generation = '33557076-a359-4a02-a088-d946cd77e7c3'
        for client_type, prefix in ((ClockClient, 'clock'), (UIClient, 'ui')):
            for stage in ('connect', 'header', 'body', 'decode'):
                with self.subTest(client=prefix, stage=stage), tempfile.TemporaryDirectory() as temporary, socket.socket() as server:
                    root = Path(temporary)
                    server.bind(('127.0.0.1', 0)); server.settimeout(3)
                    endpoint = dict(generation=generation, status='ready', last_id=0, port=server.getsockname()[1])
                    descriptor = root/'endpoint.json'; descriptor.write_text(json.dumps(endpoint))
                    received, errors = [], []
                    def serve():
                        try:
                            with server.accept()[0] as stream:
                                stream.settimeout(3)
                                def read(length):
                                    data = b''
                                    while len(data) < length:
                                        chunk = stream.recv(length-len(data))
                                        if not chunk: raise EOFError('Request truncated')
                                        data += chunk
                                    return data
                                size, = struct.unpack('!I', read(4))
                                received.append(json.loads(read(size)))
                                if stage == 'header': stream.sendall(b'\x00\x00')
                                elif stage == 'body': stream.sendall(struct.pack('!I', 10)+b'{"x')
                                else: stream.sendall(struct.pack('!I', 1)+b'{')
                        except BaseException as error:
                            errors.append(error)
                    thread = None
                    if stage != 'connect':
                        server.listen(1)
                        thread = threading.Thread(target=serve, daemon=True); thread.start()
                    else:
                        # A bound, non-listening Darwin socket can time out instead of refusing.
                        server.close()
                    try:
                        client = client_type(descriptor, root/'client')
                        expected = ConnectionRefusedError if stage == 'connect' else json.JSONDecodeError if stage == 'decode' else EOFError
                        with self.assertRaises(expected):
                            if prefix == 'clock': client.request('advance', 1)
                            else: client.request('tap', target='native-increment')
                        saved = client.path.read_bytes()
                        pending = json.loads(saved)['pending']
                        row = json.loads((client.directory/(prefix+'-commands.jsonl')).read_text())
                        self.assertEqual(row['request'], pending)
                        detail = row['transport_failure']
                        self.assertEqual(detail['phase'], stage)
                        self.assertEqual(detail['port'], endpoint['port'])
                        self.assertEqual(detail['timeout_s'], 10 if prefix == 'clock' else 20)
                        self.assertEqual(detail['send_completed'], stage != 'connect')
                        self.assertGreaterEqual(detail['failed_monotonic_s'], detail['phase_started_monotonic_s'])
                        self.assertGreaterEqual(detail['phase_started_monotonic_s'], detail['started_monotonic_s'])
                        self.assertEqual(detail['response_header_bytes'], 0 if stage == 'connect' else 2 if stage == 'header' else 4)
                        self.assertEqual(detail['response_body_bytes'], 3 if stage == 'body' else 1 if stage == 'decode' else 0)
                        self.assertEqual(detail['expected_response_bytes'], 10 if stage == 'body' else 1 if stage == 'decode' else None)
                        with patch('native_ios_transport.socket.create_connection', side_effect=AssertionError('Recovery sent a request')):
                            with self.assertRaises(RuntimeError): client.recover()
                        self.assertEqual(client.path.read_bytes(), saved)
                    finally:
                        if thread is not None:
                            thread.join(timeout=4)
                            self.assertFalse(thread.is_alive())
                    self.assertEqual(errors, [])
                    self.assertEqual(received, [] if stage == 'connect' else [pending])

    def test_send_failure_and_cancellation_keep_original_exception_and_unknown_delivery(self):
        request = Request('33557076-a359-4a02-a088-d946cd77e7c3', 1, 'advance', 1)
        for error in (TimeoutError('send stalled'), InterruptedError('cancelled')):
            with self.subTest(error=type(error).__name__):
                stream = MagicMock(); stream.__enter__.return_value = stream
                stream.sendall.side_effect = error
                with patch('native_ios_transport.socket.create_connection', return_value=stream):
                    with self.assertRaises(type(error)) as caught: exchange(dict(port=12345), request)
                self.assertIs(caught.exception, error)
                detail = error.native_transport_failure
                self.assertEqual(detail['phase'], 'send')
                self.assertTrue(detail['connected'] and detail['send_started'])
                self.assertFalse(detail['send_completed'])
                self.assertNotIn('request_bytes_sent', detail)
                stream.sendall.assert_called_once(); stream.recv.assert_not_called()
                stream.__exit__.assert_called_once()


if __name__ == '__main__': unittest.main()

class ResponseBudgetTests(unittest.TestCase):
    def test_large_catalog_reply_requires_explicit_budget_and_rejects_oversize_header(self):
        request = Request('33557076-a359-4a02-a088-d946cd77e7c3', 1, 'read')
        payload = json.dumps({'label': '부산' * 5000}).encode()
        for budget, declared, body, expected in [(1024*1024, len(payload), payload, None),
                (8192, len(payload), b'', ValueError), (1024*1024, 1024*1024+1, b'', ValueError)]:
            with self.subTest(budget=budget, declared=declared), socket.socket() as listener:
                listener.bind(('127.0.0.1', 0));listener.listen(1);listener.settimeout(3)
                errors=[]
                def serve():
                    try:
                        with listener.accept()[0] as stream:
                            stream.settimeout(3)
                            header=stream.recv(4, socket.MSG_WAITALL)
                            size,=struct.unpack('!I',header)
                            stream.recv(size, socket.MSG_WAITALL)
                            stream.sendall(struct.pack('!I',declared))
                            for offset in range(0,len(body),1024):stream.sendall(body[offset:offset+1024])
                    except BaseException as error:errors.append(error)
                thread=threading.Thread(target=serve);thread.start()
                endpoint={'port':listener.getsockname()[1]}
                try:
                    if expected:
                        with self.assertRaises(expected):exchange(endpoint,request,max_response_bytes=budget)
                    else:self.assertEqual(exchange(endpoint,request,max_response_bytes=budget),json.loads(payload))
                finally:thread.join(5)
                self.assertFalse(thread.is_alive());self.assertEqual(errors,[])
