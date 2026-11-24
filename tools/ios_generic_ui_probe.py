"""Installed generic bridge checks inside the clock probe's owned Simulator/build."""
from dataclasses import asdict
import json
from pathlib import Path
import subprocess
import signal
import time
import uuid

from native_clock_control import Request
from native_ios_clock_bridge import Bridge as ClockBridge
from native_ios_discovery import app_container
from native_ios_transport import exchange
from native_ios_ui_bridge import Bridge
from native_host_process import signal_owned_group, write_json


def check(simulator, bundle, product, project, build, output, env, source, source_sha256, target_name, *, focus_target=None):
    output.mkdir()
    command=['xcodebuild','test-without-building','-project',str(project),'-scheme',product,
        '-destination','platform=iOS Simulator,id='+simulator,'-derivedDataPath',str(build),
        '-parallel-testing-enabled','NO','-only-testing:'+product+'Tests/'+product+'Tests/testGenericBridge',
        '-resultBundlePath',str(output/'bridge.xcresult'),'CODE_SIGNING_ALLOWED=NO']
    write_json(output/'launch.json',dict(command=command,source_sha256=source_sha256))
    state=dict(state='running',phase='runner',wire_requests=[])
    def save(phase):
        state['phase']=phase;write_json(output/'status.json',state)
    with (output/'xctest.log').open('wb') as log:
        process=subprocess.Popen(command,stdout=log,stderr=subprocess.STDOUT,env=env,cwd=source,start_new_session=True)
        write_json(output/'launch.json',dict(command=command,source_sha256=source_sha256,pid=process.pid))
        try:
            deadline=time.monotonic()+45
            descriptor=None
            while time.monotonic()<deadline:
                if process.poll() is not None:raise RuntimeError('XCTest ended before endpoint: '+str(process.returncode))
                try:
                    container=app_container(simulator,bundle+'.uitests.xctrunner',output/'discovery',timeout=5)
                    candidate=container/'Documents/native-ui/ui-control.json'
                    if candidate.exists():descriptor=candidate;break
                except subprocess.CalledProcessError:
                    pass
                time.sleep(.1)
            if descriptor is None:raise TimeoutError('Generic XCTest endpoint unavailable')
            endpoint=json.loads(descriptor.read_text())
            runner_command=subprocess.run(['ps','-p',str(endpoint['pid']),'-o','command='],capture_output=True,text=True,check=True,timeout=5).stdout
            (output/'runner-process.txt').write_text(runner_command)
            if '/'+simulator+'/' not in runner_command or product+'Tests-Runner.app/'+product+'Tests-Runner' not in runner_command:
                raise RuntimeError('Endpoint is not in the owned simulator runner')
            runner=dict(simulator=simulator,generation=endpoint['generation'],pid=endpoint['pid'])
            write_json(output/'runner.json',runner)
            def transport(endpoint,request,**options):
                state['wire_requests'].append(asdict(request));save('wire-'+str(request.request_id))
                result=exchange(endpoint,request,**options)
                if request.request_id==5:
                    state['discarded_reply']=True;save('discarded-tap-reply')
                    raise EOFError('Probe discarded the applied tap reply')
                if focus_target and request.request_id==9:
                    state['discarded_press_reply']=True;save('discarded-press-reply')
                    raise EOFError('Probe discarded the timed press reply')
                return result
            bridge=Bridge(descriptor,output/'ui-client',transport,timeout=30,generic=True)
            bridge.handle(dict(call='initialize',identity=runner,source_sha256=source_sha256))
            app=None
            def call(number,operation,arguments=None,recover=False):
                value=dict(call='recover' if recover else 'execute',identity=runner,app=app,
                    request=dict(generation=runner['generation'],request_id=number,operation=operation,arguments=arguments or {}))
                suffix='-recover' if recover else ''
                write_json(output/(str(number)+suffix+'.call.json'),value)
                result=bridge.handle(value)
                write_json(output/(str(number)+suffix+'.result.json'),result)
                if result['reply']!='completed':raise RuntimeError('UI outcome unresolved')
                return result
            launched=call(1,'launch');app=launched['completion']['outcome']['app']
            write_json(output/'request-owner.json',dict(runner=runner,app=app,controller_id=str(uuid.uuid4()),source_sha256=source_sha256))
            application=app_container(simulator,bundle,output/'discovery',timeout=5)
            clock_descriptor=application/'Documents/clock-control.json'
            clock=ClockBridge(clock_descriptor,output/'clock-client')
            clock.handle(dict(call='initialize',identity=app,source_sha256=source_sha256))
            clock_id=0
            def clock_request(operation,delta_ns=0):
                nonlocal clock_id
                clock_id+=1
                request=Request(app['generation'],clock_id,operation,delta_ns)
                result=clock.handle(dict(call='execute',identity=app,request=asdict(request)))
                write_json(output/('clock-'+str(clock_id)+'.json'),result)
                if result['reply']!='completed':raise RuntimeError('Clock outcome unresolved')
                return result['completion']
            observed=call(2,'observe_targets')['completion']['outcome']
            def target(catalog,name=target_name):
                matches=[row for row in catalog['targets'] if name in (row['identifier'],row['label'])
                         and row['unique'] and row['enabled'] and row['hittable']]
                if len(matches)!=1:raise RuntimeError('Consumer action is not uniquely ready')
                return dict(reference=matches[0]['reference'],observation=catalog['observation'])
            stale_target=target(observed['catalog'])
            for delay in (.2,.7):
                save('hold-'+str(delay));time.sleep(delay)
                if clock_request('read')['now_ns']!=observed['now_ns']:raise RuntimeError('Clock moved during host hold')
            advanced=clock_request('advance',1000000)
            if advanced['status']!='ok':raise RuntimeError('Initial explicit advance failed')
            stale=call(3,'tap_target',stale_target)
            if stale['completion']['outcome'].get('reason')!='stale_observation':
                raise RuntimeError('Changed clock basis did not reject before input')
            if call(3,'tap_target',stale_target)!=stale:raise RuntimeError('No-effect retry changed its result')
            fresh=call(4,'observe_targets')['completion']['outcome']
            active_target=target(fresh['catalog'])
            applied=call(5,'tap_target',active_target)
            if not state.get('discarded_reply'):raise RuntimeError('Reply-loss control did not fire')
            # The bridge can recover the published receipt within the failed transport call.
            # A fresh bridge instance must then return that same durable result without dispatch.
            bridge=Bridge(descriptor,output/'ui-client',transport,timeout=30,generic=True)
            recovered=call(5,'tap_target',active_target,recover=True)
            if recovered!=applied:raise RuntimeError('Reopened bridge changed the recovered result')
            if applied['completion']['outcome']['outcome']!='target_tapped':raise RuntimeError('Applied tap not recovered')
            if call(5,'tap_target',active_target)!=applied:raise RuntimeError('Applied retry changed its result')
            progressed=clock_request('advance',500000000)
            if progressed['status']!='ok':raise RuntimeError('Explicit UI advance failed')
            final=call(6,'observe_targets')['completion']['outcome']
            if final['now_ns']!=observed['now_ns']+501000000:raise RuntimeError('Final UI clock differs')
            finish_id=7
            if focus_target:
                press_args=target(final['catalog'],focus_target)
                # Invalidate only the source observation, keeping the UI client's latest catalog.
                # The real source rejects this press before delivery; that receipt must release
                # only this UI reservation, leaving the clock sequence available for the next one.
                if clock_request('advance',1000000)['status']!='ok':raise RuntimeError('Stale-basis advance failed')
                clock_id+=1
                reserved=Request(app['generation'],clock_id,'advance',70000000)
                press_args['clock_request']=asdict(reserved)
                def reservation(number,arguments):
                    return dict(call='reserve_ui',identity=app,request=asdict(reserved),ui_identity=runner,
                        ui_request=dict(generation=runner['generation'],request_id=number,
                            operation='press_target',arguments=arguments))
                unused_call=reservation(7,press_args)
                write_json(output/'unused-clock.call.json',unused_call)
                if clock.handle(unused_call)['reply']!='ui_reserved':raise RuntimeError('Unused clock reservation unresolved')
                rejected=call(7,'press_target',press_args)
                rejection=rejected['completion']['outcome']
                if rejection['outcome']!='target_press_not_applied' or rejection['reason']!='stale_observation':
                    raise RuntimeError('Stale press was not rejected before delivery')
                release_call=dict(unused_call,call='release_ui',receipt_sha256=rejected['completion']['receipt_sha256'])
                write_json(output/'unused-clock.release.call.json',release_call)
                released=clock.handle(release_call)
                write_json(output/'unused-clock.released.json',released)
                clock=ClockBridge(clock_descriptor,output/'clock-client')
                if released['reply']!='ui_released' or clock.handle(release_call)!=released:
                    raise RuntimeError('Unused reservation release did not survive reopen')
                fresh_press=call(8,'observe_targets')['completion']['outcome']
                press_args=dict(target(fresh_press['catalog'],focus_target),clock_request=asdict(reserved))
                reserve_call=reservation(9,press_args)
                write_json(output/'press-clock.call.json',reserve_call)
                acknowledgement=clock.handle(reserve_call)
                write_json(output/'press-clock.reserved.json',acknowledgement)
                if acknowledgement['reply']!='ui_reserved':raise RuntimeError('Clock reservation unresolved')
                clock=ClockBridge(clock_descriptor,output/'clock-client')
                if clock.handle(reserve_call)!=acknowledgement:raise RuntimeError('Reopened reservation differs')
                try:clock.handle(release_call)
                except RuntimeError:pass
                else:raise RuntimeError('Old rejection released a new UI reservation')
                pressed=call(9,'press_target',press_args)
                if not state.get('discarded_press_reply'):raise RuntimeError('Press reply-loss control did not fire')
                if pressed['completion']['outcome']['outcome']!='target_pressed':raise RuntimeError('Timed press did not complete')
                bridge=Bridge(descriptor,output/'ui-client',transport,timeout=30,generic=True)
                if call(9,'press_target',press_args,recover=True)!=pressed or call(9,'press_target',press_args)!=pressed:
                    raise RuntimeError('Reopened timed press changed its result')
                recover_call=dict(call='recover',identity=app,request=asdict(reserved))
                recovered_clock=clock.handle(recover_call)
                write_json(output/'press-clock.recovered.json',recovered_clock)
                clock=ClockBridge(clock_descriptor,output/'clock-client')
                if clock.handle(recover_call)!=recovered_clock:raise RuntimeError('Recovered clock changed on reopen')
                evidence=pressed['completion']['outcome']['evidence']
                completion=recovered_clock['completion']
                if any(completion[key]!=evidence['clock_reply'][key] for key in
                       ('request','generation','pid','last_id','status','before_ns','now_ns','applied','advance_result','drain_result')):
                    raise RuntimeError('UI and clock receipts disagree')
                final=call(10,'observe_targets')['completion']['outcome']
                focus=final['catalog']['text_input']
                if (focus['state']!='focused' or focus_target not in (focus['identifier'],focus['label'])
                        or final['now_ns']!=observed['now_ns']+572000000):
                    raise RuntimeError('Controlled press did not focus the requested field at the expected time')
                time.sleep(.2)
                if clock_request('read')['now_ns']!=final['now_ns']:raise RuntimeError('Clock moved after press')
                state['press_evidence']=evidence
                finish_id=11
            state['final_catalog']=final['catalog']
            state['final_artifacts']=final['artifacts']
            call(finish_id,'finish')
            if process.wait(timeout=45)!=0:raise RuntimeError('XCTest exited unsuccessfully')
            if [r['request_id'] for r in state['wire_requests']]!=list(range(1,finish_id+1)):
                raise RuntimeError('A saved/uncertain UI request was redispatched')
            state['state']='passed';save('finished')
            return state
        except BaseException as error:
            state.update(state='failed',error=f'{type(error).__name__}: {error}');save('failed')
            raise
        finally:
            # This invocation retains the live Popen/group owner; never signal a saved PID.
            try:
                signal_owned_group(process.pid,signal.SIGTERM,output/'xctest.log')
                try:process.wait(timeout=10)
                except subprocess.TimeoutExpired:pass
            finally:
                signal_owned_group(process.pid,signal.SIGKILL,output/'xctest.log')
                process.wait()
                state['xctest_exit']=process.returncode
                write_json(output/'status.json',state)
