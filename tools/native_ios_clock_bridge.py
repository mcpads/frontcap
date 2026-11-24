"""One-call clock bridge for a separately owned iOS app; never acquires or replaces a simulator."""
import argparse
from dataclasses import asdict
import fcntl
import hashlib
import json
import os
from pathlib import Path
import sys
import time
import uuid

from native_clock_control import Request, atomic_json, recorded_outcome
from native_ios_control import ClockClient, validate_reply, validate_external_pending
from native_ios_transport import exchange


def exact(value, fields):
    if type(value) is not dict or set(value) != set(fields):
        raise ValueError('Unexpected bridge fields')


def identity(value):
    exact(value, ('simulator','generation','pid'))
    if (type(value['simulator']) is not str or str(uuid.UUID(value['simulator'])) != value['simulator'].lower()
            or str(uuid.UUID(value['generation'])) != value['generation']
            or type(value['pid']) is not int or not 0 < value['pid'] < 2**31):
        raise ValueError('Invalid iOS app identity')


def sync_directory(path):
    descriptor = os.open(path, os.O_RDONLY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


class Bridge:
    def __init__(self, descriptor, directory, timeout=10, transport=exchange):
        self.descriptor, self.directory = Path(descriptor).resolve(), Path(directory)
        if not 0 < timeout <= 60:
            raise ValueError('Invalid bridge transport budget')
        self.timeout, self.transport = timeout, transport

    def handle(self, call):
        deadline=time.monotonic()+self.timeout
        transport_failed=False
        def remaining():
            budget=deadline-time.monotonic()
            if budget<=0: raise TimeoutError('iOS clock bridge deadline elapsed')
            return budget
        def transmit(endpoint, request):
            nonlocal transport_failed
            budget=remaining()
            try: return self.transport(endpoint,request,timeout=budget)
            except Exception:
                transport_failed=True
                raise
        def validate_owner(receipt):
            remaining()
            self.same_app(receipt,call['identity'])
        if type(call) is not dict or call.get('call') not in ('initialize','execute','recover','reserve','reserve_ui','release_ui'):
            raise ValueError('Unknown iOS clock call')
        initialize = call['call']=='initialize'
        ui_call=call['call'] in ('reserve_ui','release_ui')
        fields=('call','identity','source_sha256') if initialize else ('call','identity','request')
        if ui_call:fields+=('ui_identity','ui_request')
        if call['call']=='release_ui':fields+=('receipt_sha256',)
        exact(call, fields)
        identity(call['identity'])
        if initialize:
            source = call['source_sha256']
            if type(source) is not str or len(source)!=64 or any(c not in '0123456789abcdef' for c in source):
                raise ValueError('Invalid frozen source hash')
        else:
            exact(call['request'], ('generation','request_id','operation','delta_ns'))
            request = Request(**call['request'])
            if request.generation != call['identity']['generation']:
                raise ValueError('Request belongs to another app')
        self.directory.mkdir(parents=True,exist_ok=True)
        with (self.directory/'bridge.lock').open('a') as lock:
            fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
            path = self.directory/'owner.json'
            owner = json.loads(path.read_text()) if path.exists() else None
            client = ClockClient(self.descriptor,self.directory/'clock',transmit,validate_owner=validate_owner)
            if owner is not None:
                exact(owner, ('identity','source_sha256','descriptor'))
                if owner['identity']!=call['identity'] or owner['descriptor']!=str(self.descriptor):
                    raise RuntimeError('App identity or descriptor changed')
            if initialize:
                with (client.directory/'clock-client.lock').open('a') as client_lock:
                    fcntl.flock(client_lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
                    expected = dict(identity=call['identity'],source_sha256=source,descriptor=str(self.descriptor))
                    if owner is None:
                        if client.path.exists():
                            raise RuntimeError('Unowned existing clock cursor')
                        endpoint = json.loads(self.descriptor.read_text())
                        self.same_app(endpoint,call['identity'])
                        if endpoint.get('status')!='ready' or endpoint.get('last_id')!=0:
                            raise RuntimeError('Clock already controlled; retain original owner')
                        atomic_json(path,expected)
                        atomic_json(client.path,dict(generation=call['identity']['generation'],next_id=1,pending=None))
                        sync_directory(client.directory)
                        sync_directory(self.directory)
                    elif owner != expected or not client.path.exists():
                        raise RuntimeError('Initialization evidence changed or incomplete')
                    return dict(reply='ready',identity=call['identity'],source_sha256=source)
            if owner is None:
                raise RuntimeError('Missing clock owner')
            state = json.loads(client.path.read_text())
            validate_external_pending(state)
            if (state.get('generation')!=request.generation or type(state.get('next_id')) is not int
                    or not 0 < state['next_id'] <= 2**63 or 'pending' not in state):
                raise RuntimeError('Invalid clock cursor')
            if state.get('last_reply') is not None:
                previous = state['last_reply']
                self.same_app(previous,call['identity'])
                previous_request = Request(**previous['request'])
                validate_reply(previous,previous_request)
                if state['next_id'] != previous_request.request_id+1:
                    raise RuntimeError('Clock cursor differs from last committed receipt')
            if state['pending'] is not None and (state['pending'].get('request_id')!=state['next_id']
                    or state['pending'].get('generation')!=state['generation']):
                raise RuntimeError('Pending clock cursor differs')
            data = asdict(request)
            if ui_call:
                from native_ios_control import validate_ui_binding
                from native_ios_press_reservation import owned_ui, require_ready, require_unused
                binding=dict(identity=call['ui_identity'],request=call['ui_request'])
                validate_ui_binding(binding,request)
                ui_directory=self.directory.parent/'ui-client'
                with owned_ui(ui_directory,owner,call['identity'],binding) as ui_state:
                    if call['call']=='reserve_ui':
                        if state['pending'] is None:require_ready(ui_directory,ui_state,binding,call['identity'])
                        client.reserve(request,binding=binding)
                        return dict(reply='ui_reserved',identity=call['identity'],request=data,
                            ui_identity=call['ui_identity'],ui_request=call['ui_request'])
                    client.release_ui(request,binding,call['receipt_sha256'],lambda:
                        require_unused(ui_directory,ui_state,binding,call['identity'],request,call['receipt_sha256']))
                    result=dict(reply='ui_released',identity=call['identity'],request=data,
                        ui_identity=call['ui_identity'],ui_request=call['ui_request'],receipt_sha256=call['receipt_sha256'])
                    releases=self.directory/'releases';releases.mkdir(exist_ok=True)
                    saved=releases/(call['receipt_sha256']+'.json')
                    if saved.exists():
                        if json.loads(saved.read_text())!=result:raise RuntimeError('Retained UI release differs')
                    else:
                        atomic_json(saved,result);sync_directory(releases)
                    return result
            if call['call']=='reserve':
                reserved = client.reserve(request)
                return dict(reply='reserved',identity=call['identity'],request=asdict(reserved))
            if call['call']=='recover' and state['pending']!=data and state.get('last_reply',{}).get('request')!=data:
                raise RuntimeError('No saved iOS clock outcome; recovery cannot dispatch')
            if state['pending'] == data:
                # No resume/transport: an uncertain command may only use its exact retained receipt.
                endpoint = json.loads(self.descriptor.read_text())
                self.same_app(endpoint,call['identity'])
                receipt = client.recover(expected_request=request)
            elif state['pending'] is not None:
                raise RuntimeError('Another request remains pending')
            elif state.get('last_reply',{}).get('request')==data:
                receipt = recorded_outcome(client.directory,validate_reply)
            elif state['next_id']==request.request_id:
                endpoint = json.loads(self.descriptor.read_text())
                self.same_app(endpoint,call['identity'])
                try: receipt = client.request(request.operation,request.delta_ns,expected_request=request)
                except Exception as original:
                    if not transport_failed: raise
                    # Resolve only an exact saved outcome, without extending this call or retrying
                    # its transport. Identity/validation errors are not transport recovery triggers.
                    try:
                        remaining()
                        receipt = client.recover(expected_request=request)
                    except Exception: raise original
            else:
                raise RuntimeError('Client sequence differs from bound request')
            self.same_app(receipt,call['identity'])
            validate_reply(receipt,request)
            raw = json.dumps(receipt,indent=2).encode()
            digest = hashlib.sha256(raw).hexdigest()
            receipts = self.directory/'receipts';receipts.mkdir(exist_ok=True)
            saved = receipts/(digest+'.json')
            if saved.exists():
                if saved.read_bytes()!=raw:
                    raise RuntimeError('Receipt digest path contains different bytes')
            else:
                atomic_json(saved,receipt)
                sync_directory(receipts)
            keys = ('request','generation','pid','last_id','status','before_ns','now_ns','applied','advance_result','drain_result')
            return dict(reply='completed',completion={**{key:receipt[key] for key in keys},'receipt_sha256':digest})

    @staticmethod
    def same_app(receipt, expected):
        if receipt.get('generation')!=expected['generation'] or type(receipt.get('pid')) is not int or receipt['pid']!=expected['pid']:
            raise RuntimeError('App generation/PID differs from owner')


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--descriptor',required=True,type=Path)
    parser.add_argument('--directory',required=True,type=Path)
    parser.add_argument('--timeout',type=float,default=10)
    args=parser.parse_args()
    try:
        raw=sys.stdin.buffer.read(65537)
        if len(raw)>65536: raise ValueError('Bridge request too large')
        reply=Bridge(args.descriptor,args.directory,args.timeout).handle(json.loads(raw))
    except Exception as error:
        reply=dict(reply='unresolved',error=f'{type(error).__name__}: {error}')
    print(json.dumps(reply))


if __name__=='__main__':
    main()
