"""Deliver hash-bound source edits under the caller's owner; retain pending receipt-only recovery."""
import hashlib
import json
import re
import time


def submit_edit(command, package, request, *, timeout=20):
    if not re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]*(?:\.[A-Za-z_][A-Za-z_0-9]*)+',package):
        raise ValueError('Invalid application package')
    if type(timeout) not in (int,float) or not 0<timeout<=300:raise ValueError('Invalid edit submission budget')
    deadline=time.monotonic()+timeout
    def remaining():
        value=deadline-time.monotonic()
        if value<=0:raise TimeoutError('Edit submission budget exhausted')
        return value
    raw=json.dumps(request,ensure_ascii=True,separators=(',',':')).encode()
    if len(raw)>196608:raise ValueError('Edit request exceeds transport budget')
    digest=hashlib.sha256(raw).hexdigest()
    # Staging alone never edits UI. Only a matching digest submitted to the provider can do so.
    output=command('shell','-T','run-as',package,'tee','files/view-edit-'+digest+'.json',input=raw,timeout=remaining())
    if output!=raw:raise RuntimeError('Edit staging unconfirmed; no submission sent')
    return command('shell','content','call','--uri','content://'+package+'.views','--method','edit',
        '--extra','generation:s:'+request['generation'],'--extra','payload_sha256:s:'+digest,timeout=remaining())


def validate_receipt(value, owner, request):
    if value.get('request')!=request or any(value.get(k)!=owner[k] for k in ('generation','pid')):
        raise RuntimeError('Edit receipt differs from bound owner/request')
    if (type(value.get('last_id')) is not int or value['last_id']!=request['request_id']
            or type(value.get('now_ns')) is not int or not 0<=value['now_ns']<2**63):
        raise RuntimeError('Invalid edit receipt sequence/time')
    if value.get('state')=='rejected':
        if value.get('reason') not in ('stale_observation','changed_view_state','unsupported_target','invalid_range',
                'split_surrogate','text_budget_exceeded','field_limit_exceeded','invalid_unicode') or 'before' in value or 'after' in value:
            raise RuntimeError('Invalid edit rejection')
    elif value.get('state')=='completed':
        before=value['before'].encode('utf-16-le');start=request['start'];end=request['end']
        if not 0<=start<=end<=len(before)//2<=16384:raise RuntimeError('Invalid completed edit range')
        expected=before[:start*2].decode('utf-16-le')+request['text']+before[end*2:].decode('utf-16-le')
        if (value['now_ns']!=request['now_ns'] or value.get('after')!=expected
                or len(expected.encode('utf-16-le'))//2>16384 or 'reason' in value):
            raise RuntimeError('Completed edit differs from requested replacement')
    else:raise RuntimeError('Edit has no terminal receipt')


def execute(command, directory, owner, request, timeout):
    from pathlib import Path
    import fcntl
    from native_clock_control import atomic_json
    from native_receipt import publish_receipt
    if request['generation']!=owner['generation']:raise ValueError('Foreign edit generation')
    if type(timeout) not in (int,float) or not 0<timeout<=300:raise ValueError('Invalid edit budget')
    root=Path(directory);root.mkdir(parents=True,exist_ok=True);deadline=time.monotonic()+timeout
    def remaining():
        value=deadline-time.monotonic()
        if value<=0:raise TimeoutError('Edit deadline exhausted; preserve pending request')
        return value
    def bound(*args,timeout=20,**options):
        raw=command(*args,timeout=min(timeout,remaining()),**options)
        remaining()
        if len(raw)>1024*1024:raise RuntimeError('Edit command response exceeds budget')
        return raw
    with (root/'bridge.lock').open('a') as lock:
        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
        path=root/'state.json'
        state=json.loads(path.read_text()) if path.exists() else dict(owner=owner,next_id=1,pending=None,last_result=None)
        if state['owner']!=owner:raise RuntimeError('Edit bridge owner changed')
        saved=state['last_result']
        if saved and saved['request']==request:
            validate_receipt(saved,owner,request)
            return dict(channel='view_text',receipt=saved,receipt_sha256=publish_receipt(root/'receipts',saved))
        if state['pending'] is not None:
            if state['pending']!=request:raise RuntimeError('Another edit remains pending')
            # A restarted bridge only reads the original receipt. It never resubmits a pending edit.
        else:
            if state['next_id']!=request['request_id']:raise RuntimeError('Edit sequence differs')
            if bound('shell','pidof',owner['package']).decode().split()!=[str(owner['pid'])]:
                raise RuntimeError('Owned edit target is not live')
            state['pending']=request;atomic_json(path,state)
            try:
                reply=submit_edit(bound,owner['package'],request,timeout=remaining())
                if reply.strip()!=b'Result: Bundle[{submitted=true}]':raise RuntimeError('Edit submission unconfirmed')
            except Exception as error:
                atomic_json(root/'submission-error.json',dict(error=f'{type(error).__name__}: {error}'))
                # Publication may precede a lost response. Only read its exact receipt within budget.
        import subprocess
        while True:
            try:
                raw=bound('shell','run-as',owner['package'],'cat','files/view-edit.json');value=json.loads(raw)
            except (subprocess.CalledProcessError,ValueError):value=None
            if value is not None:
                if any(value.get(k)!=owner[k] for k in ('generation','pid')):
                    raise RuntimeError('Edit target identity changed')
                if value.get('request')==request and value.get('state') in ('completed','rejected'):
                    validate_receipt(value,owner,request)
                    digest=publish_receipt(root/'receipts',value)
                    state.update(next_id=request['request_id']+1,pending=None,last_result=value);atomic_json(path,state)
                    return dict(channel='view_text',receipt=value,receipt_sha256=digest)
                if type(value.get('last_id')) is int and value['last_id']>=request['request_id'] and value.get('request')!=request:
                    raise RuntimeError('Foreign edit occupies the bound sequence')
            time.sleep(min(.02,remaining()))


def main():
    import argparse
    import sys
    from pathlib import Path
    from native_android_diagnostics import run_command
    parser=argparse.ArgumentParser(description=__doc__)
    for key in ('adb','serial','directory'):parser.add_argument('--'+key,required=True)
    parser.add_argument('--timeout',type=float,required=True);args=parser.parse_args()
    raw=sys.stdin.buffer.read(262145)
    if len(raw)>262144:raise ValueError('Edit bridge request exceeds budget')
    call=json.loads(raw)
    if set(call)!= {'owner','request'} or call['owner']['serial']!=args.serial:raise ValueError('Unexpected edit bridge call')
    root=Path(args.directory);root.mkdir(parents=True,exist_ok=True)
    def command(*tail,timeout,**options):return run_command([args.adb,'-s',args.serial,*tail],root,timeout=timeout,**options)
    print(json.dumps(execute(command,root,call['owner'],call['request'],args.timeout)))


if __name__=='__main__':main()
