"""Register one observed View tap, deliver once, then recover only its source window receipt."""
import base64
import json
from pathlib import Path


def validate_receipt(value, owner, request):
    if (value.get('request')!=request or any(value.get(k)!=owner[k] for k in ('generation','pid'))
            or type(value.get('last_id')) is not int or value['last_id']!=request['request_id']
            or type(value.get('now_ns')) is not int or not 0<=value['now_ns']<2**63):
        raise RuntimeError('Tap receipt differs from bound owner/request')
    state=value.get('state')
    if state=='rejected':
        if value.get('reason') not in ('stale_observation','changed_view_state','unsupported_target','unexpected_touch'):
            raise RuntimeError('Unknown tap rejection')
    elif state in ('armed','pending','completed'):
        if value['now_ns']!=request['now_ns'] or 'reason' in value:raise RuntimeError('Tap time or outcome changed')
    else:raise RuntimeError('Invalid tap state')
    if state=='completed':
        if any(type(value.get(k)) is not bool for k in ('down_handled','up_handled')):raise RuntimeError('Missing window delivery')
    elif any(k in value for k in ('down_handled','up_handled')):raise RuntimeError('Unexpected window delivery')


def register(command, package, request, timeout, method="tap"):
    # content parses colon-delimited extras; encode the bounded request without delimiters.
    return command('shell','content','call','--uri','content://'+package+'.views','--method',method,
        '--extra','generation:s:'+request['generation'],'--extra','request_base64:s:'+base64.b64encode(json.dumps(request,separators=(',',':')).encode()).decode(),timeout=timeout)


def execute(command, directory, owner, request, timeout):
    from native_android_view_touch import execute_touch
    return execute_touch(command,directory,owner,request,timeout,kind='tap',validate=validate_receipt,
        register=lambda c,p,r,t:register(c,p,r,t),
        dispatch=lambda run,r:run('shell','input','tap',str(r['x']),str(r['y'])))


def main():
    import argparse
    import sys
    from native_android_diagnostics import run_command
    parser=argparse.ArgumentParser(description=__doc__)
    for key in ('adb','serial','directory'):parser.add_argument('--'+key,required=True)
    parser.add_argument('--timeout',type=float,required=True);args=parser.parse_args()
    raw=sys.stdin.buffer.read(65537)
    if len(raw)>65536:raise ValueError('Tap bridge request exceeds budget')
    call=json.loads(raw)
    if set(call)!= {'owner','request'} or call['owner']['serial']!=args.serial:raise ValueError('Unexpected tap bridge call')
    root=Path(args.directory);root.mkdir(parents=True,exist_ok=True)
    def command(*tail,timeout):return run_command([args.adb,'-s',args.serial,*tail],root,timeout=timeout)
    print(json.dumps(execute(command,root,call['owner'],call['request'],args.timeout)))


if __name__=='__main__':main()
