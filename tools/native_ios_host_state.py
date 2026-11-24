"""Native host visibility receipts from the independent XCTest runner; no app clock."""
import re


def validate_arguments(arguments):
    if (type(arguments) is not dict or set(arguments) != {'bundle','pid','state'}
            or type(arguments['bundle']) is not str or len(arguments['bundle']) > 255
            or not re.fullmatch(r'[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+',arguments['bundle'])
            or type(arguments['pid']) is not int or not 0 < arguments['pid'] < 2**31
            or arguments['state'] not in ('background','foreground')):
        raise ValueError('Invalid host visibility arguments')


def validate_result(result, request):
    if type(result) is not dict or set(result) != {'state','change'} or result['state'] != 'host_app_state_changed':
        raise ValueError('Unconfirmed host visibility result')
    change=result['change']; args=request.arguments
    if (type(change) is not dict or set(change) != {'bundle','pid','requested','before_state','app_state'}
            or any(change[k] != args[k] for k in ('bundle','pid')) or change['requested'] != args['state']
            or type(change['before_state']) is not int or type(change['app_state']) is not int):
        raise ValueError('Host visibility result belongs to another app or request')
    before,after=change['before_state'],change['app_state']
    if not ((before == 4 and after in (2,3)) if args['state'] == 'background' else (before in (2,3) and after == 4)):
        raise ValueError('Host visibility transition was not observed')
