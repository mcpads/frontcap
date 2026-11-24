"""Describe recorded timer registrations; this is not an inventory of live/pending timers."""
import argparse
from fractions import Fraction
import json
from pathlib import Path
import re

LEGACY = re.compile(r"timer=(mach(?:-leeway)?) deadline=(\d+) virtual_ns=(\d+)$")
FRAME = re.compile(r"(\d+)\s+(\S+)\s+(0x[0-9a-fA-F]+)\s+(.*)$")


def describe(text):
    records = []
    current = None
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        legacy = LEGACY.fullmatch(line)
        if line.startswith('{') or legacy:
            if current is not None and not current['frames']:
                raise ValueError(f"line {number}: previous registration has no stack")
            if legacy:
                kind, deadline, instant = legacy.groups()
                current = dict(timer=kind, deadline_ticks=int(deadline), now_ns=int(instant),
                               basis='legacy_without_origin', frames=[])
            else:
                data = json.loads(line)
                if data.get('timer') not in ('mach', 'mach-leeway'):
                    raise ValueError(f"line {number}: unknown timer kind")
                for key in ('port', 'deadline_ticks', 'base_ticks', 'now_ns', 'numer', 'denom'):
                    if type(data.get(key)) is not int or data[key] < 0:
                        raise ValueError(f"line {number}: invalid {key}")
                if not data['numer'] or not data['denom'] or type(data.get('manual')) is not bool:
                    raise ValueError(f"line {number}: invalid clock basis")
                current = {**data, 'basis': 'manual' if data['manual'] else 'not_manual', 'frames': []}
            records.append(current)
        else:
            frame = FRAME.fullmatch(line.strip())
            if current is None or not frame:
                raise ValueError(f"line {number}: unrecognized trace content")
            index, module, address, symbol = frame.groups()
            if int(index) != len(current['frames']):
                raise ValueError(f"line {number}: nonsequential stack (truncated or interleaved)")
            current['frames'].append(dict(module=module, address=address, symbol=symbol))
    if current is not None and not current['frames']:
        raise ValueError('final registration has no stack')
    for record in records:
        record['caller'] = next((f for f in record['frames'] if f['module'] != 'vclock.dylib'), None)
        record['remaining_ns'] = None
        record['relation'] = 'unknown'
        if record['basis'] == 'manual':
            remaining = (Fraction((record['deadline_ticks'] - record['base_ticks']) * record['numer'],
                                  record['denom']) - record['now_ns'])
            # Keep exact arithmetic: Mach ticks need not represent whole nanoseconds.
            record['remaining_ns'] = dict(numerator=remaining.numerator, denominator=remaining.denominator)
            record['relation'] = 'future' if remaining > 0 else 'due'
    return dict(scope='recorded registrations only; cancellations, delivery and pending state are not inferred',
                registrations=records)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('trace', type=Path)
    args = parser.parse_args()
    try:
        report = describe(args.trace.read_text())
    except (OSError, ValueError) as error:
        parser.exit(1, f'{error}\n')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
