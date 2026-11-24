"""Bounded SpringBoard alert evidence; no app-clock or app-foreground claim."""
from native_ios_generic_ui import exact, canonical, text

REASONS = {'stale_observation', 'unknown_target', 'ambiguous_target',
           'disabled_target', 'unhittable_target', 'observation_changed'}


def validate_catalog(catalog):
    exact(catalog, ('observation', 'bundle', 'stable', 'alerts'))
    if (not canonical(catalog['observation']) or catalog['bundle'] != 'com.apple.springboard'
            or type(catalog['stable']) is not bool or type(catalog['alerts']) is not list
            or len(catalog['alerts']) > 8):
        raise ValueError('Invalid system alert catalog')
    units = 0
    def bounded(value):
        nonlocal units
        if not text(value, 1024): raise ValueError('System alert text exceeds budget')
        units += len(value.encode('utf-16-le')) // 2
        if units > 65536: raise ValueError('System alert catalog exceeds text budget')
    def frame(value):
        if (type(value) is not list or len(value) != 4
                or any(type(n) is not int or not -(2**31-1) <= n <= 2**31-1 for n in value)
                or value[2] < 0 or value[3] < 0):
            raise ValueError('Invalid system alert frame')
    index = 0
    for alert in catalog['alerts']:
        exact(alert, ('label', 'texts', 'frame_mpt', 'buttons'))
        bounded(alert['label']); frame(alert['frame_mpt'])
        if (type(alert['texts']) is not list or len(alert['texts']) > 16
                or type(alert['buttons']) is not list or len(alert['buttons']) > 16):
            raise ValueError('System alert inventory exceeds budget')
        for value in alert['texts']: bounded(value)
        for button in alert['buttons']:
            exact(button, ('reference', 'identifier', 'label', 'frame_mpt', 'unique', 'enabled', 'hittable'))
            bounded(button['identifier']); bounded(button['label']); frame(button['frame_mpt'])
            unique = sum(all(other[k] == button[k] for k in ('identifier', 'label', 'frame_mpt'))
                         for other_alert in catalog['alerts'] for other in other_alert['buttons']) == 1
            if (button['reference'] != catalog['observation']+'-'+str(index)
                    or any(type(button[k]) is not bool for k in ('unique', 'enabled', 'hittable'))
                    or button['unique'] != unique or button['hittable'] and not (
                        button['unique'] and button['enabled'] and button['frame_mpt'][2] and button['frame_mpt'][3])):
                raise ValueError('Invalid system alert button')
            index += 1
    return catalog


def target(catalog, observation, reference):
    validate_catalog(catalog)
    if catalog['stable'] and catalog['observation'] == observation:
        for alert in catalog['alerts']:
            for button in alert['buttons']:
                if button['reference'] == reference and all(button[k] for k in ('unique', 'enabled', 'hittable')):
                    return button
    raise ValueError('System input requires the latest stable alert observation')


def validate_result(result, request):
    if request.operation == 'observe_system_alerts':
        exact(result, ('state', 'catalog', 'artifacts'))
        if result['state'] != 'system_observed': raise ValueError('Unconfirmed system observation')
        catalog = validate_catalog(result['catalog'])
        artifacts = result['artifacts']
        if type(artifacts) is not list or len(artifacts) != 2: raise ValueError('Invalid system capture inventory')
        for item, name in zip(artifacts, ('screen.png', 'hierarchy.txt')):
            exact(item, ('path', 'bytes', 'sha256'))
            if (item['path'] != 'observations/'+catalog['observation']+'/'+name
                    or type(item['bytes']) is not int or not 0 < item['bytes'] <= 16*1024*1024
                    or type(item['sha256']) is not str or len(item['sha256']) != 64
                    or any(c not in '0123456789abcdef' for c in item['sha256'])):
                raise ValueError('Invalid system capture artifact')
    else:
        fields = ('input', 'reference', 'observation')
        if result.get('input') == 'not_applied':
            exact(result, fields+('reason',))
            if result['reason'] not in REASONS: raise ValueError('Invalid system input rejection')
        else:
            exact(result, fields)
            if result['input'] != 'xctest_system_tap': raise ValueError('Unconfirmed system input')
        if any(result[k] != request.arguments[k] for k in ('reference', 'observation')):
            raise ValueError('System input belongs to another observation')
