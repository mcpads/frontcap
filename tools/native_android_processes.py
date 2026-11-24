"""Validate guest ps evidence before concluding that a fixture has retired."""


def parse_inventory(text):
    lines = text.splitlines()
    if not lines:
        raise ValueError('Empty process inventory')
    header = lines[0].split()
    if header not in (['PID', 'STAT', 'NAME'], ['PID', 'PPID', 'STAT', 'NAME']):
        raise ValueError('Unknown process inventory header')
    if len(lines) == 1:
        # ps -A runs inside a live guest and must at least observe guest processes.
        raise ValueError('Process inventory has no rows')
    rows = []
    seen = set()
    for line in lines[1:]:
        fields = line.split(maxsplit=len(header)-1)
        if len(fields) != len(header):
            raise ValueError('Incomplete process inventory row')
        values = dict(zip(header, fields))
        pid = int(values['PID'])
        parent = int(values['PPID']) if 'PPID' in values else None
        if pid <= 0 or pid in seen or (parent is not None and parent < 0):
            raise ValueError('Invalid or duplicate process identity')
        seen.add(pid)
        rows.append(dict(pid=pid, ppid=parent, state=values['STAT'], name=values['NAME'], raw=line))
    return rows


def process_absent(text, pid, package):
    if type(pid) is not int or pid <= 0:
        return False
    try:
        rows = parse_inventory(text)
    except ValueError:
        return False
    return not any(row['pid'] == pid or row['name'] == package for row in rows)
