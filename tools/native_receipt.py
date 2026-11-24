"""Publish immutable hash-addressed native receipt bytes durably."""
import hashlib
import json
import os
from pathlib import Path
import tempfile


def publish_receipt(directory, receipt):
    raw = (json.dumps(receipt, sort_keys=True, separators=(',', ':'), ensure_ascii=True)+'\n').encode()
    sha = hashlib.sha256(raw).hexdigest()
    folder = Path(directory); folder.mkdir(exist_ok=True)
    path = folder/(sha+'.json')
    # A killed publication may leave its temporary evidence, but cannot poison the final hash
    # path with partial bytes. An exact local client outcome can publish it on the next call.
    descriptor, temporary = tempfile.mkstemp(prefix='.receipt-', suffix='.tmp', dir=folder)
    with os.fdopen(descriptor, 'wb') as stream:
        stream.write(raw); stream.flush(); os.fsync(stream.fileno())
    try: os.link(temporary, path)
    except FileExistsError:
        if path.read_bytes() != raw: raise RuntimeError('Saved receipt content differs')
    Path(temporary).unlink()
    descriptor = os.open(folder, os.O_RDONLY)
    try: os.fsync(descriptor)
    finally: os.close(descriptor)
    return sha
