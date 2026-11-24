"""Read committed Flutter request entries for probe/oracle comparison, without repairing evidence.

The Rust request store owns admission and semantic validation. This helper only projects its
committed disk records; it grants no execution or recovery authority.
"""
import json


def read_request_entries(directory):
    head = json.loads((directory/'requests.json').read_text())
    if 'event_count' not in head:
        return head['entries']
    count = head['event_count']
    if type(count) is not int or count < 0:
        raise ValueError('Invalid Flutter history head')
    entries = []
    for number in range(count):
        event = json.loads((directory/'events'/f'{number}.json').read_text())
        if event['event'] == 'request':
            index, entry = event['index'], event['entry']
            if type(index) is not int or not 0 <= index <= len(entries):
                raise ValueError('Invalid Flutter request position')
            if index == len(entries):
                entries.append(entry)
            else:
                entries[index] = entry
        elif event['event'] not in ('external', 'system'):
            raise ValueError('Unknown Flutter history event')
    return entries
