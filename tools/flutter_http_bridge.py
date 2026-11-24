"""One loopback Flutter exchange; caller owns target, durable intent and real deadline.

No redirects, proxies or automatic retries. A transport failure does not imply non-execution.
"""
import json
import re
import sys
import urllib.error
import urllib.request

MAX_REQUEST_BYTES = 128 * 1024
MAX_RESPONSE_BYTES = 8 * 1024 * 1024


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, msg, headers, newurl):
        return None


def exchange(call):
    if type(call) is not dict or set(call) != {'endpoint', 'request', 'timeout_ms'}:
        raise ValueError('Unexpected bridge fields')
    endpoint = call['endpoint']
    if type(endpoint) is not dict or set(endpoint) != {'port', 'token'}:
        raise ValueError('Unexpected endpoint fields')
    if type(endpoint['port']) is not int or not 0 < endpoint['port'] <= 65535:
        raise ValueError('Invalid loopback port')
    if type(endpoint['token']) is not str or not re.fullmatch('[0-9a-f]{48}', endpoint['token']):
        raise ValueError('Invalid endpoint token')
    timeout = call['timeout_ms']
    if type(timeout) is not int or not 0 < timeout <= 60000:
        raise ValueError('Invalid real request timeout')
    body = json.dumps(call['request'], separators=(',', ':')).encode()
    if len(body) > MAX_REQUEST_BYTES:
        raise ValueError('Flutter request exceeds endpoint limit')
    request = urllib.request.Request('http://127.0.0.1:%d/clock' % endpoint['port'],
        data=body, method='POST', headers={'Authorization': 'Bearer '+endpoint['token'],
                                         'Content-Type': 'application/json'})
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), NoRedirect())
    try:
        response = opener.open(request, timeout=timeout/1000)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        if response.code not in (200, 400, 403, 409):
            raise ValueError('Unexpected Flutter HTTP status %d' % response.code)
        data = response.read(MAX_RESPONSE_BYTES+1)
        if len(data) > MAX_RESPONSE_BYTES:
            raise ValueError('Flutter response exceeds bridge limit')
        return {'http_status': response.code, 'response': json.loads(data)}


if __name__ == '__main__':
    try:
        data = sys.stdin.buffer.read(MAX_REQUEST_BYTES+1025)
        if len(data) > MAX_REQUEST_BYTES+1024:
            raise ValueError('Bridge call exceeds input limit')
        print(json.dumps(exchange(json.loads(data)), separators=(',', ':')))
    except Exception as error:
        print('%s: %s' % (type(error).__name__, error), file=sys.stderr)
        sys.exit(1)
