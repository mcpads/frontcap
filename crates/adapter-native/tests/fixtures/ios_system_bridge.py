# Real durable UIKit bridge with simulated SpringBoard results. No app-clock transport.
import argparse
from dataclasses import asdict
import hashlib
import json
from pathlib import Path

from native_ios_ui_bridge import Bridge

parser = argparse.ArgumentParser()
parser.add_argument('--directory', type=Path)
parser.add_argument('--descriptor', type=Path)
parser.add_argument('--timeout', type=float)
args = parser.parse_args()
root = args.directory.parent
call = json.load(sys.stdin)


def source(endpoint, request, **options):
    with (root/'deliveries').open('a') as log:
        log.write(request.operation+'\n')
    if request.operation == 'observe_system_alerts':
        catalog = json.loads((root/'catalog.json').read_text())
        artifacts = []
        for name in ('screen.png', 'hierarchy.txt'):
            path = 'observations/'+catalog['observation']+'/'+name
            file = root/path
            file.parent.mkdir(parents=True, exist_ok=True)
            raw = name.encode()
            file.write_bytes(raw)
            artifacts.append(dict(path=path, bytes=len(raw), sha256=hashlib.sha256(raw).hexdigest()))
        result = dict(state='system_observed', catalog=catalog, artifacts=artifacts)
    elif request.operation == 'tap_system_button':
        result = dict(input='xctest_system_tap', **request.arguments)
        if (root/'reject').exists():
            result.update(input='not_applied', reason='observation_changed')
    else:
        raise AssertionError('Unexpected transport: '+request.operation)
    reply = dict(request=asdict(request), generation=request.generation, pid=123, port=12346,
                 last_id=request.request_id, status='ok', result=result, replayed=False)
    args.descriptor.write_text(json.dumps(reply))
    return reply


reply = Bridge(args.descriptor, args.directory, source, generic=True).handle(call)
if (root/'lose-reply').exists():
    (root/'lose-reply').unlink()
    sys.exit(3)  # Source result is durable, but the executor received no response.
print(json.dumps(reply))
