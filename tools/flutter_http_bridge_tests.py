"""HTTP boundaries that must not turn one dispatched intent into another request."""
import http.server
import json
import threading
import unittest
from unittest.mock import patch

import flutter_http_bridge as bridge


class HttpBoundaryTests(unittest.TestCase):
    def serve(self, status, body, location=None):
        requests = []

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                requests.append((self.path, self.rfile.read(int(self.headers['Content-Length']))))
                self.send_response(status)
                if location:
                    self.send_header('Location', location)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *args):
                pass

        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        worker = threading.Thread(target=lambda: server.serve_forever(poll_interval=.01))
        worker.start()

        def retire():
            server.shutdown()
            server.server_close()
            worker.join(timeout=2)
            self.assertFalse(worker.is_alive())
        self.addCleanup(retire)
        call = dict(endpoint=dict(port=server.server_port, token='a'*48),
                    request=dict(generation='b'*48, id='read', operation='read'), timeout_ms=1000)
        return call, requests

    def test_rejection_remains_a_rejection_with_one_send(self):
        call, requests = self.serve(409, b'{"status":"rejected","error":"stale"}')
        result = bridge.exchange(call)
        self.assertEqual(result, dict(http_status=409, response=dict(status='rejected', error='stale')))
        self.assertEqual(len(requests), 1)
        self.assertEqual(json.loads(requests[0][1]), call['request'])

    def test_redirect_cannot_repeat_request_or_forward_token(self):
        call, requests = self.serve(307, b'', '/elsewhere')
        with self.assertRaisesRegex(ValueError, 'Unexpected Flutter HTTP status 307'):
            bridge.exchange(call)
        self.assertEqual(len(requests), 1)

    def test_oversized_response_does_not_become_completion(self):
        call, requests = self.serve(200, b'0123456789')
        with patch.object(bridge, 'MAX_RESPONSE_BYTES', 8):
            with self.assertRaisesRegex(ValueError, 'response exceeds'):
                bridge.exchange(call)
        self.assertEqual(len(requests), 1)
