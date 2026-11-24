"""One-call XCTest lifecycle/input bridge for a separately owned runner and app."""
import argparse
from dataclasses import asdict
import fcntl
import hashlib
import json
import os
import stat
import uuid
from pathlib import Path
import sys
import time
import tempfile

from native_clock_control import atomic_json, recorded_outcome
from native_ios_clock_bridge import exact, identity, sync_directory, Bridge as ClockBridge
from native_ios_transport import exchange
from native_ios_ui import UIClient, validate_reply
from native_ios_generic_ui import UIKitUIRequest, validate_catalog


def copy_observation_artifacts(entries, source, destination):
    if type(entries) is not list or len(entries) != 2:
        raise ValueError('Incomplete observation artifacts')
    capture = None
    for entry, filename in zip(entries, ('screen.png', 'hierarchy.txt')):
        exact(entry, ('path', 'bytes', 'sha256'))
        parts = entry['path'].split('/')
        if (len(parts) != 3 or parts[0] != 'observations' or parts[2] != filename
                or str(uuid.UUID(parts[1])) != parts[1]):
            raise ValueError('Invalid observation artifact path')
        if capture is not None and capture != parts[1]: raise ValueError('Mixed captures')
        capture = parts[1]
        if (type(entry['bytes']) is not int or not 0 < entry['bytes'] <= 16*1024*1024
                or type(entry['sha256']) is not str or len(entry['sha256']) != 64
                or any(c not in '0123456789abcdef' for c in entry['sha256'])):
            raise ValueError('Invalid observation artifact metadata')
        target = destination/entry['path']
        origin = target if target.exists() or target.is_symlink() else source/entry['path']
        base = destination if origin == target else source
        for root in (base, destination):
            for parent in (root/'observations', root/'observations'/capture):
                if parent.is_symlink(): raise ValueError('Linked observation directory')
        fd = os.open(origin, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        with os.fdopen(fd, 'rb') as f:
            if not stat.S_ISREG(os.fstat(f.fileno()).st_mode): raise ValueError('Non-file observation')
            raw = f.read(16*1024*1024+1)
        if len(raw) != entry['bytes'] or hashlib.sha256(raw).hexdigest() != entry['sha256']:
            raise RuntimeError('Observation artifact changed')
        if origin != target:
            target.parent.mkdir(parents=True, exist_ok=True)
            # Publish only complete bytes. A killed copy may leave a temporary file,
            # but must not poison the final path used to recover this saved capture.
            descriptor, temporary = tempfile.mkstemp(prefix='.observation-', suffix='.tmp', dir=target.parent)
            try:
                with os.fdopen(descriptor, 'wb') as f:
                    f.write(raw);f.flush();os.fsync(f.fileno())
                os.link(temporary, target)  # Never overwrite established evidence.
            finally:
                Path(temporary).unlink(missing_ok=True)
        sync_directory(target.parent);sync_directory(target.parent.parent);sync_directory(destination)
    return entries


class Bridge:
    def __init__(self, descriptor, directory, transport=exchange, *, timeout=20, generic=False, system_only=False):
        self.descriptor, self.directory, self.transport = Path(descriptor).resolve(), Path(directory), transport
        if type(timeout) not in (int,float) or not 0<timeout<=60:raise ValueError('Invalid UI bridge budget')
        self.timeout=timeout
        if generic and system_only: raise ValueError('Choose one XCTest surface')
        self.system_only=system_only
        self.surface='system' if system_only else 'uikit'
        self.request_type=UIKitUIRequest

    @staticmethod
    def validate_app_result(receipt, request, runner, app):
        """Validate app ownership before UIClient commits, not only before publishing completion."""
        result=receipt['result']
        if request.operation in ('observe_system_alerts', 'tap_system_button', 'set_host_app_state'):
            return  # Runner/device evidence; no app foreground or clock sample is asserted.
        if request.operation=='set_app_state':
            if request.arguments['app'] != app: raise RuntimeError('Host transition differs from bound app')
            ClockBridge.same_app(result['app'],app)
        elif request.operation=='restart':
            from native_ios_restart import replacement
            if request.arguments['app'] != app: raise RuntimeError('Restart request differs from bound app')
            replacement(result, app, request.arguments.get('url'))
        elif request.operation=='launch':
            endpoint=result['reads']['endpoint']
            launched=dict(simulator=runner['simulator'],generation=endpoint['generation'],pid=endpoint['pid'])
            identity(launched)
            if app is not None and app!=launched:
                raise RuntimeError('Launch result replaces the bound app')
        elif request.operation in ('observe_targets','tap_target','press_target','replace_text','scroll_target'):
            ClockBridge.same_app(result['app'],app)
        else:
            ClockBridge.same_app(result['retirement']['endpoint'],app)

    def handle(self, call):
        deadline=time.monotonic()+self.timeout
        transport_failed=False
        def remaining():
            budget=deadline-time.monotonic()
            if budget<=0:raise TimeoutError('XCTest bridge deadline elapsed')
            return budget
        def transmit(endpoint,request,**options):
            nonlocal transport_failed
            budget=remaining()
            try:return self.transport(endpoint,request,timeout=budget,max_response_bytes=1024*1024)
            except Exception:
                transport_failed=True
                raise
        def validate_owner(receipt):
            remaining()
            ClockBridge.same_app(receipt,call['identity'])
        if type(call) is not dict or call.get('call') not in ('initialize','execute','recover'):
            raise ValueError('Unknown XCTest call')
        initialize=call['call']=='initialize'
        recovering=call['call']=='recover'
        exact(call, ('call','identity','source_sha256') if initialize else ('call','identity','app','request'))
        identity(call['identity'])
        if initialize:
            source=call['source_sha256']
            if type(source) is not str or len(source)!=64 or any(c not in '0123456789abcdef' for c in source):
                raise ValueError('Invalid frozen source hash')
        else:
            exact(call['request'],('generation','request_id','operation','arguments'))
            request=self.request_type(**call['request'])
            if request.generation!=call['identity']['generation']:
                raise ValueError('Request belongs to another XCTest runner')
            allowed=('launch','restart','set_app_state','observe_system_alerts','tap_system_button','observe_targets','tap_target','press_target','replace_text','scroll_target','terminate','finish')
            if self.system_only: allowed=('observe_system_alerts','tap_system_button','set_host_app_state')
            if request.operation not in allowed:
                raise ValueError('Operation not yet connected to adapter')
            if request.operation not in ('launch','restart','set_app_state','set_host_app_state'):
                exact(request.arguments,('reference','observation','direction') if request.operation=='scroll_target' else ('reference','observation','start','end','text') if request.operation=='replace_text' else ('reference','observation','clock_request') if request.operation=='press_target' else ('reference','observation') if request.operation in ('tap_target','tap_system_button') else ())
            if request.operation=='launch':
                if call['app'] is not None: raise ValueError('Launch cannot replace a bound app')
            elif self.system_only:
                if call['app'] is not None: raise ValueError('System-only calls cannot bind an app')
            else:
                identity(call['app'])
                if request.operation in ('restart','set_app_state') and request.arguments['app'] != call['app']:
                    raise ValueError('Restart request differs from supplied app')
                if request.operation=='press_target' and request.arguments['clock_request']['generation']!=call['app']['generation']:
                    raise ValueError('Press clock belongs to another app')
                if call['app']['simulator'].lower()!=call['identity']['simulator'].lower():
                    raise ValueError('App belongs to another simulator')
        self.directory.mkdir(parents=True,exist_ok=True)
        with (self.directory/'bridge.lock').open('a') as lock:
            fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
            path=self.directory/'owner.json'
            owner=json.loads(path.read_text()) if path.exists() else None
            client=UIClient(self.descriptor,self.directory/'ui',transmit,request_type=self.request_type,
                validate_owner=validate_owner,
                validate_result=lambda receipt, bound: self.validate_app_result(receipt,bound,call['identity'],app))
            if owner is not None:
                exact(owner,('identity','source_sha256','descriptor','surface'))
                if owner['surface']!=self.surface:raise RuntimeError('UI surface changed')
                if owner['identity']!=call['identity'] or owner['descriptor']!=str(self.descriptor):
                    raise RuntimeError('XCTest identity or descriptor changed')
            if initialize:
                with (client.directory/'ui-client.lock').open('a') as client_lock:
                    fcntl.flock(client_lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
                    expected=dict(identity=call['identity'],source_sha256=source,descriptor=str(self.descriptor))
                    expected['surface']=self.surface
                    if owner is None:
                        if client.path.exists(): raise RuntimeError('Unowned UI cursor')
                        endpoint=json.loads(self.descriptor.read_text());ClockBridge.same_app(endpoint,call['identity'])
                        if endpoint.get('status')!='ready' or endpoint.get('last_id')!=0:
                            raise RuntimeError('XCTest already controlled')
                        if (endpoint.get('system_only',False) is not self.system_only or
                                self.system_only and endpoint.get('system_alerts') is not True):
                            raise RuntimeError('XCTest surface capability differs')
                        atomic_json(path,expected)
                        atomic_json(client.path,dict(generation=call['identity']['generation'],next_id=1,pending=None))
                        sync_directory(client.directory);sync_directory(self.directory)
                    elif owner!=expected or not client.path.exists():
                        raise RuntimeError('Incomplete or changed initialization')
                    return dict(reply='ready',identity=call['identity'],source_sha256=source)
            if owner is None: raise RuntimeError('Missing XCTest owner')
            app_path=self.directory/'app.json'
            app=json.loads(app_path.read_text()) if app_path.exists() else None
            state=json.loads(client.path.read_text());data=asdict(request)
            if (state.get('generation')!=request.generation or type(state.get('next_id')) is not int
                    or not 0<state['next_id']<=2**63 or 'pending' not in state):
                raise RuntimeError('Invalid UI cursor')
            previous=state.get('last_reply')
            if request.operation!='launch' and app!=call['app']:
                from native_ios_restart import replacement
                if (request.operation!='restart' or previous is None or previous.get('request')!=data
                        or state['pending'] is not None or app!=replacement(previous['result'],call['app'],request.arguments.get('url'))):
                    raise RuntimeError('Missing or changed bound app')
            if request.operation=='restart': app=call['app']  # Validate the original effect, never adopt its replacement.
            if previous is not None:
                ClockBridge.same_app(previous,call['identity'])
                validate_reply(previous,self.request_type(**previous['request']))
                if state['next_id']!=previous['request']['request_id']+1:
                    raise RuntimeError('UI cursor differs from last receipt')
            if state['pending'] is not None and (state['pending'].get('request_id')!=state['next_id']
                    or state['pending'].get('generation')!=state['generation']):
                raise RuntimeError('Pending UI cursor differs')
            # Reject conflicting/stale identities before publishing an admission or a source basis.
            if state['pending'] is not None and state['pending'] != data:
                raise RuntimeError('Another UI request remains pending')
            if state['pending'] is None and (previous is None or previous['request'] != data):
                if state['next_id'] != request.request_id:
                    raise RuntimeError('UI sequence differs from bound request')
                if request.operation == 'launch' and (app is not None or request.request_id != 1):
                    raise RuntimeError('This owner cannot launch a replacement app')
            if recovering and state['pending']!=data and (previous is None or previous['request']!=data):
                raise RuntimeError('No saved XCTest outcome; recovery cannot dispatch')
            if recovering: call={**call,'call':'execute'}
            admission=self.directory/'pending.json'
            if admission.exists() and json.loads(admission.read_text())!=call:
                raise RuntimeError('Unresolved runner/app request blocks new work')
            if (request.operation=='tap_system_button' and state['pending'] is None
                    and (previous is None or previous['request']!=data)):
                from native_ios_system_ui import target
                if previous is None or previous['request']['operation']!='observe_system_alerts':
                    raise RuntimeError('System input requires the latest alert observation')
                observed=previous['result']
                target(observed['catalog'], request.arguments['observation'], request.arguments['reference'])
                copy_observation_artifacts(observed['artifacts'],self.descriptor.parent,self.directory)
            if (request.operation in ('tap_target','press_target','replace_text','scroll_target') and state['pending'] is None
                    and (previous is None or previous['request']!=data)):
                if previous is None or previous['request']['operation']!='observe_targets' or previous['result'].get('state')!='observed':
                    raise RuntimeError('Generic input requires the latest usable catalog')
                observed=previous['result'];ClockBridge.same_app(observed['app'],app)
                catalog=validate_catalog(observed['catalog'])
                if (catalog['transitioning'] or catalog['observation']!=request.arguments['observation'] or
                        not any(t['reference']==request.arguments['reference'] and t['unique'] and t['enabled'] and t['hittable']
                                and (request.operation!='press_target' or t['role'] in ('text_field','text_view'))
                                for t in catalog['targets'])):
                    raise RuntimeError('Stale or unready UIKit target')
                copy_observation_artifacts(observed['artifacts'],self.descriptor.parent,self.directory)
                if request.operation=='scroll_target':
                    from native_ios_scroll import target
                    basis=dict(request=data,basis=target(catalog,request.arguments['reference'],request.arguments['direction']),now_ns=observed['now_ns'])
                    folder=self.directory/'scroll-bases';folder.mkdir(exist_ok=True)
                    path=folder/(str(request.request_id)+'.json')
                    if path.exists() and json.loads(path.read_text())!=basis:raise RuntimeError('Retained scroll basis differs')
                    atomic_json(path,basis);sync_directory(folder)
                if request.operation=='replace_text':
                    from native_ios_edit import validate_target
                    basis=dict(request=data,basis=validate_target(catalog,request.arguments),now_ns=observed['now_ns'])
                    folder=self.directory/'edit-bases';folder.mkdir(exist_ok=True)
                    path=folder/(str(request.request_id)+'.json')
                    if path.exists() and json.loads(path.read_text())!=basis:
                        raise RuntimeError('Retained text edit basis differs')
                    atomic_json(path,basis);sync_directory(folder)
            if not admission.exists():
                atomic_json(admission,call);sync_directory(self.directory)
            if state['pending']==data:
                ClockBridge.same_app(json.loads(self.descriptor.read_text()),call['identity'])
                receipt=client.recover(expected_request=request)  # Saved receipt only, never repeat an uncertain tap.
            elif state['pending'] is not None:
                raise RuntimeError('Another UI request remains pending')
            elif previous is not None and previous['request']==data:
                receipt=recorded_outcome(client.directory,validate_reply,request_type=self.request_type,prefix='ui')
            elif state['next_id']==request.request_id:
                ClockBridge.same_app(json.loads(self.descriptor.read_text()),call['identity'])
                try:receipt=client.request(request.operation,expected_request=request,**request.arguments)
                except Exception as original:
                    if not transport_failed:raise
                    try:
                        remaining()
                        receipt=client.recover(expected_request=request)  # Read only, never retry a possibly performed tap.
                    except Exception:raise original
            else: raise RuntimeError('UI sequence differs from bound request')
            ClockBridge.same_app(receipt,call['identity']);validate_reply(receipt,request)
            result=receipt['result']
            if request.operation=='restart':
                from native_ios_restart import replacement
                launched=replacement(result,app,request.arguments.get('url'))
                atomic_json(app_path,launched);sync_directory(self.directory)
                outcome=dict(outcome='restarted',previous=app,app=launched,process_absent=True,port_closed=True,now_ns=0)
            elif request.operation=='launch':
                endpoint=result['reads']['endpoint']
                launched=dict(simulator=call['identity']['simulator'],generation=endpoint['generation'],pid=endpoint['pid'])
                identity(launched)
                if app is not None and app!=launched: raise RuntimeError('Launch result replaces the bound app')
                atomic_json(app_path,launched);sync_directory(self.directory)
                outcome=dict(outcome='launched',app=launched)
            elif request.operation=='set_host_app_state':
                outcome=dict(outcome='host_app_state_changed',change=result['change'])
            elif request.operation=='observe_system_alerts':
                from native_ios_system_ui import validate_catalog as validate_system_catalog
                outcome=dict(outcome='system_alerts_observed',catalog=validate_system_catalog(result['catalog']),
                    artifacts=copy_observation_artifacts(result['artifacts'],self.descriptor.parent,self.directory))
            elif request.operation=='tap_system_button':
                outcome=dict(outcome='system_button_not_applied' if result['input']=='not_applied' else 'system_button_tapped',
                    reference=request.arguments['reference'],observation=request.arguments['observation'])
                if result['input']=='not_applied':outcome['reason']=result['reason']
            elif request.operation=='set_app_state':
                outcome=dict(outcome='app_state_changed',app=app,change=result['change'])
            elif request.operation=='observe_targets' and result['state']=='unavailable':
                outcome=dict(outcome='observation_unavailable',app=app,now_ns=result['now_ns'],reason=result['reason'],
                    artifacts=copy_observation_artifacts(result['artifacts'],self.descriptor.parent,self.directory) if result['artifacts'] else [])
            elif request.operation=='observe_targets':
                ClockBridge.same_app(result['app'],app)
                outcome=dict(outcome='observed_targets',app=app,now_ns=result['now_ns'],
                    catalog=validate_catalog(result['catalog']),
                    artifacts=copy_observation_artifacts(result['artifacts'],self.descriptor.parent,self.directory))
            elif request.operation=='scroll_target':
                if result['input']=='not_applied':
                    outcome=dict(outcome='target_not_applied',app=app,reference=result['reference'],observation=result['observation'],reason=result['reason'])
                else:
                    basis=json.loads((self.directory/'scroll-bases'/(str(request.request_id)+'.json')).read_text())
                    if basis['request']!=data or basis['basis']!=result['before'] or basis['now_ns']!=result['now_ns']:
                        raise RuntimeError('Scroll differs from retained observation basis')
                    outcome=dict(outcome='scrolled',app=app,**{k:result[k] for k in ('reference','observation','direction','before','after','now_ns')})
            elif request.operation=='replace_text':
                if result['input']=='uikit_text_control':
                    basis=json.loads((self.directory/'edit-bases'/(str(request.request_id)+'.json')).read_text())
                    if (basis['request']!=data or result['receipt']['request']['arguments']['basis']!=basis['basis']
                            or result['now_ns']!=basis['now_ns']):
                        raise RuntimeError('Source edit differs from retained observation basis')
                outcome=dict(outcome='text_edit',app=app,reference=result['reference'],observation=result['observation'],
                    now_ns=result['now_ns'],delivery={k:v for k,v in result.items() if k not in ('app','reference','observation','now_ns')})
            elif request.operation=='press_target':
                ClockBridge.same_app(result['app'],app)
                if result['input']=='not_applied':
                    outcome=dict(outcome='target_press_not_applied',app=app,**request.arguments,reason=result['reason'])
                else:
                    outcome=dict(outcome='target_pressed',app=app,evidence={key:value for key,value in result.items() if key!='app'})
            elif request.operation=='tap_target':
                ClockBridge.same_app(result['app'],app)
                outcome=dict(outcome='target_not_applied' if result['input']=='not_applied' else 'target_tapped',
                    app=app,reference=request.arguments['reference'],observation=request.arguments['observation'])
                if result['input']=='not_applied':outcome['reason']=result['reason']
            else:
                retirement=result['retirement'];ClockBridge.same_app(retirement['endpoint'],app)
                if retirement.get('process_absent') is not True or retirement.get('port_closed') is not True:
                    raise RuntimeError('App retirement remains unconfirmed')
                outcome=dict(outcome='terminated',app=app,process_absent=True,port_closed=True)
            raw=json.dumps(receipt,indent=2).encode();digest=hashlib.sha256(raw).hexdigest()
            folder=self.directory/'receipts';folder.mkdir(exist_ok=True);saved=folder/(digest+'.json')
            if saved.exists():
                if saved.read_bytes()!=raw: raise RuntimeError('Receipt bytes differ')
            else: atomic_json(saved,receipt);sync_directory(folder)
            completion=dict(request=data,generation=receipt['generation'],pid=receipt['pid'],
                            last_id=receipt['last_id'],outcome=outcome,receipt_sha256=digest)
            if request.operation=='restart':
                proof=self.directory/'restarts'/f'{request.request_id}.json'
                proof.parent.mkdir(exist_ok=True)
                if proof.exists() and json.loads(proof.read_text())!=completion:raise RuntimeError('Restart proof changed')
                if not proof.exists():atomic_json(proof,completion);sync_directory(proof.parent)
            admission.unlink();sync_directory(self.directory)
            return dict(reply='completed',completion=completion)


def main(*, generic=False, system_only=False):
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--descriptor',required=True,type=Path);parser.add_argument('--directory',required=True,type=Path)
    parser.add_argument('--timeout',type=float,default=20)
    args=parser.parse_args()
    try:
        limit=256*1024 if generic or system_only else 65536
        raw=sys.stdin.buffer.read(limit+1)
        if len(raw)>limit: raise ValueError('Bridge request too large')
        reply=Bridge(args.descriptor,args.directory,timeout=args.timeout,generic=generic,system_only=system_only).handle(json.loads(raw))
    except Exception as error: reply=dict(reply='unresolved',error=f'{type(error).__name__}: {error}')
    print(json.dumps(reply))


if __name__=='__main__': main()
