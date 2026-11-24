"""Fake owned SDK/device boundary for real Rust adapter/recorder/MCP checks."""
import fcntl
import hashlib
import struct
import uuid
import zlib
import json
from pathlib import Path
import plistlib
import sys
import time

config = json.loads(Path(sys.argv[1]).read_text())
output = Path(sys.argv[2]);output.mkdir()
project = Path(config['project'])
control = Path(sys.argv[sys.argv.index('--control-directory') + 1]) if '--control-directory' in sys.argv else output


def save(path, value):
    temporary = path.with_suffix('.tmp');temporary.write_text(json.dumps(value));temporary.replace(path)


def system_reply(call):
    global confirmed
    request = call['request']; artifacts = []
    client = output/'system-ui/client'; (client/'receipts').mkdir(parents=True, exist_ok=True)
    if request['operation'] == 'observe_system_alerts':
        catalog = json.loads((project/'system-catalog.json').read_text())
        capture = str(uuid.uuid4()); catalog['observation'] = capture
        for i, button in enumerate(b for a in catalog['alerts'] for b in a['buttons']): button['reference'] = capture+'-'+str(i)
        def chunk(kind, data): return struct.pack('>I',len(data))+kind+data+struct.pack('>I',zlib.crc32(kind+data))
        png = b'\x89PNG\r\n\x1a\n'+chunk(b'IHDR',struct.pack('>IIBBBBB',1,1,8,6,0,0,0))+chunk(b'IDAT',zlib.compress(b'\0\xff\0\0\xff'))+chunk(b'IEND',b'')
        for name, raw in [('screen.png',png),('hierarchy.txt',b'system fixture')]:
            relative = 'observations/'+capture+'/'+name;path = client/relative
            path.parent.mkdir(parents=True,exist_ok=True);path.write_bytes(raw)
            artifacts.append(dict(path=relative,bytes=len(raw),sha256=hashlib.sha256(raw).hexdigest()))
        result = dict(state='system_observed',catalog=catalog,artifacts=artifacts)
        outcome = dict(outcome='system_alerts_observed',catalog=catalog,artifacts=artifacts)
    elif request['operation'] == 'set_host_app_state':
        args=request['arguments'];foreground=args['state']=='foreground'
        change=dict(bundle=args['bundle'],pid=args['pid'],requested=args['state'],before_state=3 if foreground else 4,app_state=4 if foreground else 3)
        result=dict(state='host_app_state_changed',change=change)
        outcome=dict(outcome='host_app_state_changed',change=change)
    else:
        confirmed = True
        result = dict(input='xctest_system_tap',**request['arguments'])
        outcome = dict(outcome='system_button_tapped',**request['arguments'])
    receipt = dict(request=request,generation=runner['generation'],pid=runner['pid'],port=12345,
                   last_id=request['request_id'],status='ok',result=result)
    raw = json.dumps(receipt).encode();digest = hashlib.sha256(raw).hexdigest()
    (client/'receipts'/(digest+'.json')).write_bytes(raw)
    reply = dict(reply='completed',completion=dict(request=request,generation=runner['generation'],pid=runner['pid'],
        last_id=request['request_id'],outcome=outcome,receipt_sha256=digest))
    results = output/'system-ui/results';results.mkdir(exist_ok=True)
    save(results/(str(request['request_id'])+'.json'),reply)
    with (output/'system-effects.jsonl').open('a') as stream:stream.write(json.dumps(request)+'\n')
    (project/'system-dispatched').touch()
    while (project/'hold-system').exists():time.sleep(.01)
    return json.dumps(reply)


