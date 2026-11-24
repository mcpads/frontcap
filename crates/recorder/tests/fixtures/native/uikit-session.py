#!/usr/bin/env python3
"""UIKit session substitute for public epoch/recovery checks; no Simulator behavior claim."""
import base64,copy,fcntl,hashlib,json,os,sys,time,uuid
from pathlib import Path
import subprocess
root=Path(sys.argv[1]);target=root/'target';workspace=root.parent.parent.parent
save=lambda p,v:write(p,json.dumps(v).encode())
def write(p,raw):
 p.parent.mkdir(parents=True,exist_ok=True);temporary=p.with_suffix('.tmp');temporary.write_bytes(raw);temporary.replace(p)
lock=(root/'session.lock').open('r+');fcntl.flock(lock,fcntl.LOCK_EX)
config=json.loads((root/'launch.json').read_text())['config']
owner=dict(runner=dict(simulator='abcd0000-0000-0000-0000-000000000001',generation='abcd0000-0000-0000-0000-000000000002',pid=41),app=dict(simulator='abcd0000-0000-0000-0000-000000000001',generation='abcd0000-0000-0000-0000-000000000003',pid=42),controller_id=config['controller_id'],source_sha256=config['xctest']['simulator']['source_sha256'])
installation=dict(bundle=config['xctest']['app']['bundle'],app=str(workspace/'Client.app'),data=str(workspace/'data'),data_device=1,data_inode=2,app_sha256='a'*64)
history=dict(initial=copy.deepcopy(owner),installation=installation,entries=[])
epoch=target;next_ui=2;now=0
state=dict(owner=copy.deepcopy(owner),next_ui=2,first_ui=2,next_clock=1,invalidated_through_ui=0,bindings={})
endpoint=lambda app:dict(generation=app['generation'],pid=app['pid'],port=1235)
def receipt(client,request,result,outcome):
 raw=dict(request=request,generation=owner['runner']['generation'],pid=41,last_id=request['request_id'],status='ok',port=1234,result=result)
 data=json.dumps(raw).encode();digest=hashlib.sha256(data).hexdigest();write(client/'receipts'/(digest+'.json'),data)
 return dict(request=request,generation=owner['runner']['generation'],pid=41,last_id=request['request_id'],outcome=outcome,receipt_sha256=digest)
def artifacts(client,observation):
 result=[]
 for name,data in [('screen.png',base64.b64decode('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=')),('hierarchy.txt',b'UIKit fixture')]:
  path='observations/'+observation+'/'+name;write(client/path,data);result.append(dict(path=path,bytes=len(data),sha256=hashlib.sha256(data).hexdigest()))
 return result
