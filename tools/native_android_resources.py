"""Check requested emulator resources against retained guest observations."""
import json
from pathlib import Path
import re


def check_cpu_count(directory, requested):
    root = Path(directory)
    detail = dict(requested=requested, observation='guest-startup-cpu-online.txt')
    try:
        if type(requested) is not int or requested <= 0:
            raise ValueError('Expected a positive requested CPU count')
        report = json.loads((root/'guest-startup-diagnostics.json').read_text())
        records = report['commands']
        if not isinstance(records, list) or any(not isinstance(r, dict) for r in records):
            raise ValueError('Malformed startup command observations')
        matches = [r for r in records if isinstance(r.get('command'), list)
                   and r['command'][-3:] == ['shell', 'cat', '/sys/devices/system/cpu/online']]
        if len(matches) != 1 or type(matches[0].get('exit_code')) is not int or matches[0]['exit_code'] != 0 or matches[0].get('error'):
            raise ValueError('CPU observation did not complete successfully exactly once')
        online = (root/'guest-startup-cpu-online.txt').read_text().strip()
        detail['online'] = online
        if not re.fullmatch(r'[0-9]+(?:-[0-9]+)?(?:,[0-9]+(?:-[0-9]+)?)*', online):
            raise ValueError('Malformed online CPU list')
        count, previous = 0, -1
        for item in online.split(','):
            bounds = [int(value) for value in item.split('-')]
            first, last = bounds[0], bounds[-1]
            if first <= previous or last < first:
                raise ValueError('Overlapping, unordered or reversed CPU ranges')
            count += last-first+1
            previous = last
        detail['observed'] = count
        return dict(name='emulator.cpu_count', passed=count == requested, detail=detail)
    except (OSError, ValueError, KeyError, TypeError) as error:
        detail['error'] = f'{type(error).__name__}: {error}'
        return dict(name='emulator.cpu_count', passed=False, detail=detail)
