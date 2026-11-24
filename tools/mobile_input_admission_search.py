"""Installed multi-region search rejection using the original observed surface."""

from mobile_observed_search import find_target, SearchIncomplete


def check_ambiguous_regions(surface, report, persist, *, direction):
    regions = [n for n in surface['nodes'] if n.get('scroll_position') is not None]
    assert len(regions) == 2 and all(direction in n['scroll_actions'] for n in regions), regions
    report['ambiguous_regions'] = {}

    def unexpected_call(*args):
        raise AssertionError('Ambiguous scroll region dispatched an operation')

    try:
        find_target(unexpected_call, surface, dict(label='Absent search target'),
                    direction, 1, 1000, 'ambiguous-region', report['ambiguous_regions'], persist)
    except SearchIncomplete as error:
        assert error.outcome == 'ambiguous_region' and error.surface == surface
    else:
        raise AssertionError('Search selected an ambiguous scroll region')
    assert report['ambiguous_regions']['steps'] == []


def check_unchanged_regions(before, after):
    def positions(surface):
        return [n['scroll_position'] for n in surface['nodes'] if n.get('scroll_position') is not None]
    assert len(positions(before)) == 2 and positions(before) == positions(after)