save(root/'worker.json',dict(pid=os.getpid(),started=' '.join(subprocess.check_output(['ps', '-o', 'lstart=', '-p', str(os.getpid())], env=dict(os.environ, TZ='UTC', LC_ALL='C', LANG='C'), text=True).split())))
try:
 for line in sys.stdin:
  m=json.loads(line)
  if m['command']=='stop':break
  if m['command']=='start':
   save(target/'launch.json',config['xctest']);save(target/'request-owner.json',owner)
   save(target/'ui-ready.json',dict(system_alerts=True,app_lifecycle=True,app_restart=True,app_entry=True,port=1234,identity=owner['runner'],descriptor='fixture',process=dict(pid=41,started='fixture')))
   save(target/'requests/requests.json',state);save(target/'restarts/history.json',history);save(root/'ready.json',owner);continue
  if m['command']=='restart':
   previous=copy.deepcopy(owner);request=dict(generation=owner['runner']['generation'],request_id=next_ui,operation='restart',arguments=dict(app=previous['app']))
   entry=dict(id=m['id'],request=request,completion=None)
   if 'url' in m:
    request['arguments']['url']=m['url']
    admission=target/'restarts'/m['id']/'admission.json';save(admission,dict(bundle=installation['bundle'],scheme=m['url'].split(':')[0],fixture=True))
    entry['admission_sha256']=hashlib.sha256(admission.read_bytes()).hexdigest()
   history['entries'].append(entry);save(target/'restarts/history.json',history)
   state['invalidated_through_ui']=next_ui-1;save(epoch/'requests/requests.json',state)
   owner=dict(owner,app=dict(owner['app'],generation=str(uuid.uuid4()),pid=owner['app']['pid']+1))
   raw=dict(state='restarted',retirement=dict(endpoint=endpoint(previous['app']),process_absent=True,port_closed=True,process_errno=3,port_errno=61),reads=dict(endpoint=endpoint(owner['app']),now_ns=0))
   if 'url' in m:raw['url']=m['url']
   outcome=dict(outcome='restarted',previous=previous['app'],app=owner['app'],process_absent=True,port_closed=True,now_ns=0)
   completion=receipt(epoch/'ui-client',request,raw,outcome);save(epoch/'ui-client/restarts'/f'{next_ui}.json',completion)
   save(target/'restarts'/m['id']/'installation-after.json',installation)
   replacement=dict(previous=previous,owner=owner,installation=installation,completion=completion)
   if 'url' in m:replacement['entry']=dict(request=dict(target=dict(platform='ios',simulator=owner['app']['simulator'],bundle=installation['bundle']),generation=previous['app']['generation'],url=m['url']),confirmation='unobserved',destination='unobserved')
   with (target/'restart-effects.jsonl').open('a') as stream:stream.write(json.dumps(replacement)+'\n')
   (workspace/'restart-published').touch();end=time.monotonic()+10
   while (workspace/'hold-restart').exists():
    if time.monotonic()>end:raise TimeoutError('test did not release restart')
    time.sleep(.005)
   if (workspace/'leave-history-pending').exists():break
   next_ui+=1;now=0;epoch=target/'epochs'/owner['app']['generation']
   state=dict(owner=copy.deepcopy(owner),next_ui=next_ui,first_ui=next_ui,next_clock=1,invalidated_through_ui=0,bindings={})
   save(epoch/'request-owner.json',owner);save(epoch/'requests/requests.json',state)
   entry['completion']=replacement;save(target/'restarts/history.json',history)
   save(root/'commands'/str(m['number'])/'result.json',dict(state='completed',replacement=replacement));continue
  if m['command']=='set_app_state':
   visibility=m['state'];op=dict(operation='set_app_state',generation=m['generation'],state=visibility)
   request=dict(generation=owner['runner']['generation'],request_id=next_ui,operation='set_app_state',arguments=dict(app=owner['app'],state=visibility))
   change=dict(requested=visibility,before_state=4 if visibility=='background' else 3,app_state=2 if visibility=='background' else 4,now_ns=None if visibility=='background' else now)
   raw=dict(state='app_state_changed',app=endpoint(owner['app']),change=change)
   ui=receipt(epoch/'ui-client',request,raw,dict(outcome='app_state_changed',app=owner['app'],change=change))
   process=dict(pid=owner['app']['pid'],started='original UIKit fixture process')
   transition=dict(owner=copy.deepcopy(owner),process_before=process,process_after=process,completion=ui)
   attempt=epoch/'app-state'/m['id'];save(attempt/'before.json',dict(owner=owner,operation=op,process=process));save(attempt/'after.json',process)
   state['bindings'][m['id']]=dict(operation=op,request=dict(channel='ui',request=request),progress=dict(state='completed',receipt=dict(channel='ui',receipt=ui)))
   next_ui+=1;state['next_ui']=next_ui;save(epoch/'requests/requests.json',state)
   with (target/'state-effects.jsonl').open('a') as stream:stream.write(json.dumps(transition)+'\n')
   (workspace/'state-published').touch();end=time.monotonic()+10
   while (workspace/'hold-state').exists():
    if time.monotonic()>end:raise TimeoutError('test did not release state transition')
    time.sleep(.005)
   if (workspace/'leave-state-pending').exists():break
   save(attempt/'receipt.json',transition)
   save(root/'commands'/str(m['number'])/'result.json',dict(state='completed',transition=transition));continue
  assert m['command']=='execute';op=m['operation'];name=op['operation'];client=epoch/'ui-client'
  if name in ['read_clock','advance_clock']:
   request=dict(generation=owner['app']['generation'],request_id=state['next_clock'],operation='read' if name=='read_clock' else 'advance',delta_ns=op.get('delta_ns',0))
   before=now;now+=request['delta_ns'];raw=dict(request=request,generation=owner['app']['generation'],pid=owner['app']['pid'],last_id=request['request_id'],port=1235,status='ok',before_ns=before,now_ns=now,applied=name=='advance_clock',advance_result=0 if name=='advance_clock' else None,drain_result=0 if name=='advance_clock' else None)
   data=json.dumps(raw).encode();digest=hashlib.sha256(data).hexdigest();write(epoch/'clock-client/receipts'/(digest+'.json'),data)
   raw.pop('port');raw['receipt_sha256']=digest;completion=dict(channel='clock',receipt=raw);bound=dict(channel='clock',request=request);state['next_clock']+=1
  else:
   arguments={k:v for k,v in op.items() if k!='operation'};request=dict(generation=owner['runner']['generation'],request_id=next_ui,operation=name,arguments=arguments)
   if name in ['observe_targets','observe_system_alerts']:
    observation=str(uuid.uuid4());images=artifacts(client,observation)
    if name=='observe_targets':
     catalog=dict(observation=observation,keyboard=False,transitioning=False,scrolls=[],text_input=dict(state='unfocused'),targets=[dict(role='button',type=9,identifier='',label='Save',value=None,selector_key='label',selector_value='Save',unique=True,enabled=True,selected=False,hittable=True,intersects_screen=True,frame_mpt=[0,0,1000,1000],reference=observation+'-0')])
     raw=dict(state='observed',app=endpoint(owner['app']),now_ns=now,catalog=catalog,artifacts=images);outcome=dict(outcome='observed_targets',app=owner['app'],now_ns=now,catalog=catalog,artifacts=images)
    else:
     catalog=json.loads((workspace/'system-alerts.json').read_text());catalog['observation']=observation
     for index,button in enumerate(b for a in catalog['alerts'] for b in a['buttons']):button['reference']=observation+'-'+str(index)
     raw=dict(state='system_observed',catalog=catalog,artifacts=images);outcome=dict(outcome='system_alerts_observed',catalog=catalog,artifacts=images)
   elif name=='tap_target':raw=dict(input='xctest_tap',app=endpoint(owner['app']),**arguments);outcome=dict(outcome='target_tapped',app=owner['app'],**arguments)
   elif name=='tap_system_button':raw=dict(input='xctest_system_tap',**arguments);outcome=dict(outcome='system_button_tapped',**arguments)
   else:raise AssertionError(name)
   completion=dict(channel='ui',receipt=receipt(client,request,raw,outcome));bound=dict(channel='ui',request=request);next_ui+=1;state['next_ui']=next_ui
  state['bindings'][m['id']]=dict(operation=op,request=bound,progress=dict(state='completed',receipt=completion));save(epoch/'requests/requests.json',state)
  save(root/'commands'/str(m['number'])/'result.json',dict(state='completed',completion=completion))
finally:save(root/'result.json',dict(error=None,retirement=dict(passed=True,errors=[],signal_error=None)))
