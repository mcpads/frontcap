"""Typed generic UIKit request/catalog validation for the existing durable UI client."""
import math
import re
import unicodedata
import uuid
from native_ios_ui import UIRequest
from native_clock_control import Request as ClockRequest
from native_ios_control import validate_reply as validate_clock_reply

ROLES = {'table':26,'collection_view':32,'scroll_view':46,'button': 9, 'text_field': 49, 'secure_text_field': 50, 'text_view': 52,
         'cell': 75, 'link': 42, 'switch': 40, 'static_text': 48}
REASONS = {'stale_observation', 'unknown_target', 'ambiguous_target', 'disabled_target',
           'unhittable_target', 'observation_changed', 'unsupported_target', 'transition_in_progress', 'invalid_press_context', 'input_unavailable', 'scroll_unavailable'}


def exact(value, keys):
    if type(value) is not dict or set(value) != set(keys):
        raise ValueError('Invalid UIKit fields')


def canonical(value):
    return type(value) is str and str(uuid.UUID(value)) == value


def valid_reference(reference, observation):
    if not canonical(observation) or type(reference) is not str:
        return False
    prefix = observation+'-'
    index = reference[len(prefix):] if reference.startswith(prefix) else ''
    return index.isascii() and index.isdigit() and 0 <= int(index) < 2048 and str(int(index)) == index


class UIKitUIRequest(UIRequest):
    def __post_init__(self):
        UIRequest(self.generation, self.request_id, 'finish', {})
        if self.operation == 'launch':
            if type(self.arguments) is not dict or set(self.arguments) - {'environment'}:
                raise ValueError('Invalid launch arguments')
            environment = self.arguments.get('environment', {})
            if (type(environment) is not dict or len(environment) > 16
                    or any(type(k) is not str or re.fullmatch('[A-Z][A-Z0-9_]{0,63}', k) is None
                        or k.startswith(('FC_', 'DYLD_', 'SIMCTL_')) or type(v) is not str
                        or len(v.encode('utf-8')) > 2048 or any(unicodedata.category(c) == 'Cc' for c in v)
                        for k, v in environment.items())):
                raise ValueError('Invalid app launch environment')
        elif self.operation == 'set_host_app_state':
            from native_ios_host_state import validate_arguments
            validate_arguments(self.arguments)
        elif self.operation == 'set_app_state':
            from native_ios_restart import identity
            exact(self.arguments, ('app','state'));identity(self.arguments['app'])
            if self.arguments['state'] not in ('background','foreground'):raise ValueError('Invalid UIKit host state')
        elif self.operation == 'restart':
            from native_ios_restart import identity, validate_url
            exact(self.arguments, ('app','url') if 'url' in self.arguments else ('app',))
            if 'url' in self.arguments: validate_url(self.arguments['url'])
            identity(self.arguments['app'])
        elif self.operation in ('observe_system_alerts', 'observe_targets', 'terminate', 'finish'):
            exact(self.arguments, ())
        elif self.operation in ('tap_system_button','tap_target','press_target','replace_text','scroll_target'):
            fields=('reference','observation')
            if self.operation=='scroll_target':fields+=('direction',)
            if self.operation=='press_target':fields+=('clock_request',)
            if self.operation=='replace_text':fields+=('start','end','text')
            exact(self.arguments,fields)
            if self.operation=='scroll_target' and self.arguments['direction'] not in ('up','down','left','right'):raise ValueError('Invalid scroll direction')
            if self.operation=='replace_text':
                from native_ios_edit import validate_arguments
                validate_arguments(self.arguments['start'],self.arguments['end'],self.arguments['text'])
            if self.operation=='press_target':
                exact(self.arguments['clock_request'],('generation','request_id','operation','delta_ns'))
                if ClockRequest(**self.arguments['clock_request']).operation!='advance':
                    raise ValueError('Press requires an exact clock advance')
            if not valid_reference(self.arguments['reference'], self.arguments['observation']):
                raise ValueError('Invalid UIKit target reference')
        else:
            raise ValueError('Unsupported generic UIKit operation')


def text(value, maximum):
    return type(value) is str and len(value.encode('utf-16-le'))//2 <= maximum


