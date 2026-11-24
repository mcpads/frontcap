#!/usr/bin/env python3
"""iOS session protocol substitute; product acquisition/receipt validation have separate tests."""
import base64,fcntl,hashlib,json,os,sys,time,uuid
from pathlib import Path
import subprocess
root=Path(sys.argv[1])
def save(path,value):
 path.parent.mkdir(parents=True,exist_ok=True)
 temp=path.with_suffix('.tmp');temp.write_text(json.dumps(value));temp.replace(path)
lock=(root/'session.lock').open('r+');fcntl.flock(lock,fcntl.LOCK_EX)
save(root/'worker.json',dict(pid=os.getpid(),started=' '.join(subprocess.check_output(['ps', '-o', 'lstart=', '-p', str(os.getpid())], env=dict(os.environ, TZ='UTC', LC_ALL='C', LANG='C'), text=True).split())))
config=json.loads((root/'launch.json').read_text())['config']
owner=dict(runner=dict(simulator='abcd0000-0000-0000-0000-000000000001',generation='abcd0000-0000-0000-0000-000000000002',pid=41),app=dict(simulator='abcd0000-0000-0000-0000-000000000001',generation='abcd0000-0000-0000-0000-000000000003',pid=42),controller_id=config['controller_id'],source_sha256=config['xctest']['simulator']['source_sha256'])
clock=0;ui=1;now=0;completed=False;deleted=False
system_fixture=root.parent.parent.parent/'system-alerts.json'
system_state=dict(owner=owner,next_clock=1,next_ui=2,invalidated_through_ui=0,bindings={})
for line in sys.stdin:
 message=json.loads(line)
 if message['command']=='start':
  if system_fixture.exists():
   save(root/'target/ui-ready.json',dict(system_alerts=True,port=1234,identity=owner['runner'],descriptor='fixture',process=dict(pid=41,started='fixture')))
  save(root/'target/requests/requests.json',system_state)
  save(root/'ready.json',owner);continue
 if message['command']=='stop':break
 with (root/'calls.jsonl').open('a') as log:log.write(json.dumps(message)+'\n')
 if message['command']=='dispatch_url':
  (root/'dispatched').touch();end=time.monotonic()+10
  while (root.parent/'hold').exists():
   if time.monotonic()>end:raise TimeoutError('test did not release the session')
   time.sleep(.005)
  response=(dict(state='failed',error='injected lost URL acceptance') if (root.parent/'fail').exists()
   else dict(state='accepted',acceptance=dict(request=message['request'],confirmation='unobserved',destination='unobserved')))
  save(root/'commands'/str(message['number'])/'result.json',response);continue
 op=message['operation'];name=op['operation']
 if name in ('observe_system_alerts','tap_system_button'):
  ui+=1
  arguments={} if name=='observe_system_alerts' else {key:op[key] for key in ('reference','observation')}
  request=dict(generation=owner['runner']['generation'],request_id=ui,operation=name,arguments=arguments)
  if name=='observe_system_alerts':
   catalog=json.loads(system_fixture.read_text());capture=str(uuid.uuid4());catalog['observation']=capture
   for index,button in enumerate(button for alert in catalog['alerts'] for button in alert['buttons']):button['reference']=capture+'-'+str(index)
   artifacts=[]
   for file,data in [('screen.png',base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=')),('hierarchy.txt',b'SpringBoard fixture')]:
    relative='observations/'+capture+'/'+file;path=root/'target/ui-client'/relative
    path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(data)
    artifacts.append(dict(path=relative,bytes=len(data),sha256=hashlib.sha256(data).hexdigest()))
   outcome=dict(outcome='system_alerts_observed',catalog=catalog,artifacts=artifacts)
  else:outcome=dict(outcome='system_button_tapped',**arguments)
  completion=dict(channel='ui',receipt=dict(request=request,generation=owner['runner']['generation'],pid=41,last_id=ui,outcome=outcome,receipt_sha256='c'*64))
  system_state['next_ui']=ui+1
  system_state['bindings'][message['id']]=dict(operation=op,request=dict(channel='ui',request=request),progress=dict(state='completed',receipt=completion))
  save(root/'target/requests/requests.json',system_state)
  save(root/'commands'/str(message['number'])/'result.json',dict(state='completed',completion=completion));continue
 if name in ['read_clock','advance_clock']:
  clock+=1;delta=op.get('delta_ns',0);before=now;now+=delta
  request=dict(generation=owner['app']['generation'],request_id=clock,operation='advance' if name=='advance_clock' else 'read',delta_ns=delta)
  receipt=dict(request=request,generation=owner['app']['generation'],pid=42,last_id=clock,status='ok',before_ns=before,now_ns=now,applied=name=='advance_clock',advance_result=0 if name=='advance_clock' else None,drain_result=0 if name=='advance_clock' else None,receipt_sha256='c'*64)
  completion=dict(channel='clock',receipt=receipt)
 elif name=='observe_targets':
  ui+=1
  request=dict(generation=owner['runner']['generation'],request_id=ui,operation=name,arguments={})
  outcome=dict(outcome='observation_unavailable',app=owner['app'],now_ns=now,reason='scroll_in_motion',artifacts=[])
  completion=dict(channel='ui',receipt=dict(request=request,generation=owner['runner']['generation'],pid=41,last_id=ui,outcome=outcome,receipt_sha256='c'*64))
 else:
  raise ValueError("unsupported operation")
 system_state['next_clock']=clock+1;system_state['next_ui']=ui+1
 system_state['bindings'][message['id']]=dict(operation=op,request=dict(channel=completion['channel'],request=request),progress=dict(state='completed',receipt=completion))
 save(root/'target/requests/requests.json',system_state)
 save(root/'commands'/str(message['number'])/'result.json',dict(state='completed',completion=completion))
save(root/'result.json',dict(error=None,retirement=dict(passed=True,errors=[],signal_error=None)))
