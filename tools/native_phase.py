"""Record a host probe's phase boundary without changing its result or clock."""
import argparse
import json
import os
from pathlib import Path
import sys
import time


def record_phase(directory, pid, phase, exit_code=None):
    row = dict(phase=phase, pid=pid, recorder_pid=os.getpid(),
               host_monotonic_ns=time.monotonic_ns(), host_time_ns=time.time_ns())
    if exit_code is not None:
        row['exit_code'] = exit_code
    try:
        with (Path(directory)/'host-phases.jsonl').open('a') as stream:
            stream.write(json.dumps(row)+'\n')
        return True
    except OSError as error:
        print(f'Cannot save host phase evidence: {error}', file=sys.stderr)
        return False


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('directory', type=Path)
    parser.add_argument('pid', type=int)
    parser.add_argument('phase')
    parser.add_argument('exit_code', type=int, nargs='?')
    args = parser.parse_args()
    raise SystemExit(0 if record_phase(args.directory, args.pid, args.phase, args.exit_code) else 1)
