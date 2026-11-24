"""Loopback-only frame fixture server; embed in an owned native runner and call shutdown."""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path


def create_server(port, output, *, fixture=None, timeline=None, redirect=None, accept_reports=True):
    assets = Path(__file__).parent
    output = Path(output)
    routes = {'/': Path(fixture) if fixture else assets / 'frame-fixture.html',
              '/frame-gate.js': assets / 'frame-gate.js'}
    if timeline:
        routes['/timeline.js'] = Path(timeline)
    for path in routes.values():
        path.resolve(strict=True)

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            if redirect and self.path == '/':
                self.send_response(302)
                self.send_header('Location', redirect)
                self.send_header('Content-Length', '0')
                self.end_headers()
                return
            name = routes.get(self.path)
            if name is None:
                self.send_error(404)
                return
            body = name.read_bytes()
            self.send_response(200)
            self.send_header('Content-Type', 'text/html' if name.suffix == '.html' else 'text/javascript')
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Cache-Control', 'no-store')
            self.send_header('Content-Security-Policy', "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; object-src 'none'")
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            if not accept_reports:
                self.send_error(405)
                return
            if self.path != '/report':
                self.send_error(404)
                return
            try:
                size = int(self.headers.get('Content-Length', '0'))
                if not 0 < size <= 65536:
                    raise ValueError('Invalid report size')
                value = json.loads(self.rfile.read(size))
                if not isinstance(value, dict) or not isinstance(value.get('passed'), bool):
                    raise ValueError('Invalid report')
            except (ValueError, UnicodeDecodeError):
                self.send_error(400)
                return
            try:
                with output.open('x') as stream:
                    stream.write(json.dumps(value, indent=2)+'\n')
            except FileExistsError:
                self.send_error(409)
                return
            self.send_response(204)
            self.end_headers()

        def log_message(self, *args):
            pass

    return ThreadingHTTPServer(('127.0.0.1', port), Handler)
