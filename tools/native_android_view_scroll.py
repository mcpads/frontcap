"""Deliver one registered observed View swipe; never redispatch an unresolved gesture."""
from native_android_view_tap import validate_receipt as validate_touch_receipt, register as register_touch
from native_android_view_touch import execute_touch


def validate_receipt(value,owner,request):
    validate_touch_receipt(value,owner,request)
    if value['state']=='completed':
        if type(value.get('moves')) is not int or not 0<value['moves']<2**32:
            raise RuntimeError('Scroll receipt lacks delivered moves')
        for key in ('before_offset_px','after_offset_px'):
            if (type(value.get(key)) is not list or len(value[key])!=2
                or any(type(v) is not int or not -2**31<=v<2**31 for v in value[key])):
                raise RuntimeError('Scroll receipt lacks source offsets')
    elif any(key in value for key in ('moves','before_offset_px','after_offset_px')):
        raise RuntimeError('Unfinished scroll advertises delivery offsets')


def register(command,package,request,timeout):
    return register_touch(command,package,request,timeout,method='scroll')


def execute(command,directory,owner,request,timeout):
    return execute_touch(command,directory,owner,request,timeout,kind='scroll',validate=validate_receipt,
        register=register,dispatch=lambda run,r:run('shell','input','swipe',str(r['x']),str(r['y']),
            str(r['end_x']),str(r['end_y']),'600'))


def main():
    import argparse
    import json
    import sys
    from pathlib import Path
    from native_android_diagnostics import run_command
    parser=argparse.ArgumentParser(description=__doc__)
    for key in ('adb','serial','directory'):parser.add_argument('--'+key,required=True)
    parser.add_argument('--timeout',type=float,required=True);args=parser.parse_args()
    raw=sys.stdin.buffer.read(65537)
    if len(raw)>65536:raise ValueError('Scroll bridge request exceeds budget')
    call=json.loads(raw)
    if set(call)!= {'owner','request'} or call['owner']['serial']!=args.serial:raise ValueError('Unexpected scroll bridge call')
    root=Path(args.directory);root.mkdir(parents=True,exist_ok=True)
    def command(*tail,timeout):return run_command([args.adb,'-s',args.serial,*tail],root,timeout=timeout)
    print(json.dumps(execute(command,root,call['owner'],call['request'],args.timeout)))


if __name__=='__main__':main()
