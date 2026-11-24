"""Bound a conditional unit-rate clock offset from an independently bound Darwin log/trace pair."""
import argparse
import hashlib
import json
from pathlib import Path

from native_file_trace import analyze


def interval(row, start, end):
    lo, hi = row[start], row[end]
    if type(lo) is not int or type(hi) is not int or not 0 <= lo <= hi:
        raise ValueError('Invalid observation interval')
    return lo, hi


def align(log, reads, trace, fd=2):
    """Requires one unreused PID, one recording, and an exclusively bound log descriptor."""
    if type(fd) is not int or fd < 0:
        raise ValueError('Invalid descriptor')
    names = ('BSC_write', 'BSC_write_nocancel')
    if trace['unavailable'] or any(e['syscall'] in names for key in
                                  ('unmatched_starts', 'unmatched_ends') for e in trace[key]):
        raise ValueError('Incomplete or ambiguous syscall evidence')
    writes = sorted((e for e in trace['paired_events'] if e['syscall'] in names
                     and e['start_arguments']['arg1'] == fd), key=lambda e: e['start_ns'])
    offset, previous_end = 0, 0
    spans = {}
    for write in writes:
        start, end = interval(write, 'start_ns', 'end_ns')
        count = write['end_arguments']['arg2']
        if (type(count) is not int or count <= 0 or write['end_arguments']['arg1'] != 0
                or count != write['start_arguments']['arg3'] or start < previous_end):
            raise ValueError('Overlapping, failed or partial log writes')
        spans[(offset, count)] = (start, end)
        offset += count
        previous_end = end
    if offset != len(log):
        raise ValueError('Log bytes do not match the complete write series')
    anchors, offset, previous_end = [], 0, 0
    for read in reads:
        start, end = interval(read, 'started_monotonic_ns', 'finished_monotonic_ns')
        length = read['length']
        if (type(length) is not int or length <= 0 or type(read['offset']) is not int
                or read['offset'] != offset or start < previous_end):
            raise ValueError('Invalid or discontinuous log reads')
        chunk = log[offset:offset + length]
        if len(chunk) != length or hashlib.sha256(chunk).hexdigest() != read['sha256']:
            raise ValueError('Log read bytes differ from their recorded digest')
        if (offset, length) not in spans:
            raise ValueError('Read/write boundaries do not match uniquely')
        write_start, write_end = spans[(offset, length)]
        empty = read.get('previous_empty_read')
        if empty is not None:
            empty_start, empty_end = interval(empty, 'started_monotonic_ns', 'finished_monotonic_ns')
            if empty['offset'] != offset or not previous_end <= empty_start <= empty_end <= start:
                raise ValueError('Empty read does not precede this byte range')
            # A write becomes visible inside its interval: bound END below, START above.
            anchors.append(dict(offset=offset, bytes=length, trace_start_ns=write_start,
                                trace_end_ns=write_end,
                                bounds_ns=[empty_start - write_end, end - write_start]))
        offset += length
        previous_end = end
    if offset != len(log) or len(reads) != len(writes) or len(anchors) < 2:
        raise ValueError('Need complete byte coverage and at least two bounded observations')
    low = max(a['bounds_ns'][0] for a in anchors)
    high = min(a['bounds_ns'][1] for a in anchors)
    if low > high:
        raise ValueError('Clock bounds contradict a constant unit-rate offset')
    return dict(status='conditional_alignment',
                assumptions=['same recording and unreused PID', 'exclusive log descriptor binding',
                             'unit relative clock rate; not proven by this calculation'],
                log_bytes=len(log), reads=len(reads), writes=len(writes), anchors=anchors,
                common_bounds_ns=[low, high])



def refine_with_markers(report, markers, trace):
    """Intersect log bounds with bracketed, uniquely identified opens in the same PID."""
    if type(markers['pid']) is not int or markers['pid'] != trace['pid']:
        raise ValueError('Calibration PID differs from the trace')
    anchors, paths = [], set()
    previous_host_end = previous_trace_end = 0
    for marker in markers['rows']:
        path = marker['path']
        if not isinstance(path, str) or not Path(path).is_absolute() or path in paths:
            raise ValueError('Calibration paths must be unique and absolute')
        paths.add(path)
        before, after = interval(marker, 'before_ns', 'after_ns')
        calls = [c for c in trace['calls'] if c['path'] == path
                 and c['syscall'] in ('BSC_open', 'BSC_open_nocancel')]
        if len(calls) != 1:
            raise ValueError('Calibration open is missing or ambiguous')
        call = calls[0]
        pairs = [e for e in trace['paired_events'] if e['thread_id'] == call['thread_id']
                 and e['syscall'] == call['syscall'] and e['start_ns'] == call['start_ns']]
        if len(pairs) != 1:
            raise ValueError('Calibration open lacks a unique raw pair')
        pair = pairs[0]
        start, end = interval(pair, 'start_ns', 'end_ns')
        if (call['errno'] != 0 or type(call['result']) is not int or call['result'] < 0
                or pair['end_arguments']['arg1'] != call['errno']
                or pair['end_arguments']['arg2'] != call['result']
                or pair['duration_ns'] != call['duration_ns'] or end - start != call['duration_ns']):
            raise ValueError('Calibration raw and processed outcomes disagree or failed')
        if before < previous_host_end or start < previous_trace_end:
            raise ValueError('Calibration ordering is inconsistent')
        bounds = [before - start, after - end]
        if bounds[0] > bounds[1]:
            raise ValueError('Calibration call exceeds its host bracket')
        anchors.append(dict(path=path, before_ns=before, after_ns=after,
                            trace_start_ns=start, trace_end_ns=end, bounds_ns=bounds))
        previous_host_end, previous_trace_end = after, end
    if len(anchors) < 2:
        raise ValueError('Need at least two calibration opens')
    low, high = max(a['bounds_ns'][0] for a in anchors), min(a['bounds_ns'][1] for a in anchors)
    combined = [max(low, report['common_bounds_ns'][0]), min(high, report['common_bounds_ns'][1])]
    if low > high or combined[0] > combined[1]:
        raise ValueError('Calibration contradicts the clock bounds')
    return dict(report, log_bounds_ns=report['common_bounds_ns'], common_bounds_ns=combined,
                calibration=dict(pid=markers['pid'], anchors=anchors, common_bounds_ns=[low, high]))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    parser.add_argument('--pid', type=int, required=True)
    parser.add_argument('--log', type=Path, required=True)
    parser.add_argument('--reads', type=Path, required=True)
    parser.add_argument('--fd', type=int, default=2)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--markers', type=Path, help='Same-PID bracketed opens; does not establish clock rate')
    args = parser.parse_args()
    trace = analyze(args.directory, args.pid)
    log, reads = args.log.read_bytes(), args.reads.read_bytes()
    report = align(log, [json.loads(line) for line in reads.splitlines()], trace, args.fd)
    report.update(pid=args.pid, fd=args.fd, sources=trace['sources'] + [
        dict(path=str(path), sha256=hashlib.sha256(raw).hexdigest())
        for path, raw in ((args.log, log), (args.reads, reads))])
    if args.markers:
        raw = args.markers.read_bytes()
        report = refine_with_markers(report, json.loads(raw), trace)
        report['sources'].append(dict(path=str(args.markers), sha256=hashlib.sha256(raw).hexdigest()))
    output = json.dumps(report, indent=2) + '\n'
    if args.output:
        with args.output.open('x') as stream:
            stream.write(output)
    else:
        print(output, end='')


if __name__ == '__main__':
    main()
