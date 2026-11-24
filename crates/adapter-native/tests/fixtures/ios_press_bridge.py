# Copied into a temporary executor fixture by ios_execution_tests.rs. Real durable bridges,
# simulated source results; this fixture never contacts a device or clock socket.
import argparse
import hashlib
import json
from pathlib import Path
from dataclasses import asdict
from native_ios_clock_bridge import Bridge as ClockBridge
from native_ios_ui_bridge import Bridge as UiBridge

parser=argparse.ArgumentParser()
parser.add_argument('--directory',type=Path)
parser.add_argument('--descriptor',type=Path)
parser.add_argument('--timeout',type=float)
args=parser.parse_args();root=args.directory.parent
call=json.load(sys.stdin)
clock_channel=args.directory.name=='clock-client'

def forbidden(*args,**kwargs):raise AssertionError('Unexpected clock transport')

def source(endpoint,request,**options):
    with (root/'deliveries').open('a') as log:log.write(request.operation+'\n')
    app=json.loads((root/'ui-client/app.json').read_text())
    app_endpoint=dict(generation=app['generation'],pid=app['pid'],port=12345)
    observation='abcd0000-0000-0000-0000-000000000009'
    if request.operation=='observe_targets':
        artifacts=[]
        for name in ('screen.png','hierarchy.txt'):
            path='observations/'+observation+'/'+name
            file=root/path;file.parent.mkdir(parents=True,exist_ok=True);file.write_bytes(name.encode())
            artifacts.append(dict(path=path,bytes=len(name),sha256=hashlib.sha256(name.encode()).hexdigest()))
        catalog=dict(observation=observation,keyboard=False,transitioning=False,scrolls=[],text_input=dict(state='unfocused'),
            targets=[dict(role='text_field',type=49,identifier='title',label='Title',value='',selector_key='identifier',
                selector_value='title',unique=True,enabled=True,selected=False,hittable=True,intersects_screen=True,
                frame_mpt=[0,0,1000,1000],reference=observation+'-0')])
        result=dict(state='observed',app=app_endpoint,now_ns=500000000,catalog=catalog,artifacts=artifacts)
    elif (root/'reject').exists():
        result=dict(input='not_applied',app=app_endpoint,**request.arguments,reason='observation_changed')
    else:
        result=json.loads((root/'press.json').read_text())
        result.update(request.arguments,app=app_endpoint)
        for name in ('before','began','ended'):result[name]['generation']=app['generation']
        result['clock_reply'].update(request=request.arguments['clock_request'],generation=app['generation'],
            pid=app['pid'],port=12345,last_id=request.arguments['clock_request']['request_id'])
        (root/'clock.json').write_text(json.dumps(result['clock_reply']))
    reply=dict(request=asdict(request),generation=request.generation,pid=123,port=12346,
        last_id=request.request_id,status='ok',result=result,replayed=False)
    args.descriptor.write_text(json.dumps(reply));return reply

bridge=(ClockBridge(args.descriptor,args.directory,transport=forbidden) if clock_channel else
        UiBridge(args.descriptor,args.directory,source,generic=True))
fault=root/'fault'
if (not clock_channel and call['request']['operation']=='press_target'
        and fault.exists() and fault.read_text()=='before_ui'):
    fault.unlink();sys.exit(3)
reply=bridge.handle(call)
phase=call['call'] if clock_channel else call['request']['operation']
if fault.exists() and fault.read_text()==phase:
    fault.unlink()
    sys.exit(3) # Applied/published outcome, but no bridge response reaches the executor.
print(json.dumps(reply))
