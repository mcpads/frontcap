"""Separate guest timer boundaries from host output acquisition; never change timing verdicts."""
import argparse
import hashlib
import json
from pathlib import Path


PHASES = ['before-arm-output', 'after-arm-output', 'before-registration', 'after-registration',
          'loop-entry', 'callback-entry', 'before-output', 'after-output']
COLUMNS = ['monotonic_before_ns', 'thread_cpu_ns', 'monotonic_after_ns', 'tid']
TIMERS = {'Handler': 'FIRE Handler.postDelayed', 'Timer': 'FIRE java.util.Timer',
          'ValueAnimator': 'FIRE ValueAnimator end;'}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def analyze(rows):
    lines = [row for row in rows if row.get('state') == 'line']
    ends = [row for row in rows if row.get('state') == 'finished']
    require(len(ends) == 1 and ends[0].get('exit_code') == 0
            and ends[0].get('reader_alive') is False and not ends[0].get('reader_error')
            and not ends[0].get('cleanup_error')
            and ends[0].get('received_lines') == ends[0].get('forwarded_lines') == len(lines),
            'Incomplete reader lifetime')
    require([row['sequence'] for row in lines] == list(range(len(lines))), 'Missing or duplicate acquired line')
    require(bool(lines), 'No acquired lines')
    identity = tuple(lines[0][k] for k in ('pid', 'reader_thread_id', 'child_pid'))
    previous = 0
    for row in lines:
        require(tuple(row[k] for k in ('pid', 'reader_thread_id', 'child_pid')) == identity, 'Reader identity changed')
        times = [row[k] for k in ('received_monotonic_ns', 'forwarding_started_monotonic_ns', 'forwarded_monotonic_ns')]
        require(all(type(t) is int and t > 0 for t in times) and previous <= times[0] <= times[1] <= times[2]
                and not row.get('output_error'), 'Invalid host acquisition/forwarding interval')
        previous = times[0]

    def event(prefix):
        found = [row for row in lines if row['line'].startswith(prefix)]
        require(len(found) == 1, 'Expected one '+prefix)
        return found[0]

    observations = [json.loads(row['line'].split(' ', 1)[1]) for row in lines
                    if row['line'].startswith('TIMER_OBSERVATION ')]
    if not observations:
        return dict(status='unavailable', reason='No guest timer boundaries in this trace')
    require(len(observations) == 3 and {row['timer'] for row in observations} == set(TIMERS),
            'Missing or duplicate timer observation')
    pid = json.loads(event('CLOCK_OBSERVATION ')['line'].split(' ', 1)[1])['pid']
    require(type(pid) is int and pid > 0, 'Invalid guest identity')
    armed = event('ARM 5000 ms timers')
    results = []
    for row in observations:
        require(row['pid'] == pid and row['delay_ms'] == 5000 and row['phases'] == PHASES
                and row['columns'] == COLUMNS and len(row['samples']) == len(PHASES), 'Timer identity/schema mismatch')
        missing = [phase for phase, sample in zip(PHASES, row['samples']) if sample is None]
        for sample in row['samples']:
            if sample is None:
                continue
            require(isinstance(sample, list) and len(sample) == 4
                    and all(type(v) is int and v >= 0 for v in sample)
                    and sample[0] <= sample[2] and sample[3] > 0, 'Missing or invalid guest timer sample')
        marks = dict(zip(PHASES, row['samples']))
        for before, after in [('before-arm-output', 'after-arm-output'),
                              ('after-arm-output', 'before-registration'),
                              ('before-registration', 'after-registration'),
                              ('after-registration', 'loop-entry'),
                              ('callback-entry', 'before-output'), ('before-output', 'after-output')]:
            if marks[before] is not None and marks[after] is not None:
                require(marks[before][2] <= marks[after][0], 'Guest timer boundaries out of order')
        require(len({marks[name][3] for name in PHASES[:5] if marks[name] is not None}) <= 1
                and len({marks[name][3] for name in PHASES[5:] if marks[name] is not None}) <= 1, 'Timer boundary thread changed')

        # Registration precedes every callback, but java.util.Timer can run while the
        # registering thread is still returning from schedule() or preparing its loop.
        for before, after in [('before-registration', 'callback-entry')] + (
                [('loop-entry', 'callback-entry')] if row['timer'] == 'Handler' else []):
            if marks[before] is not None and marks[after] is not None:
                require(marks[before][2] <= marks[after][0], 'Timer callback precedes its causal boundary')
        if row['timer'] == 'Handler':
            require(len({sample[3] for sample in row['samples'] if sample is not None}) <= 1,
                    'Handler callback is not on its registering main looper thread')

        def interval(before, after):
            a, b = marks[before], marks[after]
            if a is None or b is None:
                return None
            result = dict(lower_ns=b[0]-a[2], upper_ns=b[2]-a[0])
            if a[3] == b[3] and a[2] <= b[0]:
                require(b[1] >= a[1], 'Thread CPU observation decreased')
                result['thread_cpu_ns'] = b[1]-a[1]
            return result

        fires = [line for line in lines if line['line'].startswith(TIMERS[row['timer']])]
        require(len(fires) <= 1 and (bool(fires) or (marks['before-output'] is None and marks['after-output'] is None)),
                'Missing or duplicate FIRE output')
        fired = fires[0] if fires else None
        require(fired is None or fired['sequence'] > armed['sequence'], 'Timer output precedes ARM')
        results.append(dict(timer=row['timer'], status='incomplete' if missing else 'complete',
                            missing_phases=missing, guest_intervals={name: interval(a, b) for name, a, b in [
            ('arm_output', 'before-arm-output', 'after-arm-output'),
            ('arm_to_registration', 'after-arm-output', 'before-registration'),
            ('registration', 'before-registration', 'after-registration'),
            ('main_setup', 'after-arm-output', 'loop-entry'),
            ('registered_to_callback', 'after-registration', 'callback-entry'),
            ('arm_to_callback', 'before-arm-output', 'callback-entry'),
            ('callback_to_output', 'callback-entry', 'before-output'),
            ('output', 'before-output', 'after-output')
        ]}, host=dict(arm_to_fire_received_ns=fired['received_monotonic_ns']-armed['received_monotonic_ns'] if fired else None,
                      arm_forwarding_ns=armed['forwarded_monotonic_ns']-armed['received_monotonic_ns'],
                      fire_forwarding_ns=fired['forwarded_monotonic_ns']-fired['received_monotonic_ns'] if fired else None)))
    return dict(status='complete' if all(row['status'] == 'complete' for row in results) else 'partial',
                guest_pid=pid, timers=results)


