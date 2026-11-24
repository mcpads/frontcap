"""One bound clock operation for a source View app on the caller's owned emulator.

Reuses ClockClient's durable cursor and provider transport. Pending requests only read receipts;
this bridge never resends an uncertain command or acquires/replaces a target.
"""
import argparse
from dataclasses import asdict
import fcntl
import json
from pathlib import Path
import sys
import time
from native_android_control import ClockClient, ClockRejected, validate_reply
from native_clock_control import Request, atomic_json, recorded_outcome
from native_receipt import publish_receipt
from native_android_diagnostics import run_command


def execute(command, directory, owner, request, timeout):
    directory=Path(directory);directory.mkdir(parents=True,exist_ok=True)
    if request.generation!=owner['generation'] or type(owner['pid']) is not int or not 0<owner['pid']<2**31:
        raise ValueError('Clock request differs from app owner')
    if type(timeout) not in (int,float) or not 0<timeout<=300:raise ValueError('Invalid clock budget')
    deadline=time.monotonic()+timeout
    def bound(*args,timeout=20):
        remaining=deadline-time.monotonic()
        if remaining<=0:raise TimeoutError('Clock bridge deadline elapsed')
        raw=command(*args,timeout=min(timeout,remaining))
        if time.monotonic()>deadline:raise TimeoutError('Clock reply arrived after deadline')
        if len(raw)>1024*1024:raise RuntimeError('Clock reply exceeds budget')
        if args==('shell','run-as',owner['package'],'cat','files/clock-control.json'):
            value=json.loads(raw)
            if any(value.get(k)!=owner[k] for k in ('generation','pid')):
                raise RuntimeError('Clock target changed; retain original request')
        return raw
    client=ClockClient(bound,directory/'client',owner['package'],transport='provider',receipt_timeout=timeout)
    data=asdict(request)
    with (directory/'bridge.lock').open('a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        owner_path=directory/'owner.json'
        if owner_path.exists():
            if json.loads(owner_path.read_text())!=owner:raise RuntimeError('Clock bridge owner changed')
        else:
            if client.path.exists() or request.request_id!=1:raise RuntimeError('Missing clock bridge owner')
            atomic_json(owner_path,owner)
        state=json.loads(client.path.read_text()) if client.path.exists() else None
        if state and (state.get('package')!=owner['package'] or state.get('generation')!=request.generation or state.get('transport')!='provider'):
            raise RuntimeError('Clock cursor owner/transport changed')
        try:
            if state and state.get('pending') is not None:
                if state['pending']!=data:raise RuntimeError('Another clock request is pending')
                receipt=client.recover(expected_request=request)
            elif state and state.get('last_reply',{}).get('request')==data:
                receipt=recorded_outcome(client.directory,validate_reply)
            elif state and state.get('last_rejection',{}).get('request')==data:
                receipt=state['last_rejection']
            else:
                if (state and state.get('next_id')!=request.request_id) or (not state and request.request_id!=1):
                    raise RuntimeError('Clock cursor differs from bound request')
                if bound('shell','pidof',owner['package']).decode().split()!=[str(owner['pid'])]:
                    raise RuntimeError('Owned app is not live')
                try:receipt=client.request(request.operation,request.delta_ns,expected_request=request)
                except ClockRejected:raise
                except Exception as original:
                    # A lost submission reply can follow publication. Only read the exact receipt.
                    try:receipt=client.recover(expected_request=request)
                    except ClockRejected:raise
                    except Exception:raise original
        except ClockRejected as error:receipt=error.reply
        validate_reply(receipt,request)
        if any(receipt.get(k)!=owner[k] for k in ('generation','pid')):raise RuntimeError('Foreign clock completion')
        result=dict(channel='clock',request=data,generation=receipt['generation'],pid=receipt['pid'],
            last_id=receipt['last_id'],before_ns=receipt['before_ns'],now_ns=receipt['now_ns'],applied=receipt['applied'])
        if receipt['status']=='rejected':
            if receipt.get('error') not in ('invalid_request','clock_overflow') or receipt['last_id']!=request.request_id-1 or receipt['before_ns']!=receipt['now_ns']:
                raise RuntimeError('Rejection is not a non-consuming outcome')
            result.update(channel='clock_rejected',error=receipt['error'])
        result['receipt_sha256']=publish_receipt(directory/'receipts',receipt)
        return result


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    for key in ('adb','serial','directory'):parser.add_argument('--'+key,required=True)
    parser.add_argument('--timeout',type=float,required=True);args=parser.parse_args()
    raw=sys.stdin.buffer.read(65537)
    if len(raw)>65536:raise ValueError('Clock request exceeds budget')
    call=json.loads(raw)
    if set(call)!= {'owner','request'} or call['owner']['serial']!=args.serial:raise ValueError('Unexpected clock call')
    directory=Path(args.directory);directory.mkdir(parents=True,exist_ok=True)
    def command(*tail,timeout):return run_command([args.adb,'-s',args.serial,*tail],directory,timeout=timeout)
    print(json.dumps(execute(command,directory,call['owner'],Request(**call['request']),args.timeout)))


if __name__=='__main__':main()
