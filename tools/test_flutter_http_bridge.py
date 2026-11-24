"""The advertised UTF-16 text bound must survive stdin and JSON HTTP encoding."""
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import subprocess
import sys
import threading
import unittest


class FlutterBridgeTextTest(unittest.TestCase):
    def test_full_unicode_text_budget_reaches_the_loopback_endpoint(self):
        observed = []
        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args): pass
            def do_POST(self):
                value = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                observed.append(value)
                body = json.dumps(dict(status='pending', id=value['id'], generation=value['generation'])).encode()
                self.send_response(200)
                self.send_header('Content-Length', str(len(body)))
                self.end_headers()
                self.wfile.write(body)
        with HTTPServer(('127.0.0.1', 0), Handler) as server:
            server.timeout = 5
            thread = threading.Thread(target=server.handle_request)
            thread.start()
            request = dict(generation='a'*48, id='full-text', operation='replace_text',
                           observation='o1', target='s1', start=0, end=0, text='한'*16384)
            call = dict(endpoint=dict(port=server.server_port, token='b'*48), request=request, timeout_ms=4000)
            try:
                reply = subprocess.run([sys.executable, str(Path(__file__).with_name('flutter_http_bridge.py'))],
                    input=json.dumps(call), capture_output=True, text=True, timeout=6)
                self.assertEqual(reply.returncode, 0, reply.stderr)
                self.assertEqual(json.loads(reply.stdout)['http_status'], 200)
            finally:
                thread.join(timeout=6)
            self.assertFalse(thread.is_alive())
            self.assertEqual(observed, [request])


if __name__ == '__main__': unittest.main()
