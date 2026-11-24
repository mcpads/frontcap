"""Consumer label changes must remain ordinary observed input, without adapter edits."""


def check_label_change(call, surface, report, persist, *, rename_advance_us=0):
    def unique(surface, label):
        nodes = [n for n in surface['nodes'] if n['label'] == label and n['tappable']]
        assert len(nodes) == 1, (label, nodes)
        return nodes[0]

    def tap(identity, surface, node):
        operation = dict(operation='tap', observation=surface['observation'], target=node['id'])
        result = call(identity, operation)
        assert not result.get('not_applied')
        assert call(identity, operation) == result

    tap('rename-control', surface, unique(surface, 'Rename available target'))
    report['rename_advance_us'] = rename_advance_us
    persist()
    if rename_advance_us:
        call('rename-time', dict(operation='advance', delta_us=rename_advance_us))
    renamed = call('after-rename', dict(operation='observe'))['surface']
    report['after_rename'] = renamed; persist()
    assert not any(n['label'] == 'Available target' for n in renamed['nodes'])
    assert any(n['label'] == 'Applied taps: 1' for n in renamed['nodes'])
    tap('renamed-input', renamed, unique(renamed, 'Renamed action ✓'))
    after = call('after-renamed-input', dict(operation='observe'))['surface']
    report['after_renamed_input'] = after; persist()
    assert any(n['label'] == 'Applied taps: 2' for n in after['nodes'])
    assert not any(n['label'] == 'Available target' for n in after['nodes'])
    unique(after, 'Renamed action ✓')
    clock = 'logical_us' if 'logical_us' in surface else 'now_ns'
    expected = rename_advance_us if clock == 'logical_us' else rename_advance_us * 1000
    assert surface[clock] == 0 and renamed[clock] == after[clock] == expected
    return after
