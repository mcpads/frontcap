#!/usr/bin/env python3
"""Session substitute for recorder socket/lease/journal tests; never acquires a device."""
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import time

root = Path(sys.argv[1])
def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temp = path.with_suffix('.tmp')
    temp.write_text(json.dumps(value))
    temp.replace(path)

generation = '58e8003c-c32d-49f2-a4d3-628724d76b41'
lock = (root/'session.lock').open('r+')
fcntl.flock(lock, fcntl.LOCK_EX)
save(root/'worker.json', dict(pid=os.getpid(), started=' '.join(subprocess.check_output(['ps', '-o', 'lstart=', '-p', str(os.getpid())], env=dict(os.environ, TZ='UTC', LC_ALL='C', LANG='C'), text=True).split())))
sequence = 0
now = 0
for line in sys.stdin:
    message = json.loads(line)
    if message['command'] == 'start':
        config = json.loads((root/'launch.json').read_text())['config']
        app = config['view_app']
        save(root/'view-ready.json', dict(package=app['package'], activity=app['activity'], serial='emulator-5580', generation=generation, pid=42, source_sha256='a'*64, apk_sha256='b'*64))
        continue
    if message['command'] == 'stop':
        (root/'stop-dispatched').touch()
        end = time.monotonic()+10
        while (root.parent/'stop-hold').exists():
            if time.monotonic() > end: raise TimeoutError('test did not release Stop')
            time.sleep(.01)
        break
    with (root/'calls.jsonl').open('a') as log:
        log.write(json.dumps(message)+'\n')
    (root/'dispatched').touch()
    end = time.monotonic()+10
    while (root.parent/'hold').exists():
        if time.monotonic() > end: raise TimeoutError('test did not release the session')
        time.sleep(.005)
    if message['command'] == 'dispatch_view_url':
        # This fixture only qualifies controller routing/recovery, never native OS delivery.
        if (root.parent/'fail').exists():
            response = dict(state='failed', error='injected lost URL acceptance')
        else:
            response = dict(state='accepted', acceptance=dict(request=message['request'], confirmation='unobserved', destination='unobserved'))
    elif (root.parent/'fail').exists():
        response = dict(state='failed', error='injected lost target receipt')
    else:
        op = message['operation']
        delta = op.get('by_us',0)*1000
        before = now
        now += delta
        sequence += 1
        request = dict(generation=generation, request_id=sequence, operation='advance' if 'by_us' in op else 'read', delta_ns=delta)
        response = dict(state='completed', completion=dict(channel='clock', request=request, generation=generation, pid=42, last_id=sequence, before_ns=before, now_ns=now, applied=True, receipt_sha256='c'*64))
    save(root/'commands'/str(message['number'])/'result.json', response)
save(root/'result.json', dict(error=None, retirement=dict(passed=True, errors=[], signal_error=None)))
