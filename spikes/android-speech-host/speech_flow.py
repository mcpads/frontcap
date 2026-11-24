"""Interpret the optional lifecycle-only observer without inventing request identity."""
LIFECYCLE = frozenset(('speak', 'stop', 'onStart', 'onSuccess', 'onError', 'onStop'))


def completed_flow_ids(records):
    grouped = {}
    for row in records:
        if row.get('event') == 'boundary' and type(row.get('identity')) is int and 0 < row['identity'] <= 64:
            grouped.setdefault(row['identity'], []).append(row)
    complete = []
    for identity, rows in grouped.items():
        # A reused token, late callback, failure or missing boundary is not a unique completed request.
        if [r.get('kind') for r in rows] != ['speak', 'onStart', 'onSuccess']:
            continue
        if not all(r.get('request_count') == 1 and r.get('correlation') == 'single-observed-request'
                   and r.get('local_error') == 0 and r.get('observer_exception') is False
                   and r.get('limit_reached') is False for r in rows):
            continue
        sequences = [r.get('sequence') for r in rows]
        if all(type(n) is int and 0 < n <= 256 for n in sequences) and sequences[0] < sequences[1] < sequences[2]:
            complete.append(identity)
    return sorted(complete)
