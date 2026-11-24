"""Inspect Xcode file-call exports for an independently identified PID, without judging its exit."""
import argparse
import hashlib
import json
from pathlib import Path
import xml.etree.ElementTree as ET


class ExportTable:
    def __init__(self, path, schema):
        raw = Path(path).read_bytes()
        self.source = dict(path=str(path), sha256=hashlib.sha256(raw).hexdigest())
        root = ET.fromstring(raw)
        self.references = {}
        for element in root.iter():
            identity = element.get('id')
            if identity is not None:
                if identity in self.references:
                    raise ValueError('Duplicate export identity')
                self.references[identity] = element
        declarations = root.findall('.//schema')
        if not declarations or any(d.get('name') != schema for d in declarations):
            raise ValueError('Unexpected or missing export schema')
        columns = [c.findtext('mnemonic') for c in declarations[0].findall('col')]
        if not columns or None in columns or len(columns) != len(set(columns)):
            raise ValueError('Invalid export columns')
        if any([c.findtext('mnemonic') for c in d.findall('col')] != columns for d in declarations):
            raise ValueError('Inconsistent export columns')
        self.rows = []
        for row in root.findall('.//row'):
            if len(row) != len(columns):
                raise ValueError('Export row does not match its declared columns')
            self.rows.append({k: self.resolve(v) for k, v in zip(columns, row, strict=True)})

    def resolve(self, element):
        seen = set()
        while element is not None and element.get('ref') is not None:
            reference = element.get('ref')
            if reference in seen or reference not in self.references:
                raise ValueError('Cyclic or missing export reference')
            seen.add(reference)
            element = self.references[reference]
        return element

    def child(self, element, name):
        element = self.resolve(element)
        return self.resolve(element.find(name)) if element is not None else None

    def pid(self, process):
        return number(self.child(process, 'pid'))


def number(element):
    if element is None or element.tag == 'sentinel':
        return None
    return int(element.text)


def analyze(directory, pid):
    """An unmatched event is partial evidence, never a synthesized completed call."""
    if type(pid) is not int or pid <= 0:
        raise ValueError('Expected an independently identified positive PID')
    root = Path(directory)
    calls = ExportTable(root/'FsSyscall.xml', 'FsSyscall')
    events = ExportTable(root/'kdebug.xml', 'kdebug')
    names = ExportTable(root/'syscall-name-map.xml', 'syscall-name-map')
    mapping = {}
    for row in names.rows:
        code = tuple(number(row[k]) for k in ('class', 'subclass', 'code'))
        name = row['name'].text
        if None in code or not name:
            raise ValueError('Incomplete syscall mapping')
        if code in mapping and mapping[code] != name:
            raise ValueError('Conflicting syscall mapping')
        mapping[code] = name
    report = dict(pid=pid, sources=[table.source for table in (calls, events, names)],
                  calls=[], paired_events=[], unmatched_starts=[], unmatched_ends=[],
                  unavailable=[], other_pid_rows=0)
    for row in calls.rows:
        observed_pid = calls.pid(row['process'])
        if observed_pid is None:
            report['unavailable'].append(dict(table='FsSyscall', reason='missing_process_identity'))
            continue
        if observed_pid != pid:
            report['other_pid_rows'] += 1
            continue
        report['calls'].append(dict(thread_id=number(calls.child(row.get('thread'), 'tid')),
                                   start_ns=number(row['start']), duration_ns=number(row['duration']),
                                   syscall=row['syscall'].text, path=row['path'].text,
                                   result=number(row['return']), errno=number(row['errno'])))
    pending = {}
    previous = {}
    for row in events.rows:
        code = tuple(number(row[k]) for k in ('class', 'subclass', 'code'))
        name = mapping.get(code)
        if name is None:
            report['unavailable'].append(dict(table='kdebug', reason='unmapped_event', code=code))
            continue
        if not name.startswith('BSC_'):
            continue
        observed_pid = events.pid(events.child(row['thread'], 'process'))
        if observed_pid is None:
            report['unavailable'].append(dict(table='kdebug', reason='missing_process_identity', syscall=name))
            continue
        if observed_pid != pid:
            report['other_pid_rows'] += 1
            continue
        tid = number(events.child(row['thread'], 'tid'))
        when = number(row['time'])
        phase = row['function'].get('fmt')
        if tid is None or when is None or phase not in ('START', 'END'):
            report['unavailable'].append(dict(table='kdebug', reason='incomplete_event', syscall=name))
            continue
        key = (tid, code)
        if key in previous and when < previous[key]:
            raise ValueError('Syscall events move backwards within a thread')
        previous[key] = when
        event = dict(thread_id=tid, syscall=name, time_ns=when,
                     arguments={f'arg{i}': number(row.get(f'arg{i}')) for i in range(1, 5)})
        if phase == 'START':
            if key in pending:
                # A missing end or nested call cannot justify guessing a pairing.
                report['unmatched_starts'].append(pending.pop(key))
                report['unavailable'].append(dict(table='kdebug', reason='repeated_start', **event))
                event['ambiguous'] = True
            pending[key] = event
        elif key not in pending:
            report['unmatched_ends'].append(event)
        else:
            start = pending.pop(key)
            if start.get('ambiguous'):
                report['unmatched_starts'].append(start)
                report['unmatched_ends'].append(event)
            else:
                report['paired_events'].append(dict(thread_id=tid, syscall=name,
                    start_ns=start['time_ns'], end_ns=when, duration_ns=when-start['time_ns'],
                    start_arguments=start['arguments'], end_arguments=event['arguments']))
    report['unmatched_starts'].extend(pending.values())
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    parser.add_argument('--pid', required=True, type=int, help='PID from independent acquisition evidence')
    parser.add_argument('--output', type=Path, help='New report path; existing files are never overwritten')
    args = parser.parse_args()
    output = json.dumps(analyze(args.directory, args.pid), indent=2) + '\n'
    if args.output:
        with args.output.open('x') as stream:
            stream.write(output)
    else:
        print(output, end='')


if __name__ == '__main__':
    main()
