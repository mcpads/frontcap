"""Data-selected validation and cancel checks over the existing UIKit MCP surface."""
import hashlib


def validate(config):
    for key in ('advance_ms', 'focus_advance_ms'):
        if type(config.get(key)) is not int or not 0 < config[key] <= 60000:
            raise ValueError('Expected an explicit navigation/focus advance budget')
    if not isinstance(config.get('draft_text'), str) or not config['draft_text']:
        raise ValueError('Expected a nonempty discarded draft')


def run(config, detail, expected_detail, stored, saved_bytes, call, observe, target):
    validate(config)
    call('tap_target', 'edit-saved', **target(detail, config['edit_target']))
    call('advance', 'edit-navigation', by_ms=config['advance_ms'])
    editor = observe('edit-opened')
    call('press_target', 'edit-title-focus', **target(editor, config['title_target']),
         advance_ms=config['focus_advance_ms'])
    focused = observe('edit-title-focused')
    state = focused['catalog']['text_input']
    assert state['state'] == 'focused' and state['text'] == config['saved_title']
    length = len(state['text'].encode('utf-16-le')) // 2
    call('replace_text', 'clear-edit-title', **target(focused, config['title_target']),
         start=0, end=length, text='')
    empty = observe('edit-empty')
    assert empty['catalog']['text_input']['text'] == ''
    call('tap_target', 'invalid-edit-save', **target(empty, config['save_target']))
    invalid = observe('edit-validation')
    assert any(all(row.get(k) == v for k, v in config['error_expected'].items())
               and row['intersects_screen'] for row in invalid['catalog']['targets']), 'Validation error missing'
    assert stored.read_bytes() == saved_bytes, 'Invalid submission changed persisted data'
    focus = invalid['catalog']['text_input']
    refocused = invalid
    if focus['state'] != 'focused' or not all(focus.get(k) == v for k, v in config['title_target'].items()):
        call('press_target', 'refocus-invalid-title', **target(invalid, config['title_target']),
             advance_ms=config['focus_advance_ms'])
        refocused = observe('edit-refocused')
    assert refocused['catalog']['text_input']['text'] == ''
    call('replace_text', 'discarded-title', **target(refocused, config['title_target']),
         start=0, end=0, text=config['draft_text'])
    draft = observe('edit-draft')
    assert draft['catalog']['text_input']['text'] == config['draft_text']
    assert stored.read_bytes() == saved_bytes, 'Draft changed persisted data before submission'
    call('tap_target', 'cancel-edit', **target(draft, config['cancel_target']))
    call('advance', 'cancel-navigation', by_ms=config['advance_ms'])
    returned = observe('cancelled-detail')
    for expected in expected_detail:
        assert any(all(row.get(k) == v for k, v in expected.items()) and row['intersects_screen']
                   for row in returned['catalog']['targets']), 'Cancel did not restore saved detail'
    assert stored.read_bytes() == saved_bytes, 'Cancel changed persisted data'
    # Reopen through the ordinary control to verify that the editor also discarded its draft.
    call('tap_target', 'reopen-after-cancel', **target(returned, config['edit_target']))
    call('advance', 'reopen-navigation', by_ms=config['advance_ms'])
    reopened = observe('cancel-reopened')
    assert any(all(row.get(k) == v for k, v in config['title_target'].items())
               and row['value'] == config['saved_title'] for row in reopened['catalog']['targets'])
    assert stored.read_bytes() == saved_bytes
    return dict(now_ns=reopened['now_ns'], storage_sha256=hashlib.sha256(saved_bytes).hexdigest(),
                validation=True, cancelled=True, reopened=True)
