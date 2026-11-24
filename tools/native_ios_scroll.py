"""Source geometry and one observed XCTest swipe; no implicit motion or search retries."""
from native_ios_generic_ui import exact,canonical,text,REASONS

DIRECTIONS=('up','down','left','right')

def validate_scroll(value):
    exact(value,('token','role','identifier','label','offset_mpt','minimum_mpt','maximum_mpt',
        'viewport_mpt','enabled','dragging','decelerating'))
    if (not canonical(value['token']) or value['role'] not in ('table','collection_view','scroll_view','text_view')
        or not text(value['identifier'],4096) or not text(value['label'],4096)
        or not (value['identifier'] or value['label'])
        or any(type(value[k]) is not bool for k in ('enabled','dragging','decelerating'))):
        raise ValueError('Invalid scroll source')
    for key in ('offset_mpt','minimum_mpt','maximum_mpt','viewport_mpt'):
        v=value[key]
        if type(v) is not list or len(v)!=2 or any(type(n) is not int or not -(2**31-1)<=n<=2**31-1 for n in v):
            raise ValueError('Invalid scroll geometry')
    if any(value['minimum_mpt'][i]>value['maximum_mpt'][i] or value['viewport_mpt'][i]<0 for i in (0,1)):
        raise ValueError('Invalid scroll extent')
    return value

def allows(value,direction):
    if direction not in DIRECTIONS:raise ValueError('Invalid scroll direction')
    if (not value['enabled'] or value['dragging'] or value['decelerating'] or 0 in value['viewport_mpt']
        or any(not value['minimum_mpt'][i]<=value['offset_mpt'][i]<=value['maximum_mpt'][i] for i in (0,1))):return False
    axis=1 if direction in ('up','down') else 0
    return (value['offset_mpt'][axis]<value['maximum_mpt'][axis] if direction in ('down','right')
        else value['offset_mpt'][axis]>value['minimum_mpt'][axis])

def target(catalog,reference,direction):
    rows=[t for t in catalog['targets'] if t['reference']==reference and t['unique'] and t['enabled'] and t['hittable']]
    if len(rows)!=1:raise ValueError('Scroll target is not ready')
    row=rows[0]
    matches=[s for s in catalog['scrolls'] if s['role']==row['role'] and s[row['selector_key']]==row['selector_value']]
    if len(matches)!=1 or catalog['transitioning'] or not allows(matches[0],direction):
        raise ValueError('Scroll direction is unavailable or source is moving')
    return matches[0]

def validate_result(result,request):
    fields=('input','app','reference','observation')
    if any(result.get(k)!=request.arguments[k] for k in ('reference','observation')):raise ValueError('Scroll request differs')
    if result.get('input')=='not_applied':
        exact(result,fields+('reason',))
        if result['reason'] not in REASONS:raise ValueError('Unknown scroll rejection')
        return
    exact(result,fields+('now_ns','direction','before','after'))
    before=validate_scroll(result['before']);after=validate_scroll(result['after'])
    if (result['input']!='xctest_scroll' or result['direction']!=request.arguments['direction']
        or type(result['now_ns']) is not int or not 0<=result['now_ns']<2**63
        or not allows(before,result['direction'])
        or any(before[k]!=after[k] for k in ('token','role','identifier','label'))):
        raise ValueError('Scroll delivery differs')