with (output/'session.lock').open('w') as lock:
    fcntl.flock(lock, fcntl.LOCK_EX)
    assert json.loads(sys.stdin.readline()) == {'command': 'start'}
    device = 'emulator-5580' if config['platform'] == 'android' else 'ABCD0000-0000-0000-0000-000000000001'
    target = (dict(platform='android', serial=device, package='org.example.client') if config['platform'] == 'android'
              else dict(platform='ios', simulator=device.lower(), bundle='org.example.client'))
    ready = dict(owner=dict(platform=config['platform'], device=device,
        generation='a'*48, source_sha256=config['source_sha256']), endpoint=dict(port=12345, token='b'*48),
        external_url_target=target)
    runner = dict(simulator=device, generation='abcd0000-0000-0000-0000-000000000002',pid=123)
    if config.get('system_ui'):
        ready['system_ui'] = runner
        (output/'system-ui').mkdir()
        save(output/'system-ui/host-app-state.json',True)
    app = output/'fixture.app';app.mkdir()
    (app/'Info.plist').write_bytes(plistlib.dumps(dict(CFBundleIdentifier='org.example.client',
        CFBundleURLTypes=[dict(CFBundleURLSchemes=['client-test'])])))
    if config.get('installed_app'):
        data = output/'data';data.mkdir();(data/'state.json').write_text('persisted work')
        application = dict(simulator=device,bundle='org.example.client',
            containers=dict(app=str(app),data=str(data),data_device=1,data_inode=2,installed_library_modes={}),
            process=dict(pid=101,birth='first',command=str(app/'Runner')+' --enable-dart-profiling --verify-entry-points'),
            endpoint=dict(ready['endpoint'],generation=ready['owner']['generation'],scenario='session'))
        if config['platform']=='android':
            application=dict(serial=device,package='org.example.client',activity='org.example.client.Main',
                installation=dict(package='org.example.client',apk='/data/app/package/base.apk',apk_sha256=config['installed_app']['apk_sha256'],
                    data='/data/user/0/org.example.client',data_device=1,data_inode=2),
                process=dict(pid=101,boot_id='abcd0000-0000-0000-0000-000000000001',start_ticks=100,command='org.example.client'),
                endpoint=dict(application['endpoint'],port=23456),forwarding=dict(serial=device,host_port=ready['endpoint']['port'],device_port=23456))
        save(output/'application.json',application)
    save(output/'ready.json',ready)
    pending = None
    confirmed = False
    state = 'interrupted'
    for line in sys.stdin:
        message = json.loads(line)
        if message == {'command': 'stop'}:
            state = 'stopped';break
        assert message['command'] == 'host'
        number = message['number'];directory = output/'commands'/str(number)
        request = json.loads((directory/'request.json').read_text())
        if request['operation'] == 'set_app_state':
            assert request['generation']==ready['owner']['generation']
            evidence=directory/'app-state';evidence.mkdir()
            save(evidence/'before.json',application)
            if config['platform']=='ios':
                reply=json.loads(system_reply(request['call']))
                native=dict(platform='ios',completion=reply['completion'])
            else:
                focused=['org.example.client/org.example.client.Main','dev.launcher/dev.launcher.Home']
                if request['state']=='foreground':focused.reverse()
                native=dict(platform='android',focused_before=focused[0],focused_after=focused[1])
                for name,component in zip(('window-before.txt','window-after.txt'),focused):
                    (evidence/name).write_text('mCurrentFocus=Window{1 u0 '+component+'}')
            save(evidence/'after.json',application);save(evidence/'native.json',native)
            value=dict(before=application,after=application,native=native,ready=ready,state=request['state'])
            save(directory/'command.log',value)
            with (output/'state-effects.jsonl').open('a') as stream:stream.write(json.dumps(request)+'\n')
            (project/'state-published').touch()
            while (project/'hold-state').exists():time.sleep(.01)
            text=json.dumps(value)
        elif request['operation'] == 'begin_entry':
            assert pending is None and request['generation']==ready['owner']['generation']
            confirmed=False
            pending=dict(command_number=number,previous=application,entry=dict(
                request=dict(target=target,generation=request['generation'],url=request['url']),
                confirmation='unobserved',destination='unobserved'))
            save(directory/'command.log',pending)
            with (output/'entry-effects.jsonl').open('a') as stream:stream.write(json.dumps(pending)+'\n')
            (project/'entry-dispatched').touch()
            while (project/'hold-entry').exists():time.sleep(.01)
            text=json.dumps(pending)
        elif request['operation'] == 'poll_entry':
            assert pending is not None and request['begin_command']==pending['command_number']
            assert request['generation']==pending['entry']['request']['generation']
            if not confirmed:value=dict(state='waiting',begin_command=pending['command_number'],readiness=dict(phase='process'))
            else:
                generation=uuid.uuid4().hex+uuid.uuid4().hex[:16]
                ready=dict(ready,owner=dict(ready['owner'],generation=generation))
                application=dict(application,process=dict(application['process'],pid=application['process']['pid']+1,birth=generation,command=str(app/'Runner')),
                    endpoint=dict(application['endpoint'],generation=generation))
                value=dict(state='ready',begin_command=pending['command_number'],replacement=dict(
                    previous=pending['previous'],current=application,ready=ready,entry=pending['entry']))
                save(output/'current-ready.json',ready);pending=None
            save(directory/'command.log',value)
            with (output/'entry-polls.jsonl').open('a') as stream:stream.write(json.dumps(value)+'\n')
            (project/'entry-polled').touch()
            while (project/'hold-entry-poll').exists():time.sleep(.01)
            text=json.dumps(value)
        elif request['operation'] == 'restart_app':
            assert request['generation']==ready['owner']['generation']
            previous=application
            generation=uuid.uuid4().hex+uuid.uuid4().hex[:16]
            ready=dict(ready,owner=dict(ready['owner'],generation=generation))
            changed_process=dict(application['process'],pid=application['process']['pid']+1)
            if config['platform']=='android':
                changed_process['start_ticks']+=1
                ready=dict(ready,endpoint=dict(ready['endpoint'],port=ready['endpoint']['port']+1))
                application=dict(application,forwarding=dict(application['forwarding'],host_port=ready['endpoint']['port']))
            else:changed_process['birth']=generation
            application=dict(application,process=changed_process,
                endpoint=dict(application['endpoint'],generation=generation))
            save(output/'current-ready.json',ready)
            value=dict(previous=previous,current=application,ready=ready)
            if request.get('url') is not None:
                assert config['platform']=='android'
                value['entry']=dict(request=dict(target=target,generation=request['generation'],url=request['url']),confirmation='unobserved',destination='unobserved')
            save(directory/'command.log',value)
            with (output/'restart-effects.jsonl').open('a') as stream:stream.write(json.dumps(value)+'\n')
            (project/'restart-dispatched').touch()
            while (project/'hold-restart').exists():time.sleep(.01)
            text=json.dumps(value)
        elif request['operation'] == 'system_ui':
            text = system_reply(request['call'])
            if (project/'lose-system-reply').exists():
                state='failed';break
        elif request['operation'] == 'installed_apps':
            text = '{ "org.example.client" = { CFBundleIdentifier = "org.example.client"; Path = "%s"; }; }' % app
        else:
            with (output/'effects.jsonl').open('a') as stream:
                stream.write(json.dumps(request['request'])+'\n')
            (project/'dispatched').touch()
            while (project/'hold').exists():
                time.sleep(.01)
            if (project/'lose-host-reply').exists():
                state = 'failed';break
            text = 'Status: ok\nActivity: org.example.client/.Main\n' if config['platform'] == 'android' else ''
        log = directory/'command.log'
        if not log.exists():
            temporary = log.with_suffix('.tmp')
            temporary.write_text(text)
            temporary.replace(log)
        save(directory/'reply.json', dict(number=number, error=None))
    save(control/'result.json', dict(state=state, retirement=dict(passed=True, errors=[], signal_error=None), error=None))
