"""Collect bounded per-record Flutter raster evidence without truncating log lines."""
import base64
import hashlib
import json
from pathlib import Path
import re
import struct
import zlib

from native_host_process import write_json


def png_dimensions(data):
    if not data.startswith(b'\x89PNG\r\n\x1a\n'):
        raise ValueError('Invalid PNG signature')
    offset, dimensions, ended = 8, None, False
    while offset < len(data):
        size = struct.unpack('>I', data[offset:offset+4])[0]
        kind = data[offset+4:offset+8]
        payload = data[offset+8:offset+8+size]
        checksum = data[offset+8+size:offset+12+size]
        if len(checksum) != 4 or zlib.crc32(kind+payload) != struct.unpack('>I', checksum)[0]:
            raise ValueError('PNG chunk checksum mismatch')
        if kind == b'IHDR':
            dimensions = struct.unpack('>II', payload[:8])
        offset += size+12
        if kind == b'IEND':
            ended = True
            break
    if not ended or offset != len(data) or dimensions is None or min(dimensions) <= 0:
        raise ValueError('Incomplete PNG')
    return dimensions


def collect_captures(log, directory, *, required_names=()):
    directory = Path(directory)
    captures = {}
    partials = []
    for line in Path(log).read_text().splitlines():
        if line.startswith('FRONTCAP_FLUTTER_CAPTURE_PARTIAL '):
            partials.append(json.loads(line.split('FRONTCAP_FLUTTER_CAPTURE_PARTIAL ', 1)[1]))
        for marker, field in [('FRONTCAP_FLUTTER_CAPTURE_META ', 'metadata'),
                              ('FRONTCAP_FLUTTER_CAPTURE_CHUNK ', 'chunks')]:
            if not line.startswith(marker):
                continue
            value = json.loads(line[len(marker):])
            name = value['name']
            if not re.fullmatch('[a-z0-9-]{1,80}', name):
                raise ValueError('Invalid capture name')
            row = captures.setdefault(name, dict(chunks=[]))
            if field == 'chunks':
                row['chunks'].append(value)
            elif field in row:
                raise ValueError('Repeated capture metadata')
            else:
                row[field] = value
    missing = sorted(set(required_names)-set(captures))
    report = dict(passed=not missing, captures=[], missing=missing, partial_captures=partials)
    target = directory/'captures'
    target.mkdir(exist_ok=True)
    for name, row in captures.items():
        item = dict(name=name, metadata=row.get('metadata'), complete=False)
        report['captures'].append(item)
        try:
            chunks = row['chunks']
            if not item['metadata'] or not chunks:
                raise ValueError('Missing metadata or image chunks')
            count = chunks[0]['count']
            if (type(count) is not int or not 0 < count <= 32768 or len(chunks) != count
                    or [c['index'] for c in chunks] != list(range(count))
                    or any(c['count'] != count for c in chunks)):
                raise ValueError('Incomplete or repeated image chunks')
            data = base64.b64decode(''.join(c['data'] for c in chunks), validate=True)
            width, height = png_dimensions(data)
            (target/(name+'.png')).write_bytes(data)
            item.update(complete=True, sha256=hashlib.sha256(data).hexdigest(),
                        width=width, height=height, bytes=len(data))
        except (ValueError, KeyError, struct.error) as error:
            item['error'] = str(error)
            report['passed'] = False
    if not captures:
        report['passed'] = False
    write_json(directory/'capture-report.json', report)
    if not report['passed']:
        raise RuntimeError('Incomplete raster evidence; retain capture report and log')
    return report
