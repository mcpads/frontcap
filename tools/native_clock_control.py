"""Typed requests and durable state for native clock controllers."""
from dataclasses import dataclass
import fcntl
import json
import os
from pathlib import Path
import uuid


@dataclass(frozen=True)
class Request:
    generation: str
    request_id: int
    operation: str
    delta_ns: int = 0

    def __post_init__(self):
        if str(uuid.UUID(self.generation)) != self.generation:
            raise ValueError('Invalid generation')
        if type(self.request_id) is not int or not 0 < self.request_id <= 2**63 - 1:
            raise ValueError('Invalid request id')
        if self.operation not in ('read', 'advance') or type(self.delta_ns) is not int or not 0 <= self.delta_ns < 2**63:
            raise ValueError('Invalid clock operation')
        if self.operation == 'read' and self.delta_ns:
            raise ValueError('Read cannot advance time')


def require_bound_request(request, expected):
    if expected is not None and request != expected:
        raise RuntimeError('Client cursor changed from bound request')


def atomic_json(path, value):
    temporary = path.with_suffix('.tmp')
    with temporary.open('w') as stream:
        json.dump(value, stream, indent=2)
        stream.flush()
        os.fsync(stream.fileno())
    temporary.replace(path)


def recorded_outcome(directory, validate, *, request_type=Request, prefix='clock'):
    """Read the last consumed request's outcome without touching any target endpoint."""
    directory = Path(directory)
    with (directory/(prefix+'-client.lock')).open('a') as lock:
        # A competing controller must not outlive the caller's operation budget.
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        state = json.loads((directory/(prefix+'-client.json')).read_text())
        if state.get('pending') is not None:
            raise RuntimeError('Pending command; resolve it before reading the last outcome')
        reply = state.get('last_reply')
        if not isinstance(reply, dict):
            raise RuntimeError('No recorded outcome')
        request = request_type(**reply['request'])
        if (request.generation != state['generation'] or request.request_id + 1 != state['next_id']
                or reply.get('generation') != state['generation'] or reply.get('last_id') != request.request_id
                or reply.get('status') not in ('ok', 'failed')):
            raise RuntimeError('Recorded outcome does not match client state')
        validate(reply, request)
        return reply
