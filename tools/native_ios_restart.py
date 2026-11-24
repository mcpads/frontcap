"""Validate an explicit UIKit process replacement before committing its runner sequence."""
import errno
import re
import unicodedata
import uuid


def identity(app):
    if (type(app) is not dict or set(app) != {'simulator', 'generation', 'pid'}
            or type(app['pid']) is not int or not 0 < app['pid'] < 2**31
            or type(app['generation']) is not str or str(uuid.UUID(app['generation'])) != app['generation']
            or type(app['simulator']) is not str or str(uuid.UUID(app['simulator'])) != app['simulator'].lower()):
        raise ValueError('Invalid UIKit restart app identity')
    return app


def validate_url(url):
    if (type(url) is not str or len(url.encode('utf-8')) > 8192
            or re.fullmatch(r'[A-Za-z][A-Za-z0-9+.-]{0,63}:.+', url) is None
            or any(c.isspace() or unicodedata.category(c) == 'Cc' for c in url)):
        raise ValueError('Invalid UIKit entry URL')
    return url


def replacement(result, previous, url=None):
    identity(previous)
    fields = {'state', 'retirement', 'reads'}
    if url is not None:
        validate_url(url); fields.add('url')
    if (type(result) is not dict or set(result) != fields or result['state'] != 'restarted'
            or result.get('url') != url):
        raise ValueError('Missing UIKit restart completion')
    retired, reads = result['retirement'], result['reads']
    if (type(retired) is not dict or set(retired) != {'endpoint', 'process_absent', 'port_closed', 'process_errno', 'port_errno'}
            or retired['process_absent'] is not True or retired['port_closed'] is not True
            or type(retired['process_errno']) is not int or retired['process_errno'] != errno.ESRCH
            or type(retired['port_errno']) is not int or retired['port_errno'] != errno.ECONNREFUSED
            or type(reads) is not dict or set(reads) != {'endpoint', 'now_ns'}
            or type(reads['now_ns']) is not int or reads['now_ns'] != 0):
        raise ValueError('UIKit restart lacks old retirement or new zero clock')
    for endpoint in [retired['endpoint'], reads['endpoint']]:
        if type(endpoint) is not dict or set(endpoint) != {'generation', 'pid', 'port'}:
            raise ValueError('Invalid UIKit restart endpoint')
        identity(dict(simulator=previous['simulator'], generation=endpoint['generation'], pid=endpoint['pid']))
        if type(endpoint['port']) is not int or not 0 < endpoint['port'] < 65536:
            raise ValueError('Invalid UIKit restart endpoint port')
    old, new = retired['endpoint'], reads['endpoint']
    if old['generation'] != previous['generation'] or old['pid'] != previous['pid']:
        raise ValueError('UIKit restart retired a different app')
    if new['generation'] == old['generation'] or new['pid'] == old['pid']:
        raise ValueError('UIKit restart reused its old process')
    return dict(simulator=previous['simulator'], generation=new['generation'], pid=new['pid'])
