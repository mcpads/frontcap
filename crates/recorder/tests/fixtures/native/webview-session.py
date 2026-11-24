#!/usr/bin/env python3
"""Recorder boundary fixture, not native device or clock qualification."""
import base64,fcntl,hashlib,json,os,sys
from pathlib import Path
import subprocess
root=Path(sys.argv[1])
capture_root=root/'target/webview'
def save(path,value):
 path.parent.mkdir(parents=True,exist_ok=True)
 temp=path.with_suffix('.tmp');temp.write_text(json.dumps(value));temp.replace(path)
lock=(root/'session.lock').open('r+');fcntl.flock(lock,fcntl.LOCK_EX)
save(root/'worker.json',dict(pid=os.getpid(),started=' '.join(subprocess.check_output(['ps', '-o', 'lstart=', '-p', str(os.getpid())], env=dict(os.environ, TZ='UTC', LC_ALL='C', LANG='C'), text=True).split())))
endpoint=dict(generation='abcd0000-0000-0000-0000-000000000002',pid=42,port=12345)
owner=dict(simulator='abcd0000-0000-0000-0000-000000000001',process=dict(pid=42,started='fixture'),endpoint=endpoint,attachment_sha256='a'*64)
now=0;cursor=0;saved={};contacts_saved={};lifecycle_saved={};lifecycle_cursor=1
def state(value):return dict(elapsedMs=value/1e6,performanceMs=value/1e6,dateMs=1700000000000+value/1e6,advancing=False,failure=None,pendingTimers=0,cancelledOnFailure=0,timerQueue=dict(timers=[],omitted=0,failure=None))
for line in sys.stdin:
 message=json.loads(line)
 if message['command']=='start':save(root/'ready.json',owner);continue
 if message['command']=='stop':break
 with (root/'calls.jsonl').open('a') as log:log.write(json.dumps(message)+'\n')
 op=message['operation'];name=op['operation'];identity=message['id']
 if name=='restart':
  capture_root=root/'epochs'/identity
  previous=json.loads(json.dumps(owner));endpoint=dict(endpoint,generation='abcd0000-0000-0000-0000-000000000009',pid=44)
  owner=dict(owner,endpoint=endpoint,process=dict(pid=44,started='replacement'))
  now=0;cursor=0;saved={};contacts_saved={};lifecycle_saved={}
  save(root/'ready.json',owner)
  completion=dict(kind='restarted',previous=previous,owner=owner,page_ready=False)
 elif name=='clock':
  cursor+=1;before=now;now+=op['delta_ns'];advancing=op['kind']=='advance'
  receipt=dict(**endpoint,request=dict(generation=endpoint['generation'],request_id=cursor,operation=op['kind'],delta_ns=op['delta_ns']),last_id=cursor,now_ns=now,replayed=False,status='ok',applied=advancing,document='abcd0000-0000-0000-0000-000000000003',state=state(now),progress=dict(before=state(before),after=state(now),requestedMs=now/1e6) if advancing else None,error=None)
  receipt['canvas_kit']=dict(document=receipt['document'],published=True)
  receipt['navigation']=dict(document=receipt['document'],current=dict(href='http://localhost/game',hrefTruncated=False,focused=True),historyObserved=True,failure=None,received=1,dropped=0,events=[dict(sequence=1,elapsedMs=0,href='http://localhost/game',hrefTruncated=False,focused=True,kind='pageshow',trusted=True,persisted=False)])
  saved[identity]=receipt;completion=dict(kind='clock',reply=receipt)
 elif name=='inspect_progress':
  completion=dict(kind='progress',observation=dict(owner=owner,retired=False,basis='retained_receipts',service=dict(state='not_inspected'),clock=dict(storage_uncertain=False,last_completed=None,pending=None),contact_endpoint=None,contacts=None,requires_cancel=None,lifecycle=None,unobserved=['service_delivery','media','speech','native_presentation','other_app_work']))
 elif name=='reconcile':completion=dict(kind='reconciled',reply=saved[op['request_id']])
 elif name=='contacts':
  contact=op['input'];receipt=dict(outcome='enqueued',revision=contact['expected_revision']+1,contacts=contact.get('contacts',[]),changed=True,mach_timestamp=1)
  contacts_saved[identity]=receipt;completion=dict(kind='contacts',receipt=receipt)
 elif name=='reconcile_contacts':completion=dict(kind='contacts_reconciled',receipt=contacts_saved[op['request_id']])
 elif name in ('background','foreground'):
  lifecycle_cursor+=1
  receipt=dict(generation=endpoint['generation'],pid=43,port=12346,request=dict(generation=endpoint['generation'],request_id=lifecycle_cursor,operation=name,arguments={}),last_id=lifecycle_cursor,replayed=False,status='ok',result=dict(operation=name,app_state=3 if name=='background' else 4,frame_points=[0,0,0,0] if name=='background' else [0,0,852,393],input='none',touches=None),error=None)
  lifecycle_saved[identity]=receipt;completion=dict(kind='lifecycle',reply=receipt)
 elif name=='reconcile_lifecycle':completion=dict(kind='lifecycle_reconciled',reply=lifecycle_saved[op['request_id']])
 elif name=='capture':
  data=base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=')
  path=capture_root/('capture-'+identity)/'screen.png';path.parent.mkdir(parents=True);path.write_bytes(data)
  completion=dict(kind='captured',path=str(path),bytes=len(data),sha256=hashlib.sha256(data).hexdigest())
 else:raise AssertionError(op)
 result=dict(state='failed',error='simulated response loss') if identity in ('uncertain','home-uncertain') else dict(state='completed',completion=completion)
 save(root/'commands'/str(message['number'])/'result.json',result)
save(root/'result.json',dict(error=None,retirement=dict(passed=True,errors=[],signal_error=None)))
