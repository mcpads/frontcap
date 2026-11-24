"""Account for trace thread states inside identity-bound accepted-registration spans.

Use only exports from one verified trace. This measures the accepted body, not receiver entry,
host latency or a cause of non-running time. A trace loss audit remains a separate prerequisite.
"""
import argparse
import csv
import hashlib
import json
from pathlib import Path


def number(value):
    if type(value) is int:
        return value
    if isinstance(value, str) and value.lstrip('-').isdigit():
        return int(value)
    raise ValueError('Invalid trace integer')


def account(span, states):
    start, duration, utid = (number(span[k]) for k in ('ts', 'dur', 'utid'))
    if start < 0 or utid <= 0:
        raise ValueError('Invalid trace span')
    if duration < 0:
        return {'status': 'unclosed_span'}
    end = start + duration
    intervals = []
    for row in states:
        if number(row['utid']) != utid:
            continue
        ts, dur = number(row['ts']), number(row['dur'])
        if ts < 0:
            raise ValueError('Invalid thread-state timestamp')
        if dur < 0:
            if ts < end:
                return {'status': 'unclosed_thread_state'}
            continue
        lo, hi = max(start, ts), min(end, ts + dur)
        if lo < hi:
            state = row['state']
            if not isinstance(state, str) or not state or state == '[NULL]':
                return {'status': 'unknown_thread_state'}
            intervals.append((lo, hi, state))
    intervals.sort()
    cursor = start
    totals = {}
    gaps = []
    for lo, hi, state in intervals:
        if lo < cursor:
            return {'status': 'overlapping_thread_states'}
        if lo > cursor:
            gaps.append([cursor, lo])
        totals[state] = totals.get(state, 0) + hi - lo
        cursor = hi
    if cursor < end:
        gaps.append([cursor, end])
    return {'status': 'incomplete_coverage' if gaps else 'complete_coverage',
            'duration_ns': duration, 'states_ns': totals, 'gaps_ns': gaps,
            'covered_ns': sum(totals.values())}


def analyze(owner, events, spans, states):
    result = []
    seen = set()
    for event in events:
        if event.get('phase') != 'registration_accepted':
            continue
        request = event['request']
        if (event['pid'] != owner['identity']['pid'] or
                event['generation'] != owner['identity']['generation'] or
                request['generation'] != owner['identity']['generation'] or
                request['controller_id'] != owner['controller_id']):
            raise ValueError('Foreign accepted request')
        rid = request['request_id']
        if type(rid) is not int or rid <= 0 or rid in seen:
            raise ValueError('Invalid or duplicate accepted request')
        seen.add(rid)
        label = f"FrontcapInput:{request['generation']}:{request['controller_id']}:{rid}"
        matches = [s for s in spans if s['name'] == label]
        row = {'request': request, 'span_candidates': matches}
        if len(matches) != 1:
            row['accounting'] = {'status': 'missing_span' if not matches else 'ambiguous_span'}
        else:
            span = matches[0]
            if number(span['pid']) != owner['identity']['pid'] or number(span['tid']) != owner['identity']['pid']:
                raise ValueError('Request span differs from owner main thread')
            row['accounting'] = account(span, states)
        result.append(row)
    if not result:
        raise ValueError('No accepted requests')
    return {'requests': result, 'limits': [
        'accepted-body spans exclude request validation and pre-receiver delay',
        'thread states do not identify a waiting resource or a causal mechanism',
        'trace Running time is not interchangeable with app thread-CPU accounting',
        'complete state coverage does not prove loss-free capture or successful input',
        'all exports must come from the same verified trace; no host clock alignment is inferred']}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('owner', 'receiver_log', 'spans', 'states', 'output'):
        parser.add_argument(name, type=Path)
    args = parser.parse_args()
    paths = [args.owner, args.receiver_log, args.spans, args.states]
    data = [p.read_bytes() for p in paths]
    events = [json.loads(line[line.index('{'):]) for line in data[1].decode().splitlines()
              if 'FrontcapInput' in line and '{' in line]
    result = analyze(json.loads(data[0]), events, list(csv.DictReader(data[2].decode().splitlines())),
                     list(csv.DictReader(data[3].decode().splitlines())))
    result['inputs'] = {str(p): {'bytes': len(b), 'sha256': hashlib.sha256(b).hexdigest()}
                        for p, b in zip(paths, data)}
    result['analysis_sha256'] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    with args.output.open('x') as output:
        output.write(json.dumps(result, indent=2) + '\n')


if __name__ == '__main__':
    main()
