"""Validate native UIKit host state without inferring page disposal or response consumption."""
from native_ios_restart import identity

def validate_result(result, request):
    if type(result) is not dict or set(result) != {'state','app','change'} or result['state'] != 'app_state_changed':
        raise ValueError('Missing UIKit host transition receipt')
    endpoint=result['app'];bound=request.arguments['app'];identity(bound)
    if (type(endpoint) is not dict or set(endpoint) != {'generation','pid','port'}
            or endpoint['generation'] != bound['generation'] or type(endpoint['pid']) is not int or endpoint['pid'] != bound['pid']
            or type(endpoint['port']) is not int or not 0 < endpoint['port'] < 65536):
        raise ValueError('UIKit host transition changed its app endpoint')
    change=result['change']
    if (type(change) is not dict or set(change) != {'requested','before_state','app_state','now_ns'}
            or change['requested'] != request.arguments['state']
            or type(change['before_state']) is not int or type(change['app_state']) is not int):
        raise ValueError('Invalid UIKit host transition fields')
    if change['requested']=='background':
        valid=change['before_state']==4 and change['app_state'] in (2,3) and change['now_ns'] is None
    elif change['requested']=='foreground':
        valid=change['before_state'] in (2,3) and change['app_state']==4 and type(change['now_ns']) is int and 0<=change['now_ns']<2**63
    else:valid=False
    if not valid:raise ValueError('Unconfirmed UIKit host state transition')
    return change
