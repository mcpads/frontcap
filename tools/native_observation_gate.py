"""Bounded, per-case diagnostic rendezvous before observation transport dispatch."""
import os
import json
from pathlib import Path
import time

from native_clock_control import atomic_json


def wait_for_release(directory, details, *, wait_seconds=5):
    directory = Path(directory)
    try:
        directory.mkdir()
    except FileExistsError:
        # Only a previously released rendezvous permits later observations. A duplicate while
        # waiting, an expired gate or interrupted marker publication must never bypass the hold.
        try:
            previous = json.loads((directory/'state.json').read_text())
        except (OSError, ValueError) as error:
            raise RuntimeError('Capture gate remains unresolved; retain its original evidence') from error
        if not isinstance(previous, dict) or previous.get('status') != 'released':
            raise RuntimeError('Capture gate remains unresolved; retain its original evidence')
        return
    marker = dict(**details, pid=os.getpid(), status='waiting',
                  started_monotonic_ns=time.monotonic_ns(), wait_seconds=wait_seconds,
                  clock_domain='python_time_monotonic',
                  clock_implementation=time.get_clock_info('monotonic').implementation)
    atomic_json(directory/'state.json', marker)
    deadline = time.monotonic()+wait_seconds
    while not (directory/'release').exists():
        if time.monotonic() >= deadline:
            atomic_json(directory/'state.json', {**marker, 'status':'expired',
                                                'finished_monotonic_ns':time.monotonic_ns()})
            raise TimeoutError('Capture gate expired; operation was not dispatched')
        time.sleep(.005)
    atomic_json(directory/'state.json', {**marker, 'status':'released',
                                        'finished_monotonic_ns':time.monotonic_ns()})