def handler_failure_details(trace_path):
    """Attach source-bound diagnostics without changing the caller's failed timing verdict."""
    trace_path = Path(trace_path)
    reader = trace_path.with_name(trace_path.stem+'-reader.jsonl')
    identity = dict(reader=reader.name)
    try:
        raw = reader.read_bytes()
        identity['source_sha256'] = hashlib.sha256(raw).hexdigest()
        rows = [json.loads(line) for line in raw.splitlines()]
        lines = [row for row in rows if row.get('state') == 'line']
        require(bool(lines), 'No acquired lines')
        start = lines[0]['received_monotonic_ns']
        rendered = ''.join(f"{(row['received_monotonic_ns']-start)/1e9:+8.3f} s  {row['line']}"
                           for row in lines)
        require(rendered == trace_path.read_text(), 'Reader evidence does not reproduce the checked trace')
        result = analyze(rows)
        handler = next((row for row in result.get('timers', []) if row['timer'] == 'Handler'), None)
        return dict(identity, status=result['status'], handler=handler,
                    **({'reason': result['reason']} if 'reason' in result else {}))
    except FileNotFoundError as error:
        return dict(identity, status='unavailable', error=str(error))
    except (OSError, ValueError, KeyError, TypeError, IndexError, AttributeError, OverflowError) as error:
        return dict(identity, status='invalid', error=f'{type(error).__name__}: {error}')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('reader', type=Path)
    args = parser.parse_args()
    try:
        raw = args.reader.read_bytes()
        result = analyze([json.loads(line) for line in raw.splitlines()])
        result['source_sha256'] = hashlib.sha256(raw).hexdigest()
    except (OSError, ValueError, KeyError, TypeError, IndexError) as error:
        result = dict(status='invalid', error=f'{type(error).__name__}: {error}')
    print(json.dumps(result, indent=2))
    raise SystemExit(0 if result['status'] == 'complete' else 1)
