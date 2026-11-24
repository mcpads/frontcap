"""Generation-bound host requests for the owned XCTest UI bridge."""
import argparse
from dataclasses import asdict, dataclass
import fcntl
import json
from pathlib import Path
import re
import time
import uuid
from typing import ClassVar

from native_clock_control import require_bound_request, atomic_json, recorded_outcome
from native_controller_checkpoint import checkpoint
from native_ios_transport import exchange


@dataclass(frozen=True)
class UIRequest:
    tap_targets: ClassVar[tuple[str, ...]] = ('native-increment', 'native-hold-work', 'native-hold-due')
    generation: str
    request_id: int
    operation: str
    arguments: dict

    def __post_init__(self):
        if str(uuid.UUID(self.generation)) != self.generation:
            raise ValueError('Invalid UI generation')
        if type(self.request_id) is not int or not 0 < self.request_id < 2**63:
            raise ValueError('Invalid UI request id')
        args = self.arguments
        valid = type(args) is dict
        if valid and self.operation == 'launch':
            valid = not args or (set(args) == {'clock_stop_at'}
                                and args['clock_stop_at'] in ('before-advance', 'after-advance', 'after-receipt'))
        elif valid and self.operation in ('terminate', 'finish'):
            valid = not args
        elif valid and self.operation == 'tap':
            valid = ('target' in args and set(args) <= {'target', 'label', 'stop_at'}
                     and args['target'] in self.tap_targets
                     and ('stop_at' not in args or args['stop_at'] in ('before-tap', 'after-tap', 'after-receipt'))
                     and ('label' not in args or type(args['label']) is str and 0 < len(args['label']) <= 80))
        elif valid and self.operation == 'capture':
            valid = (set(args) == {'name', 'clock', 'text'}
                     and all(type(v) is str and 0 < len(v) <= 80 for v in args.values())
                     and re.fullmatch('[a-z0-9-]+', args['name']) is not None)
        else:
            valid = False
        if not valid:
            raise ValueError('Invalid UI operation')


def validate_reply(reply, request):
    if (reply.get('generation') != request.generation or reply.get('request') != asdict(request)
            or type(reply.get('last_id')) is not int or reply['last_id'] != request.request_id
            or reply.get('status') != 'ok' or type(reply.get('result')) is not dict):
        raise RuntimeError('Unconfirmed UI outcome; keep the pending request')
    result = reply['result']
    if request.operation == 'restart':
        from native_ios_restart import replacement
        replacement(result, request.arguments['app'], request.arguments.get('url'))
        valid = True
    elif request.operation in ('launch', 'terminate', 'finish'):
        valid = result.get('state') == ('launched' if request.operation == 'launch' else 'terminated')
    elif request.operation in ('set_app_state', 'set_host_app_state', 'observe_system_alerts', 'tap_system_button', 'observe_targets', 'tap_target', 'press_target', 'replace_text', 'scroll_target'):
        from native_ios_generic_ui import validate_generic_result
        validate_generic_result(result, request)
        valid = True
    elif request.operation == 'tap':
        valid = result.get('input') == 'xctest-tap' and result.get('target') == request.arguments['target']
    else:
        valid = (result.get('phase') == request.arguments['name'] and result.get('clock') == request.arguments['clock']
                 and result.get('text') == request.arguments['text'] and type(result.get('reads')) is dict)
    if not valid:
        raise RuntimeError('UI response does not confirm the requested action')


