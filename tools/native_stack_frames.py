"""Interpret one preserved ARM64 binary in failure-time raw frames; never edit source evidence."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import struct
import subprocess


def binary_identity(path):
    original = Path(path).read_bytes()
    data, slice_offset = original, 0
    # mach-o/fat.h specifies big-endian 32/64-bit universal architecture tables.
    if len(data) >= 8 and struct.unpack_from('>I', data)[0] in (0xcafebabe, 0xcafebabf):
        magic, count = struct.unpack_from('>II', data)
        layout = '>iiQQII' if magic == 0xcafebabf else '>iiIII'
        width = struct.calcsize(layout)
        if not count or count > 64 or 8 + count * width > len(data):
            raise ValueError('Invalid universal architecture table')
        extents, selected = [], []
        for index in range(count):
            row = struct.unpack_from(layout, data, 8 + index * width)
            cpu, subtype, offset, size, alignment = row[:5]
            if (not size or offset < 8 + count * width or offset + size > len(data)
                    or alignment > 63 or offset % (1 << alignment)
                    or (len(row) == 6 and row[5] != 0)):
                raise ValueError('Invalid universal slice extent or alignment')
            extents.append((offset, offset + size))
            if cpu == 0x100000c and subtype == 0:
                selected.append((offset, size))
        ordered = sorted(extents)
        if any(a[1] > b[0] for a, b in zip(ordered, ordered[1:])):
            raise ValueError('Overlapping universal slices')
        if len(selected) != 1:
            raise ValueError('Expected exactly one ARM64 slice')
        slice_offset, slice_size = selected[0]
        data = original[slice_offset:slice_offset + slice_size]
    if len(data) < 32:
        raise ValueError('Truncated Mach-O header')
    magic, cpu, subtype, _, count, size, _, _ = struct.unpack_from('<IiiIIIII', data)
    if magic != 0xfeedfacf or cpu != 0x100000c or subtype != 0:
        raise ValueError('Expected a thin little-endian ARM64 Mach-O binary')
    if size > len(data) - 32 or count > size // 8:
        raise ValueError('Invalid Mach-O load-command extent')
    offset, uuids, headers, executable = 32, [], [], []
    for _ in range(count):
        if offset + 8 > 32 + size:
            raise ValueError('Truncated load command')
        command, length = struct.unpack_from('<II', data, offset)
        if length < 8 or offset + length > 32 + size:
            raise ValueError('Invalid load-command size')
        if command == 0x1b:
            if length != 24:
                raise ValueError('Invalid UUID command')
            uuids.append(data[offset+8:offset+24].hex())
        if command == 0x19:
            if length < 72:
                raise ValueError('Invalid segment command')
            _, _, _, address, extent, file_offset, file_size, _, protection, _, _ = struct.unpack_from('<II16sQQQQiiII', data, offset)
            if address + extent > 2**64 or file_offset + file_size > len(data):
                raise ValueError('Invalid segment extent')
            if file_offset == 0 and file_size >= 32:
                headers.append(address)
            if protection & 4:
                executable.append((address, address + extent))
        offset += length
    if offset != 32 + size or len(uuids) != 1 or len(headers) != 1 or not executable:
        raise ValueError('Expected one UUID, one loaded header segment and executable segments')
    return dict(uuid=uuids[0], header_vmaddr=headers[0], executable=executable,
                sha256=hashlib.sha256(original).hexdigest(),
                slice_offset=slice_offset, slice_size=len(data))


def fields(line):
    return dict(re.findall(r'(\w+)=([^ ]+)', line.strip()))


def snapshots(text):
    """Keep each scheduler interval separate; never borrow a later image inventory."""
    result, current = [], None
    for line in text.splitlines():
        kind = line.split(' ', 1)[0]
        if kind == 'scheduler':
            current = dict(header=fields(line), threads=[], tasks=[], images=[], complete=False, errors=[])
            result.append(current)
        elif kind == 'timeout':
            current = None
        elif (current is not None and current['complete']
              and kind in ('thread', 'thread_stack', 'thread_frame', 'thread_stack_end',
                           'stack_image', 'scheduler_end')):
            # The producer ends execution/image acquisition before writing task submission
            # sites. Silently dropping late records would hide corruption or interleaving.
            current['errors'].append(f'{kind} follows scheduler_end')
        elif current is not None and (not current['complete'] or kind in ('task', 'task_frame')):
            try:
                row = fields(line)
                if kind == 'thread_stack':
                    number = int(row['id'])
                    if any(thread['id'] == number for thread in current['threads']):
                        raise ValueError('Duplicate thread observation')
                    thread = dict(id=number, addresses=[], stop=row.get('stop'),
                                  state_result=row.get('state_result'),
                                  complete='state_result' not in row)
                    current['threads'].append(thread)
                    if row.get('state_result') == '0':
                        thread['addresses'] = [dict(kind=k, address=int(row[k], 16)) for k in ('pc', 'lr')]
                    elif 'state_result' in row:
                        thread['stop'] = 'state_error'
                elif kind in ('thread_frame', 'thread_stack_end'):
                    matches = [t for t in current['threads'] if t['id'] == int(row['id'])]
                    if len(matches) != 1:
                        raise ValueError('Frame has no unique thread observation')
                    thread = matches[0]
                    if thread['complete']:
                        raise ValueError('Frame record follows completed thread observation')
                    frames = sum(a['kind'] == 'return' for a in thread['addresses'])
                    if kind == 'thread_frame':
                        depth = int(row['depth'])
                        if thread['state_result'] != '0' or depth != frames:
                            raise ValueError('Execution frame lacks successful state or sequential depth')
                        thread['addresses'].append(dict(kind='return', depth=depth, address=int(row['return'], 16)))
                    else:
                        if int(row['frames']) != frames:
                            raise ValueError('Execution frame count does not match terminator')
                        thread['stop'] = row['stop']
                        thread['complete'] = True
                elif kind == 'stack_image' and row.get('stop') == 'uuid':
                    if row.get('result') != '0':
                        raise ValueError('Image UUID has a failed read result')
                    if not re.fullmatch('[0-9a-f]{32}', row['uuid']):
                        raise ValueError('Invalid image UUID')
                    current['images'].append(dict(uuid=row['uuid'], header=int(row['header'], 16)))
                elif kind == 'task':
                    number = int(row['id'])
                    if any(task['id'] == number for task in current['tasks']):
                        raise ValueError('Duplicate task observation')
                    current['tasks'].append(dict(id=number, header=row, addresses=[]))
                elif kind == 'task_frame':
                    matches = [task for task in current['tasks'] if task['id'] == int(row['id'])]
                    if len(matches) != 1:
                        raise ValueError('Frame has no unique task observation')
                    addresses = matches[0]['addresses']
                    depth = int(row['depth'])
                    if depth != len(addresses):
                        raise ValueError('Submission frame depth is not sequential')
                    addresses.append(dict(kind='submission', depth=depth, address=int(row['address'], 16)))
                elif kind == 'scheduler_end':
                    current['end'] = row
                    current['complete'] = True
                    if any(not thread['complete'] for thread in current['threads']):
                        raise ValueError('Execution thread observation has no terminator')
            except (KeyError, ValueError) as error:
                current['errors'].append(str(error))
    return result


def symbolize(binary, load, addresses):
    result = subprocess.run(['xcrun', 'atos', '-arch', 'arm64', '-o', str(binary), '-l', hex(load),
                             *[hex(a) for a in addresses]], capture_output=True, text=True, timeout=10)
    if result.returncode:
        raise ValueError(result.stderr.strip() or 'atos failed')
    lines = result.stdout.splitlines()
    if len(lines) != len(addresses):
        raise ValueError('atos returned a different number of addresses')
    return lines


def analyze(trace, binary, *, resolve=symbolize):
    trace, binary = Path(trace), Path(binary)
    raw = trace.read_bytes()
    identity = binary_identity(binary)
    report = dict(trace=str(trace), trace_sha256=hashlib.sha256(raw).hexdigest(),
                  binary=str(binary), binary_identity=identity, snapshots=[])
    for index, snapshot in enumerate(snapshots(raw.decode('utf-8'))):
        snapshot['index'] = index
        report['snapshots'].append(snapshot)
        matches = [i for i in snapshot['images'] if i['uuid'] == identity['uuid']]
        snapshot['symbol_status'] = ('invalid_trace' if snapshot['errors'] else
                                     'incomplete_snapshot' if not snapshot['complete'] else
                                     'binary_not_observed' if not matches else
                                     'ambiguous_load' if len(matches) != 1 else 'available')
        if snapshot['symbol_status'] != 'available':
            continue
        load = matches[0]['header']
        slide = load - identity['header_vmaddr']
        selected = []
        for thread in snapshot['threads'] + snapshot['tasks']:
            for address in thread['addresses']:
                value = address['address']
                address['status'] = 'outside_binary'
                if any(lo + slide <= value < hi + slide for lo, hi in identity['executable']):
                    address['status'] = 'pending'
                    selected.append(address)
        values = list(dict.fromkeys(a['address'] for a in selected))
        if not values:
            snapshot['symbol_status'] = 'no_addresses_in_binary'
            continue
        try:
            symbols = dict(zip(values, resolve(binary, load, values), strict=True))
            for address in selected:
                name = symbols[address['address']]
                address['status'] = 'unresolved' if re.fullmatch(r'0x[0-9a-fA-F]+', name.strip()) else 'interpreted'
                address['interpretation'] = name
        except (OSError, ValueError, subprocess.SubprocessError) as error:
            snapshot['symbol_status'] = 'symbolizer_error'
            snapshot['symbol_error'] = str(error)
            for address in selected:
                address['status'] = 'unresolved'
    # atos opens the pathname separately from our identity read. Do not publish symbols if
    # that file changed or vanished while the observations were being interpreted.
    try:
        final_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
        verification = dict(status='matched' if final_hash == identity['sha256'] else 'changed',
                            final_sha256=final_hash)
    except OSError as error:
        verification = dict(status='unavailable', error=str(error))
    report['binary_verification'] = verification
    if verification['status'] != 'matched':
        for snapshot in report['snapshots']:
            if snapshot['symbol_status'] in ('available', 'symbolizer_error'):
                snapshot['symbol_status'] = 'binary_unverified'
            for thread in snapshot['threads'] + snapshot['tasks']:
                for address in thread['addresses']:
                    if 'interpretation' in address:
                        del address['interpretation']
                        address['status'] = 'unresolved'
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('trace', type=Path)
    parser.add_argument('binary', type=Path)
    parser.add_argument('--output', type=Path, help='New report path; existing files are never overwritten')
    args = parser.parse_args()
    report = analyze(args.trace, args.binary)
    output = json.dumps(report, indent=2) + '\n'
    if args.output:
        with args.output.open('x') as stream:
            stream.write(output)
    else:
        print(output, end='')


if __name__ == '__main__':
    main()
