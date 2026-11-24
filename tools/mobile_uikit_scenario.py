"""UIKit projection for the shared scenario executor; source identities remain authoritative."""
import base64
import hashlib
from native_ios_scroll import allows, DIRECTIONS


def surface(value, observed):
    catalog=value['catalog'];focus=catalog['text_input'];nodes=[]
    for row in catalog['targets']:
        ready=row['unique'] and row['enabled'] and row['hittable'] and not catalog['transitioning']
        focused=(focus['state']=='focused' and row['unique'] and row['role']==focus['role']
            and row['selector_value']==focus[row['selector_key']])
        node=dict(id=row['reference'],identifier=row['identifier'],label=row['label'],role=row['role'],
            value=focus['text'] if focused else row['value'],focused=focused,
            editable=focused and not focus['marked'] and ready,tappable=ready)
        matches=[v for v in catalog['scrolls'] if v['role']==row['role']
            and v[row['selector_key']]==row['selector_value']]
        if len(matches)==1 and row['unique']:
            scroll=matches[0]
            node.update(scroll_position=scroll['offset_mpt'],scroll_min=scroll['minimum_mpt'],
                scroll_max=scroll['maximum_mpt'],scroll_actions=[d for d in DIRECTIONS if ready and allows(scroll,d)],
                scroll_edges={})
            stable=(ready and scroll['enabled'] and not scroll['dragging'] and not scroll['decelerating']
                and all(v>0 for v in scroll['viewport_mpt'])
                and all(scroll['minimum_mpt'][i]<=scroll['offset_mpt'][i]<=scroll['maximum_mpt'][i] for i in (0,1)))
            if stable:
                for axis,low,high in ((0,'left','right'),(1,'up','down')):
                    if scroll['minimum_mpt'][axis]<scroll['maximum_mpt'][axis]:
                        node['scroll_edges'][low]=scroll['offset_mpt'][axis]==scroll['minimum_mpt'][axis]
                        node['scroll_edges'][high]=scroll['offset_mpt'][axis]==scroll['maximum_mpt'][axis]
        nodes.append(node)
    return dict(generation=value['app']['generation'],observation=observed,
        source_observation=catalog['observation'],now_ns=value['now_ns'],ime_visible=catalog['keyboard'],nodes=nodes)


def call(backend, identity, operation):
    kind=operation['operation']
    def completion(reply, expected):
        outcome=reply['structuredContent']['result']['body']['outcome']
        if outcome['outcome']!='ios_completed' or outcome['completion']['channel']!=expected:
            raise RuntimeError('Expected terminal UIKit completion on requested channel')
        return outcome['completion']['receipt']
    def app(value):
        if value['app']['generation']!=backend.generation:
            raise RuntimeError('UIKit result belongs to another app generation')
    if kind in ('observe','capture'):
        observed=identity if kind=='observe' else operation['observation']
        reply=backend.native(observed,dict(type='observe'))
        value=completion(reply,'ui')['outcome'];app(value)
        if value['outcome']=='observation_unavailable':
            if kind!='observe':raise RuntimeError('Cannot export an unavailable UIKit observation')
            return dict(generation=backend.generation,unavailable=value['reason']),reply
        if value['outcome']!='observed_targets':raise RuntimeError('Expected UIKit target catalog')
        images=[item for item in reply['content'] if item['type']=='image']
        if len(images)!=1 or images[0]['mimeType']!='image/png':raise RuntimeError('Expected one bound UIKit PNG')
        raw=base64.b64decode(images[0]['data'],validate=True)
        path='observations/'+value['catalog']['observation']+'/screen.png'
        artifact=next(item for item in value['artifacts'] if item['path']==path)
        if len(raw)!=artifact['bytes'] or hashlib.sha256(raw).hexdigest()!=artifact['sha256']:
            raise RuntimeError('UIKit inline image differs from recorded capture')
        result=dict(generation=backend.generation,surface=surface(value,observed))
        if kind=='capture':result['capture']=dict(state='complete',surface=result['surface'],
            png_base64=images[0]['data'],source_request_id=observed,kind='uikit-observation-screen')
        return result,reply
    if kind=='advance':
        if operation['delta_us']%1000:raise ValueError('UIKit advance requires whole milliseconds')
        reply=backend.native(identity,dict(type='advance',by_ms=operation['delta_us']//1000))
        receipt=completion(reply,'clock')
        if receipt['generation']!=backend.generation:raise RuntimeError('Clock belongs to another app generation')
        return dict(generation=backend.generation,native_completion=receipt),reply
    names=dict(tap='tap_target',press='press_target',replace_text='replace_text',scroll='scroll_target')
    if kind not in names:raise ValueError('Unsupported UIKit scenario operation')
    # Recover the exact saved catalog instead of retaining an in-memory UUID map across reconnects.
    basis=call(backend,operation['observation'],dict(operation='observe'))[0]['surface']
    if not any(row['id']==operation['target'] for row in basis['nodes']):
        raise RuntimeError('UIKit input reference differs from saved observation')
    arguments=dict(type=names[kind],reference=operation['target'],observation=basis['source_observation'])
    if kind=='replace_text':arguments.update({k:operation[k] for k in ('start','end','text')})
    elif kind=='scroll':arguments['direction']=operation['direction']
    elif kind=='press':
        if operation['advance_us']%1000:raise ValueError('UIKit press requires whole milliseconds')
        arguments['advance_ms']=operation['advance_us']//1000
    reply=backend.native(identity,arguments)
    receipt=completion(reply,'press' if kind=='press' else 'ui')
    outcome=(receipt['ui'] if kind=='press' else receipt)['outcome'];app(outcome)
    result=dict(generation=backend.generation,native_completion=receipt)
    expected=dict(tap='target_tapped',press='target_pressed',replace_text='text_edit',scroll='scrolled')[kind]
    if outcome['outcome'] in ('target_not_applied','target_press_not_applied'):
        if outcome['reason'] not in ('observation_changed','stale_observation','transition_in_progress'):
            raise RuntimeError('UIKit rejected input cannot be refreshed')
        if kind=='press' and receipt['clock']['outcome']!='released':raise RuntimeError('Rejected press clock not released')
        result['not_applied']='observation_changed'
    elif outcome['outcome']!=expected:raise RuntimeError('UIKit input result differs from requested operation')
    elif kind=='press':
        if receipt['clock']['outcome']!='delivered':raise RuntimeError('Press clock delivery missing')
    elif kind=='replace_text':
        delivery=outcome['delivery']
        if delivery['input']=='not_applied':
            if delivery['reason'] not in ('observation_changed','stale_observation'):
                raise RuntimeError('UIKit text rejection cannot be refreshed')
            result['not_applied']='observation_changed'
        elif delivery['input']=='uikit_text_control':
            source=delivery['receipt']['result']
            if source['input']=='not_applied':
                if source['reason']!='input_changed':raise RuntimeError('UIKit source text rejection cannot be refreshed')
                result['not_applied']='observation_changed'
            elif source['input']!='uikit_text_input':raise RuntimeError('Unknown UIKit source text result')
        else:raise RuntimeError('Unknown UIKit text delivery')
    return result,reply