class UIClient:
    """Single host owner. Unknown tap outcomes block another input until resolved."""
    def __init__(self, descriptor, directory, transport=exchange, *, request_type=UIRequest,
                 validate_owner=None, validate_result=None):
        self.descriptor, self.directory, self.transport = Path(descriptor), Path(directory), transport
        self.directory.mkdir(parents=True, exist_ok=True)
        self.path = self.directory / 'ui-client.json'
        self.request_type = request_type
        self.validate_owner, self.validate_result = validate_owner, validate_result

    def request(self, operation, *, expected_request=None, **arguments):
        return self._perform(operation, arguments, expected_request=expected_request)

    def resume(self, *, expected_request=None):
        return self._perform(expected_request=expected_request)

    def recover(self, *, expected_request=None):
        """Resolve an exact saved terminal receipt without contacting the XCTest process."""
        return self._perform(receipt_only=True, expected_request=expected_request)

    def _perform(self, operation=None, arguments=None, receipt_only=False, expected_request=None):
        with (self.directory / 'ui-client.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            endpoint = json.loads(self.descriptor.read_text())
            if self.validate_owner is not None: self.validate_owner(endpoint)
            if self.path.exists():
                state = json.loads(self.path.read_text())
            else:
                if endpoint.get('status') != 'ready' or endpoint.get('last_id') != 0:
                    raise RuntimeError('UI bridge already has a controller; use its retained directory')
                state = dict(generation=endpoint['generation'], next_id=1, pending=None)
            if operation is None:
                if not state['pending']:
                    raise RuntimeError('No pending UI command')
                request = self.request_type(**state['pending'])
                require_bound_request(request, expected_request)
            else:
                if state['pending']:
                    raise RuntimeError('Pending UI command; resume before issuing another')
                request = self.request_type(state['generation'], state['next_id'], operation, arguments)
                require_bound_request(request, expected_request)
                state['pending'] = asdict(request)
                checkpoint(self.directory, 'before-pending', request, operation='tap')
                atomic_json(self.path, state)
                checkpoint(self.directory, 'after-pending', request, operation='tap')
            row = dict(request=asdict(request), source='receipt' if receipt_only else 'transport',
                       started_monotonic_ns=time.monotonic_ns())
            try:
                if endpoint.get('generation') != state['generation']:
                    raise RuntimeError('UI generation changed; old input remains unresolved')
                if not receipt_only and (type(endpoint.get('port')) is not int or not 0 < endpoint['port'] < 65536):
                    raise RuntimeError('Invalid UI loopback endpoint')
                reply = endpoint if receipt_only else self.transport(endpoint, request, timeout=20)
                row['reply'] = reply
                if self.validate_owner is not None: self.validate_owner(reply)
                validate_reply(reply, request)
                if self.validate_result is not None: self.validate_result(reply, request)
                checkpoint(self.directory, 'after-reply', request, operation='tap')
                state.update(next_id=request.request_id + 1, pending=None, last_reply=reply)
                atomic_json(self.path, state)
                checkpoint(self.directory, 'after-commit', request, operation='tap')
                return reply
            except BaseException as error:
                row['error'] = f'{type(error).__name__}: {error}'
                if hasattr(error, 'native_transport_failure'):
                    row['transport_failure'] = error.native_transport_failure
                raise
            finally:
                row['finished_monotonic_ns'] = time.monotonic_ns()
                with (self.directory / 'ui-commands.jsonl').open('a') as stream:
                    stream.write(json.dumps(row) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--descriptor', required=True, type=Path)
    parser.add_argument('--directory', required=True, type=Path)
    parser.add_argument('operation', choices=['launch', 'tap', 'capture', 'terminate', 'finish', 'resume', 'recover', 'outcome'])
    parser.add_argument('--arguments', type=json.loads, default={})
    args = parser.parse_args()
    if not isinstance(args.arguments, dict) or (args.operation in ('resume', 'recover', 'outcome') and args.arguments):
        parser.error('Arguments must be an object; resume/recover/outcome take no operation arguments')
    if args.operation == 'outcome':
        reply = recorded_outcome(args.directory, validate_reply, request_type=UIRequest, prefix='ui')
    else:
        client = UIClient(args.descriptor, args.directory)
        reply = (getattr(client, args.operation)() if args.operation in ('resume', 'recover')
                 else client.request(args.operation, **args.arguments))
    print(json.dumps(reply, indent=2))
