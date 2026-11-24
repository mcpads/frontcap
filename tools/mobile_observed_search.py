"""Bounded search in one observed mobile scroll region; no app labels or hidden settling."""


class SearchIncomplete(RuntimeError):
    def __init__(self, outcome, surface):
        super().__init__('Observed search: '+outcome)
        self.outcome, self.surface = outcome, surface


def matches(node, selector):
    return all(value in (node.get('label'), node.get('tooltip')) if key == 'label_or_tooltip'
               else value in node.get('label', '') if key == 'label_contains'
               else node.get(key) == value for key, value in selector.items())


def outside_bounds(region):
    values = [region.get(key) for key in ('scroll_position', 'scroll_min', 'scroll_max')]
    if all(type(value) in (int, float) for value in values):
        values = [[value] for value in values]
    if not all(isinstance(value, list) for value in values) or len({len(value) for value in values}) != 1:
        return False
    return any(low <= high and (position < low or position > high)
               for position, low, high in zip(*values))


def find_target(call, surface, selector, direction, budget, advance_us, identity, report, persist,
                *, max_boundary_advances=0, boundary_advance_us=0):
    report.update(selector=selector, direction=direction, max_scrolls=budget,
                  advance_us=advance_us, steps=[])
    if max_boundary_advances:
        report.update(max_boundary_advances=max_boundary_advances, boundary_advance_us=boundary_advance_us)
    boundary_advances = 0
    for step in range(budget+1):
        while max_boundary_advances:
            regions = [n for n in surface['nodes'] if n.get('scroll_position') is not None]
            if len(regions) != 1 or not outside_bounds(regions[0]):
                break
            report.setdefault('boundary_wait', []).append(dict(observation=surface['observation'],
                position=regions[0]['scroll_position'], minimum=regions[0]['scroll_min'],
                maximum=regions[0]['scroll_max'], advances=boundary_advances))
            persist()
            if boundary_advances == max_boundary_advances:
                report['outcome'] = 'action_unavailable'
                persist()
                raise SearchIncomplete('action_unavailable', surface)
            prefix = identity+'-boundary-'+str(boundary_advances)
            call(prefix+'-time', dict(operation='advance', delta_us=boundary_advance_us))
            surface = call(prefix+'-observe', dict(operation='observe'))[0]['surface']
            boundary_advances += 1
        found = [node for node in surface['nodes'] if matches(node, selector)]
        if len(found) == 1:
            report['outcome'] = 'found'
            persist()
            return surface
        if len(found) > 1:
            report['outcome'] = 'ambiguous'
            persist()
            raise SearchIncomplete('ambiguous', surface)
        regions = [n for n in surface['nodes'] if n.get('scroll_position') is not None]
        if len(regions) != 1:
            report['outcome'] = 'ambiguous_region'
            persist()
            raise SearchIncomplete('ambiguous_region', surface)
        region = regions[0]
        report['steps'].append(dict(observation=surface['observation'], target=region['id'],
            position=region['scroll_position'], minimum=region.get('scroll_min'), maximum=region.get('scroll_max')))
        outcome = None
        if direction not in region['scroll_actions']:
            limits = [region.get('scroll_min'), region.get('scroll_max')]
            at_edge = False if 'scroll_edges' in region else any(
                v is not None and abs(region['scroll_position']-v) < 1e-6 for v in limits)
            if 'scroll_edges' in region:
                at_edge = region['scroll_edges'].get(direction) is True
            else:
                # A scalar Flutter extent does not identify the axis. Require the reverse
                # semantic action, not an unrelated available direction, to establish an edge.
                opposite = dict(up='down', down='up', left='right', right='left')[direction]
                at_edge = at_edge and opposite in region['scroll_actions']
            outcome = 'end_reached' if at_edge else 'action_unavailable'
        elif step == budget:
            outcome = 'budget_exhausted'
        if outcome:
            report['outcome'] = outcome
            persist()
            raise SearchIncomplete(outcome, surface)
        persist()
        operation = dict(operation='scroll', observation=surface['observation'],
                         target=region['id'], direction=direction)
        request_id = identity+'-scroll-'+str(step)
        applied = call(request_id, operation)[0]
        assert call(request_id, operation)[0] == applied
        if applied.get('not_applied'):
            surface = call(identity+'-changed-'+str(step), dict(operation='observe'))[0]['surface']
            continue
        call(identity+'-time-'+str(step), dict(operation='advance', delta_us=advance_us))
        surface = call(identity+'-observe-'+str(step), dict(operation='observe'))[0]['surface']