def validate_text_input(value):
    if type(value) is not dict:
        raise ValueError('Invalid UIKit text state')
    state = value.get('state')
    if state == 'unfocused':
        exact(value, ('state',))
    elif state in ('unsupported', 'unavailable'):
        exact(value, ('state', 'reason'))
        reasons = ('input_type', 'secure_text', 'read_only') if state == 'unsupported' else (
            'wrong_thread', 'view_budget', 'text_budget', 'selector_budget', 'selection_missing', 'selection_invalid')
        if value['reason'] not in reasons:
            raise ValueError('Unknown UIKit text state reason')
    elif state == 'focused':
        exact(value, ('state','token','role','identifier','label','text','start','end','marked'))
        if (not canonical(value['token']) or value['role'] not in ('text_field','text_view')
                or not text(value['identifier'],4096) or not text(value['label'],4096)
                or not text(value['text'],16384) or type(value['start']) is not int
                or type(value['end']) is not int or type(value['marked']) is not bool
                or not 0 <= value['start'] <= value['end'] <= len(value['text'].encode('utf-16-le'))//2):
            raise ValueError('Invalid UIKit focused input')
    else:
        raise ValueError('Unknown UIKit text state')


def validate_catalog(catalog):
    exact(catalog, ('observation', 'keyboard', 'targets', 'text_input', 'transitioning', 'scrolls'))
    validate_text_input(catalog['text_input'])
    from native_ios_scroll import validate_scroll
    if type(catalog['scrolls']) is not list or len(catalog['scrolls'])>2048:raise ValueError('Invalid scroll catalog')
    tokens=[validate_scroll(s)['token'] for s in catalog['scrolls']]
    if len(set(tokens))!=len(tokens):raise ValueError('Duplicate scroll token')
    if (not canonical(catalog['observation']) or type(catalog['keyboard']) is not bool
            or type(catalog['transitioning']) is not bool or type(catalog['targets']) is not list or len(catalog['targets']) > 2048):
        raise ValueError('Invalid UIKit catalog')
    fields = ('role', 'type', 'identifier', 'label', 'value', 'selector_key', 'selector_value',
              'unique', 'enabled', 'selected', 'hittable', 'intersects_screen', 'frame_mpt', 'reference')
    for index, target in enumerate(catalog['targets']):
        exact(target, fields)
        if (type(target['role']) is not str or type(target['type']) is not int
                or ROLES.get(target['role']) != target['type']
                or not text(target['identifier'],4096) or not text(target['label'],4096)
                or target['reference'] != catalog['observation']+'-'+str(index)
                or any(type(target[k]) is not bool for k in ('unique','enabled','selected','hittable','intersects_screen'))):
            raise ValueError('Invalid UIKit target')
        value=target['value']
        if value is not None and not (text(value,16384) or type(value) is bool or
                type(value) in (int,float) and math.isfinite(value)):
            raise ValueError('Invalid UIKit value')
        frame=target['frame_mpt']
        if (type(frame) is not list or len(frame)!=4
                or any(type(n) is not int or not -(2**31-1)<=n<=2**31-1 for n in frame)
                or frame[2]<0 or frame[3]<0):
            raise ValueError('Invalid UIKit frame')
    for target in catalog['targets']:
        key='identifier' if target['identifier'] else 'label'
        selected=target[key]
        unique=bool(selected) and sum(other['type']==target['type'] and other[key]==selected
                                     for other in catalog['targets'])==1
        if (target['selector_key']!=key or target['selector_value']!=selected or target['unique']!=unique
                or target['role']=='static_text' and target['hittable']
                or target['hittable'] and not (unique and target['enabled'] and target['intersects_screen'])
                or target['intersects_screen'] and (not target['frame_mpt'][2] or not target['frame_mpt'][3])):
            raise ValueError('Inconsistent UIKit selector/readiness')
    return catalog


def validate_press(result, request):
    common=('input','app','reference','observation','clock_request')
    if result.get('input')=='not_applied':
        exact(result,common+('reason',))
        if result['reason'] not in REASONS:raise ValueError('Unknown press rejection')
    else:
        exact(result,common+('before','began','ended','clock_reply'))
        if result['input']!='xctest_timed_press':raise ValueError('Unresolved press')
        clock=result['clock_reply']
        exact(clock,('request','generation','pid','port','last_id','status','before_ns','now_ns',
                     'applied','advance_result','drain_result','replayed'))
        validate_clock_reply(clock,ClockRequest(**request.arguments['clock_request']))
        if (clock['status']!='ok' or clock['replayed'] is not False or type(clock['pid']) is not int
                or clock['pid']!=result['app']['pid'] or clock['port']!=result['app']['port'] or type(clock['port']) is not int or not 0<clock['port']<65536):
            raise ValueError('Press clock ownership/delivery differs')
        for name in ('before','began','ended'):
            row=result[name];exact(row,('generation','serial','now_ns','touches'))
            if (row['generation']!=result['app']['generation'] or type(row['serial']) is not int
                    or not 0<=row['serial']<2**64 or type(row['now_ns']) is not int or not 0<=row['now_ns']<2**63
                    or type(row['touches']) is not list or len(row['touches'])>16):
                raise ValueError('Invalid source input boundary')
            for touch in row['touches']:
                exact(touch,('phase','timestamp','x','y','view','hit','identifier','label','input_identifier','input_label','input_role'))
                if (type(touch['phase']) is not int or not 0<=touch['phase']<=4
                        or touch['input_role'] not in ('none','text_field','text_view')
                        or any(not text(touch[k],4096) for k in ('view','hit','identifier','label','input_identifier','input_label'))
                        or any(type(touch[k]) not in (int,float) or not math.isfinite(touch[k]) for k in ('timestamp','x','y'))):
                    raise ValueError('Invalid source touch')
        before,began,ended=(result[k] for k in ('before','began','ended'))
        if (began['serial']!=before['serial']+1 or ended['serial']<=began['serial']
                or before['now_ns']>began['now_ns'] or began['now_ns']!=clock['before_ns']
                or ended['now_ns']!=clock['now_ns'] or len(began['touches'])!=1 or len(ended['touches'])!=1):
            raise ValueError('Unconfirmed press ordering')
        begin,end=began['touches'][0],ended['touches'][0]
        if (begin['phase']!=0 or end['phase']!=3 or begin['input_role']=='none'
                or any(begin[k]!=end[k] for k in ('input_role','input_identifier','input_label'))
                or not (begin['input_identifier'] or begin['input_label'])):
            raise ValueError('Press ended on another input')
    if (result['clock_request']!=request.arguments['clock_request']
            or result['clock_request']['generation']!=result['app']['generation']
            or any(result[k]!=request.arguments[k] for k in ('reference','observation'))):
        raise ValueError('Press belongs to another request')


def validate_generic_result(result, request):
    if result.get('state') == 'unresolved' or result.get('input') == 'unresolved':
        reason = result.get('error')
        if type(reason) is str and re.fullmatch('[a-z][a-z0-9_]{0,127}', reason):
            raise RuntimeError('Unresolved UIKit outcome: '+reason)
        raise RuntimeError('Unresolved UIKit outcome; keep the pending request')
    if request.operation == 'set_host_app_state':
        from native_ios_host_state import validate_result
        validate_result(result, request)
        return
    if request.operation == 'set_app_state':
        from native_ios_app_state import validate_result
        validate_result(result, request)
        return
    if request.operation in ('observe_system_alerts', 'tap_system_button'):
        from native_ios_system_ui import validate_result
        validate_result(result, request)
        return
    if request.operation == 'observe_targets':
        unavailable=result.get('state')=='unavailable'
        exact(result, ('state','app','now_ns','reason' if unavailable else 'catalog','artifacts'))
        if type(result['now_ns']) is not int or not 0<=result['now_ns']<2**63:
            raise ValueError('Invalid UIKit observation result')
        artifacts=result['artifacts']
        if unavailable:
            if result['reason'] not in ('scroll_in_motion','capture_changed'):raise ValueError('Unknown unavailable observation')
            expected=0 if result['reason']=='scroll_in_motion' else 2
            if type(artifacts) is not list or len(artifacts)!=expected:raise ValueError('Invalid unavailable inventory')
            capture=artifacts[0]['path'].split('/')[1] if artifacts else None
            if capture is not None and not canonical(capture):raise ValueError('Invalid capture identity')
        else:
            if result['state']!='observed':raise ValueError('Invalid observation state')
            validate_catalog(result['catalog']);capture=result['catalog']['observation']
            if type(artifacts) is not list or len(artifacts)!=2:raise ValueError('Invalid UIKit capture inventory')
        for item,name in zip(artifacts,('screen.png','hierarchy.txt')):
            exact(item,('path','bytes','sha256'))
            if (item['path']!='observations/'+capture+'/'+name
                    or type(item['bytes']) is not int or not 0<item['bytes']<=16*1024*1024
                    or type(item['sha256']) is not str or len(item['sha256'])!=64
                    or any(c not in '0123456789abcdef' for c in item['sha256'])):
                raise ValueError('Invalid UIKit capture metadata')
    elif request.operation=='scroll_target':
        from native_ios_scroll import validate_result
        validate_result(result,request)
    elif request.operation=='replace_text':
        from native_ios_edit import validate_result
        validate_result(result,request)
    elif request.operation=='press_target':
        validate_press(result,request)
    else:
        fields=('input','app','reference','observation')
        if result.get('input')=='not_applied':
            exact(result,fields+('reason',))
            if result['reason'] not in REASONS:raise ValueError('Unknown UIKit rejection')
        else:
            exact(result,fields)
            if result['input']!='xctest_tap':raise ValueError('Unknown UIKit input outcome')
        if any(result[k]!=request.arguments[k] for k in ('reference','observation')):
            raise ValueError('UIKit outcome belongs to another target')
    endpoint=result['app']
    exact(endpoint,('generation','pid','port'))
    if (not canonical(endpoint['generation']) or type(endpoint['pid']) is not int
            or not 0<endpoint['pid']<2**31 or type(endpoint['port']) is not int or not 0<endpoint['port']<65536):
        raise ValueError('Invalid UIKit app endpoint')
