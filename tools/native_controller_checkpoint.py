"""Opt-in real-time rendezvous for killing only an owned controller process."""
from dataclasses import asdict
import os
import time

from native_clock_control import atomic_json


def checkpoint(directory, phase, request, *, operation='advance'):
    if os.environ.get('FC_CONTROLLER_STOP_AT') != phase or request.operation != operation:
        return
    path = directory/'controller-checkpoint.json'
    marker = dict(phase=phase, request=asdict(request), pid=os.getpid(), status='waiting', wait_s=10)
    atomic_json(path, marker)
    time.sleep(10)
    atomic_json(path, {**marker, 'status':'expired'})
    os._exit(78)
