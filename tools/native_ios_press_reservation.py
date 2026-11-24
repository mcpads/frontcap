"""UI-owned proof for a reserved clock; no target transport or generic cancellation."""
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os

from native_ios_control import validate_ui_binding
from native_ios_generic_ui import UIKitUIRequest, validate_catalog
from native_ios_ui import validate_reply


@contextmanager
def owned_ui(directory, owner, app, binding):
    # Lock order: clock bridge -> UI bridge -> UI client -> clock client.
    # UI execution does not acquire the clock bridge lock.
    with (directory/'bridge.lock').open('r+') as bridge, (directory/'ui/ui-client.lock').open('r+') as client:
        fcntl.flock(bridge,fcntl.LOCK_EX|fcntl.LOCK_NB)
        fcntl.flock(client,fcntl.LOCK_EX|fcntl.LOCK_NB)
        ui_owner=json.loads((directory/'owner.json').read_text())
        if (ui_owner.get('identity')!=binding['identity'] or ui_owner.get('surface')!='uikit'
                or ui_owner.get('source_sha256')!=owner['source_sha256']
                or json.loads((directory/'app.json').read_text())!=app
                or binding['identity']['simulator'].lower()!=app['simulator'].lower()
                or binding['identity']['generation']==app['generation']
                or binding['identity']['pid']==app['pid']):
            raise RuntimeError('UI reservation owner differs')
        yield json.loads((directory/'ui/ui-client.json').read_text())


def require_ready(directory, state, binding, app):
    request=UIKitUIRequest(**binding['request'])
    previous=state.get('last_reply')
    if (state.get('generation')!=request.generation or state.get('pending') is not None
            or state.get('next_id')!=request.request_id or (directory/'pending.json').exists()
            or previous is None or previous['request']['operation']!='observe_targets'):
        raise RuntimeError('UI reservation requires the next observed input')
    observed=UIKitUIRequest(**previous['request']);validate_reply(previous,observed)
    catalog=validate_catalog(previous['result']['catalog'])
    endpoint=previous['result']['app']
    if (observed.request_id+1!=request.request_id or previous.get('pid')!=binding['identity']['pid']
            or endpoint['generation']!=app['generation'] or endpoint['pid']!=app['pid']
            or catalog['transitioning'] or catalog['observation']!=request.arguments['observation']
            or not any(t['reference']==request.arguments['reference'] and t['role'] in ('text_field','text_view')
                and t['unique'] and t['enabled'] and t['hittable'] for t in catalog['targets'])):
        raise RuntimeError('UI reservation target is stale or unready')


def require_unused(directory, state, binding, app, request, digest):
    validate_ui_binding(binding,request)
    if type(digest) is not str or len(digest)!=64 or any(c not in '0123456789abcdef' for c in digest):
        raise ValueError('Invalid UI receipt digest')
    path=directory/'receipts'/(digest+'.json')
    with os.fdopen(os.open(path,os.O_RDONLY|os.O_NOFOLLOW),'rb') as stream:
        raw=stream.read(1024*1024+1)
    if len(raw)>1024*1024 or hashlib.sha256(raw).hexdigest()!=digest:
        raise RuntimeError('UI rejection receipt bytes differ')
    receipt=json.loads(raw);ui=UIKitUIRequest(**binding['request'])
    if (set(receipt)!= {'request','generation','pid','port','last_id','status','result','replayed'}
            or type(receipt['replayed']) is not bool
            or type(receipt['port']) is not int or not 0<receipt['port']<65536):
        raise RuntimeError('Invalid UI rejection envelope')
    validate_reply(receipt,ui)
    result=receipt['result'];endpoint=result['app']
    if (receipt.get('pid')!=binding['identity']['pid'] or result.get('input')!='not_applied'
            or endpoint['generation']!=app['generation'] or endpoint['pid']!=app['pid']
            or state.get('generation')!=ui.generation or state.get('pending') is not None
            or state.get('next_id')!=ui.request_id+1 or state.get('last_reply')!=receipt
            or (directory/'pending.json').exists()):
        raise RuntimeError('No committed no-effect proof for the reserved UI request')
