#!/usr/bin/env python3
"""Recorder owner-transition fixture; not device or evidence-recovery qualification."""
import fcntl,hashlib,json,os,sys
from pathlib import Path
import subprocess
root=Path(sys.argv[1]);config=json.loads((root/'launch.json').read_text())['config']
def save(path,value):
 path.parent.mkdir(parents=True,exist_ok=True)
 temp=path.with_suffix('.tmp');temp.write_text(json.dumps(value));temp.replace(path)
lock=(root/'session.lock').open('r+');fcntl.flock(lock,fcntl.LOCK_EX)
save(root/'worker.json',dict(pid=os.getpid(),started=' '.join(subprocess.check_output(['ps', '-o', 'lstart=', '-p', str(os.getpid())], env=dict(os.environ, TZ='UTC', LC_ALL='C', LANG='C'), text=True).split())))
digest=hashlib.sha256(json.dumps([config['app']['library']['sha256'],None,config['app']['mute_audio']],separators=(',',':')).encode()).hexdigest()
owner=dict(simulator='abcd0000-0000-0000-0000-000000000001',process=dict(pid=42,started='original'),attachment_sha256=digest)
for line in sys.stdin:
 message=json.loads(line)
 if message['command']=='start':save(root/'host-ready.json',dict(owner=owner,contacts=None,pages=False));continue
 if message['command']=='stop':break
 with (root/'calls.jsonl').open('a') as out:out.write(json.dumps(message)+'\n')
 op=message['operation'];assert op['operation']=='host';op=op['request']
 if op['operation']=='restart':
  assert op['previous']==owner
  previous=owner;owner=dict(owner,process=dict(pid=43,started='replacement'))
  completion=dict(kind='restarted',previous=previous,ready=dict(owner=owner,contacts=None,pages=False))
 elif op['operation']=='observe':
  completion=dict(kind='observed',capture=dict(owner=owner,path=str(root/'unused.png'),bytes=8,sha256='b'*64,input=None))
 else:raise AssertionError(op)
 save(root/'commands'/str(message['number'])/'result.json',dict(state='completed',completion=dict(kind='host',completion=completion)))
save(root/'result.json',dict(error=None,retirement=dict(passed=True,errors=[],signal_error=None)))
