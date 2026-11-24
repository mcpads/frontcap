"""Observed committed-text ranges and the app-owned UIKit editing receipt."""
from native_ios_generic_ui import exact,canonical,text,validate_text_input,REASONS


def validate_arguments(start,end,replacement):
    if type(start) is not int or type(end) is not int or not 0<=start<=end<=16384 or not text(replacement,16384):
        raise ValueError('Invalid UIKit edit range or text budget')


def validate_target(catalog,arguments):
    start,end,replacement=(arguments[k] for k in ('start','end','text'))
    validate_arguments(start,end,replacement)
    focus=catalog['text_input']
    if focus['state']!='focused' or focus['marked'] or catalog['transitioning']:
        raise ValueError('UIKit edit requires a focused committed-text field')
    matched=any(t['reference']==arguments['reference'] and t['role']==focus['role']
        and t['unique'] and t['enabled'] and t['hittable'] and t['selector_value']==focus[t['selector_key']]
        for t in catalog['targets'])
    raw=focus['text'].encode('utf-16-le');units=[int.from_bytes(raw[n:n+2],'little') for n in range(0,len(raw),2)]
    def boundary(n):
        return n<=len(units) and (n==0 or n==len(units) or not (0xd800<=units[n-1]<=0xdbff and 0xdc00<=units[n]<=0xdfff))
    if (not matched or not boundary(start) or not boundary(end)
            or len(units)-(end-start)+len(replacement.encode('utf-16-le'))//2>16384):
        raise ValueError('UIKit edit target or UTF-16 range differs from observation')
    return focus


def validate_result(result,request):
    common=('input','app','reference','observation','now_ns')
    if (any(result.get(k)!=request.arguments[k] for k in ('reference','observation'))
            or type(result.get('now_ns')) is not int or not 0<=result['now_ns']<2**63):
        raise ValueError('UIKit edit observation differs')
    if result.get('input')=='not_applied':
        exact(result,common+('reason',))
        if result['reason'] not in REASONS:raise ValueError('Unknown UIKit rejection')
        return
    exact(result,common+('receipt',))
    if result['input']!='uikit_text_control':raise ValueError('Unresolved UIKit text delivery')
    receipt=result['receipt']
    exact(receipt,('generation','pid','port','request','last_id','status','replayed','result'))
    source=receipt['request'];exact(source,('generation','request_id','operation','arguments'))
    args=source['arguments'];exact(args,('basis','start','end','text'))
    validate_text_input(args['basis']);validate_arguments(args['start'],args['end'],args['text'])
    if (not canonical(receipt['generation']) or receipt['generation']!=source['generation']
            or type(receipt['pid']) is not int or receipt['pid']!=result['app']['pid']
            or type(receipt['port']) is not int or not 0<receipt['port']<65536
            or type(receipt['last_id']) is not int or not 0<receipt['last_id']<2**63
            or type(source['request_id']) is not int or source['request_id']!=receipt['last_id']
            or source['operation']!='replace_text' or receipt['status']!='ok' or receipt['replayed'] is not False
            or any(args[k]!=request.arguments[k] for k in ('start','end','text'))
            or args['basis']['state']!='focused' or args['basis']['marked']):
        raise ValueError('UIKit source edit receipt differs from request')
    value=receipt['result']
    if value.get('input')=='uikit_text_input':
        exact(value,('input','before','after'))
        validate_text_input(value['before']);validate_text_input(value['after'])
        if value['before']!=args['basis']:raise ValueError('UIKit edit basis changed before insertion')
    else:
        exact(value,('input','reason','current'));validate_text_input(value['current'])
        if value['input']!='not_applied' or value['reason'] not in (
            'input_unavailable','input_changed','composition_active','invalid_range','text_budget','range_unavailable'):
            raise ValueError('Unknown source text rejection')
