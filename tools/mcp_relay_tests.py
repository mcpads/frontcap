"""Relay access evidence must include discovery and requests rejected before dispatch."""
import importlib.util
import contextlib
import hashlib
import io
import json
from pathlib import Path
import socket
import tempfile
import threading
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('mcp_relay', Path(__file__).with_name('mcp-relay.py'))
relay = importlib.util.module_from_spec(spec)
spec.loader.exec_module(relay)


class AccessEvidenceTests(unittest.TestCase):
    def test_discovery_call_and_rejection_are_retained_without_extra_dispatch(self):
        discovery = {'instructions': 'Use documented operations.', 'tools': [{'name': 'native'}]}
        calls = []

        def request(method, params):
            calls.append((method, params))
            return {'result': {'isError': False, 'structuredContent': {'request_id': 'original'}}}

        with tempfile.TemporaryDirectory() as directory:
            log = Path(directory) / 'access.jsonl'
            replies = []
            errors = []
            for raw in ['{"discover":true}\n', '{"tool":"native","arguments":{"id":"original"}}\n', 'not-json\n', '[]\n']:
                server, client = socket.socketpair()
                def serve():
                    try:
                        relay.handle(server, request, discovery, log, threading.Lock())
                    except Exception as error:
                        errors.append(error)
                worker = threading.Thread(target=serve, daemon=True)
                worker.start()
                with client, client.makefile('rw') as stream:
                    client.settimeout(3)
                    stream.write(raw)
                    stream.flush()
                    replies.append(json.loads(stream.readline()))
                worker.join(3)
                self.assertFalse(worker.is_alive())
            self.assertEqual(errors, [])
            records = [json.loads(line) for line in log.read_text().splitlines()]
            self.assertEqual([r['answer'] for r in records], replies)
            self.assertEqual(records[0]['event'], 'discovery')
            self.assertEqual(records[0]['answer'], discovery)
            self.assertEqual(records[1]['tool'], 'native')
            self.assertEqual(records[1]['arguments'], {'id': 'original'})
            self.assertEqual([r['event'] for r in records[2:]], ['rejected', 'rejected'])
            self.assertEqual([r['request'] for r in records[2:]], ['not-json\n', '[]\n'])
            self.assertEqual(calls, [('tools/call', {'name': 'native', 'arguments': {'id': 'original'}})])


class ClientOutputTests(unittest.TestCase):
    def test_large_error_and_discovery_are_lossless_files_without_redispatch(self):
        for message, answer in [
            ({'tool': 'native'}, {'is_error': True, 'result': {'request_id': 'original', 'detail': '한글' * 12000}}),
            ({'discover': True}, {'instructions': 'Read tools', 'tools': [{'description': 'x' * 20000}]}),
        ]:
            with self.subTest(message=message), tempfile.TemporaryDirectory() as directory:
                raw = json.dumps(answer, ensure_ascii=False) + '\n'
                stdout = io.StringIO()
                with patch.object(relay.Path, 'cwd', return_value=Path(directory)), patch.object(relay, 'send', return_value=raw) as send, contextlib.redirect_stdout(stdout):
                    relay.client_response('socket', message)
                receipt = json.loads(stdout.getvalue())
                self.assertLess(len(stdout.getvalue()), 1024)
                saved = Path(receipt['response_file']).read_bytes()
                self.assertEqual(saved, raw.encode())
                self.assertEqual(receipt['bytes'], len(saved))
                self.assertEqual(receipt['sha256'], hashlib.sha256(saved).hexdigest())
                self.assertEqual(receipt['is_error'], answer.get('is_error'))
                send.assert_called_once_with('socket', message)

    def test_output_collision_prevents_dispatch_and_small_response_can_be_saved(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / 'response.json'
            raw = '{"is_error":false,"result":{"request_id":"original"}}\n'
            with patch.object(relay, 'send', return_value=raw) as send, contextlib.redirect_stdout(io.StringIO()):
                relay.client_response('socket', {'tool': 'native'}, output=target)
                with self.assertRaises(FileExistsError):
                    relay.client_response('socket', {'tool': 'native'}, output=target)
            self.assertEqual(target.read_text(), raw)
            send.assert_called_once()

    def test_small_and_explicit_inline_keep_legacy_output_without_retained_files(self):
        with tempfile.TemporaryDirectory() as directory:
            for inline, raw in [(False, '{"is_error":false}\n'), (True, json.dumps({'result': 'x' * 20000}) + '\n')]:
                stdout = io.StringIO()
                with patch.object(relay.Path, 'cwd', return_value=Path(directory)), patch.object(relay, 'send', return_value=raw), contextlib.redirect_stdout(stdout):
                    relay.client_response('socket', {'tool': 'native'}, inline=inline)
                self.assertEqual(stdout.getvalue(), raw)
            self.assertEqual(list(Path(directory).rglob('*.json')), [])


if __name__ == '__main__':
    unittest.main()
