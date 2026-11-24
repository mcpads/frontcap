"""Shared one-dispatch source registration and receipt-only recovery for View touch gestures."""
import fcntl
import json
from pathlib import Path
import subprocess
import time
from native_clock_control import atomic_json
from native_receipt import publish_receipt
from native_android_input import focused_component, require_uncovered_tap


def execute_touch(command, directory, owner, request, timeout, *, kind, validate, register, dispatch):
    if request['generation']!=owner['generation']:raise ValueError('Foreign touch generation')
    if type(timeout) not in (int,float) or not 0<timeout<=300:raise ValueError('Invalid touch budget')
    root=Path(directory);root.mkdir(parents=True,exist_ok=True);deadline=time.monotonic()+timeout
    def remaining():
        value=deadline-time.monotonic()
        if value<=0:raise TimeoutError('Touch unresolved; preserve pending request without redispatch')
        return value
    def run(*args):
        raw=command(*args,timeout=remaining());remaining()
        if len(raw)>1024*1024:raise RuntimeError('Touch response exceeds budget')
        return raw
    last_observed=None
    def read():
        nonlocal last_observed
        try:value=json.loads(run('shell','run-as',owner['package'],'cat','files/view-'+kind+'.json'))
        except (subprocess.CalledProcessError,ValueError):return None
        if any(value.get(k)!=owner[k] for k in ('generation','pid')):raise RuntimeError('Touch owner changed')
        if value.get('request')==request:
            validate(value,owner,request)
            if value!=last_observed:
                atomic_json(root/'observed-receipt.json',value);last_observed=value
            return value
        if type(value.get('last_id')) is int and value['last_id']>=request['request_id']:
            raise RuntimeError('Foreign touch occupies the sequence')
        return None
    def result(value):
        return dict(channel='view_'+kind,receipt=value,receipt_sha256=publish_receipt(root/'receipts',value))
    with (root/'bridge.lock').open('a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        path=root/'state.json'
        state=json.loads(path.read_text()) if path.exists() else dict(owner=owner,next_id=1,pending=None,last_result=None)
        if state['owner']!=owner:raise RuntimeError('Touch bridge owner changed')
        saved=state['last_result']
        if saved and saved['request']==request:
            validate(saved,owner,request);return result(saved)
        fresh=state['pending'] is None
        if not fresh and state['pending']!=request:raise RuntimeError('Another touch remains pending')
        if fresh:
            if state['next_id']!=request['request_id']:raise RuntimeError('Touch sequence differs')
            if run('shell','pidof',owner['package']).decode().split()!=[str(owner['pid'])]:raise RuntimeError('Owned touch target is not live')
            state.update(pending=request,phase='registering');atomic_json(path,state)
            try:
                reply=register(command,owner['package'],request,remaining());remaining()
                (root/('registration-'+str(request['request_id'])+'.txt')).write_bytes(reply)
                if reply.strip()!=b'Result: Bundle[{submitted=true}]':raise RuntimeError('Touch registration unconfirmed')
            except Exception as error:
                atomic_json(root/'registration-error.json',dict(error=f'{type(error).__name__}: {error}'))
                fresh=False # Uncertain registration never grants a new physical delivery.
        while True:
            value=read()
            if value is not None:
                if value['state'] in ('completed','rejected'):
                    answer=result(value)
                    state.update(next_id=request['request_id']+1,pending=None,phase=None,last_result=value);atomic_json(path,state)
                    return answer
                if fresh and value['state']=='armed':
                    window=run('shell','dumpsys','window');(root/('window-'+str(request['request_id'])+'.txt')).write_bytes(window)
                    if focused_component(window)!=owner['package']+'/'+owner['activity']:raise RuntimeError('Touch Activity is not foreground')
                    require_uncovered_tap(window,request['x'],request['y'])
                    if kind=='scroll':require_uncovered_tap(window,request['end_x'],request['end_y'])
                    state['phase']='dispatched';atomic_json(path,state);fresh=False
                    def deliver(*args):
                        # Reserve time to reconcile even when the shell/input reply never arrives.
                        raw=command(*args,timeout=remaining()/2);remaining()
                        if len(raw)>1024*1024:raise RuntimeError('Touch delivery reply exceeds budget')
                        return raw
                    try:dispatch(deliver,request)
                    except Exception as error:
                        atomic_json(root/'delivery-error.json',dict(error=f'{type(error).__name__}: {error}'))
                    # Regardless of transport outcome, only the bound source receipt can complete it.
            time.sleep(min(.02,remaining()))
