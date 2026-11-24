"""Control a live instrumented iOS Simulator app through its loopback endpoint."""
import argparse
from dataclasses import asdict
import fcntl
import json
from pathlib import Path
import time

from native_clock_control import require_bound_request, Request, atomic_json, recorded_outcome
from native_controller_checkpoint import checkpoint
from native_ios_discovery import app_container
from native_ios_transport import exchange


def validate_reply(reply, request):
    if (reply.get('request') != asdict(request) or reply.get('generation') != request.generation
            or reply.get('last_id') != request.request_id or reply.get('status') not in ('ok', 'failed')):
        raise RuntimeError('Unconfirmed clock outcome; retain the pending request')
    before, now, applied = (reply.get(k) for k in ('before_ns', 'now_ns', 'applied'))
    advance, drain = reply.get('advance_result'), reply.get('drain_result')
    if (type(before) is not int or type(now) is not int or not 0 <= before <= now < 2**63
            or type(applied) is not bool or now != before + (request.delta_ns if applied else 0)):
        raise RuntimeError('Malformed clock mutation outcome')
    if request.operation == 'read':
        valid = reply['status'] == 'ok' and not applied and advance is None and drain is None
    else:
        valid = (type(advance) is int and advance >= 0 and applied == (advance == 0)
                 and ((not applied and drain is None) or (applied and type(drain) is int and drain >= 0))
                 and (reply['status'] == 'ok') == (applied and drain == 0))
    if not valid:
        raise RuntimeError('Inconsistent advance/drain outcome')


class ClockClient:
    def __init__(self, descriptor, directory, transport=exchange, *, validate_owner=None):
        self.descriptor, self.directory, self.transport = Path(descriptor), Path(directory), transport
        self.validate_owner = validate_owner
        self.directory.mkdir(parents=True, exist_ok=True)
        self.path = self.directory / 'clock-client.json'

    def request(self, operation, delta_ns=0, *, expected_request=None):
        return self._perform(operation, delta_ns, expected_request=expected_request)

    def resume(self, *, expected_request=None):
        return self._perform(expected_request=expected_request)

    def recover(self, *, expected_request=None):
        """Resolve only a recorded terminal outcome, without contacting the target."""
        return self._perform(receipt_only=True, expected_request=expected_request)

    def reserve(self, request, *, binding=None):
        """Persist an externally delivered advance; never send it or authorize a UI retry."""
        if not isinstance(request, Request) or request.operation != 'advance':
            raise ValueError('External delivery requires an exact advance request')
        with (self.directory / 'clock-client.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            state = json.loads(self.path.read_text())  # Initialization must already have established ownership.
            validate_external_pending(state)
            if binding is not None:
                validate_ui_binding(binding,request)
            expected = Request(state['generation'], state['next_id'], 'advance', request.delta_ns)
            require_bound_request(expected, request)
            endpoint = json.loads(self.descriptor.read_text())
            if self.validate_owner is not None: self.validate_owner(endpoint)
            if endpoint.get('generation') != request.generation:
                raise RuntimeError('Target generation changed')
            if state['pending'] is not None:
                if (state['pending'] != asdict(request) or state.get('external') is not True
                        or state.get('external_ui')!=binding):
                    raise RuntimeError('Another clock delivery remains pending')
                return request
            if binding is not None and state.get('last_release',{}).get('binding')==binding:
                raise RuntimeError('Released UI request cannot reserve again')
            if (endpoint.get('status') not in ('ready','ok','failed')
                    or type(endpoint.get('last_id')) is not int
                    or endpoint['last_id'] != request.request_id-1):
                raise RuntimeError('Target clock cursor differs before external reservation')
            state.update(pending=asdict(request), external=True)
            if binding is not None:state['external_ui']=binding
            checkpoint(self.directory, 'before-pending', request)
            atomic_json(self.path, state)
            checkpoint(self.directory, 'after-pending', request)
            return request

    def release_ui(self, request, binding, receipt_sha256, verify_proof):
        """Release only a bound, unused advance while the caller holds the UI owner locks."""
        validate_ui_binding(binding,request)
        if (type(receipt_sha256) is not str or len(receipt_sha256)!=64
                or any(c not in '0123456789abcdef' for c in receipt_sha256)):
            raise ValueError('Invalid UI proof digest')
        release=dict(request=asdict(request),binding=binding,receipt_sha256=receipt_sha256)
        with (self.directory/'clock-client.lock').open('a') as lock:
            fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
            state=json.loads(self.path.read_text());validate_external_pending(state)
            verify_proof()
            if state['pending'] is None and state.get('last_release')==release:
                return release
            if (state['pending']!=asdict(request) or state.get('external_ui')!=binding
                    or state.get('external') is not True):
                raise RuntimeError('UI release differs from the pending reservation')
            endpoint=json.loads(self.descriptor.read_text())
            if self.validate_owner is not None:self.validate_owner(endpoint)
            if (endpoint.get('generation')!=request.generation
                    or endpoint.get('status') not in ('ready','ok','failed')
                    or type(endpoint.get('last_id')) is not int or endpoint['last_id']!=request.request_id-1):
                raise RuntimeError('Reserved clock may have been delivered')
            state.update(pending=None,last_release=release)
            state.pop('external');state.pop('external_ui')
            checkpoint(self.directory,'before-release',request)
            atomic_json(self.path,state)
            checkpoint(self.directory,'after-release',request)
            return release

    def _perform(self, operation=None, delta_ns=0, receipt_only=False, expected_request=None):
        with (self.directory / 'clock-client.lock').open('a') as lock:
            # A competing controller must not outlive the caller's operation budget.
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            endpoint = json.loads(self.descriptor.read_text())
            if self.validate_owner is not None: self.validate_owner(endpoint)
            if self.path.exists():
                state = json.loads(self.path.read_text())
            else:
                if endpoint.get('status') != 'ready' or endpoint.get('last_id') != 0:
                    raise RuntimeError('Target already has a controller; use its retained directory')
                state = dict(generation=endpoint['generation'], next_id=1, pending=None)
            validate_external_pending(state)
            if state.get('external') and not receipt_only:
                raise RuntimeError('External clock delivery can only recover its retained receipt')
            if operation is None:
                if not state['pending']:
                    raise RuntimeError('No pending clock command')
                request = Request(**state['pending'])
                require_bound_request(request, expected_request)
            else:
                if state['pending']:
                    raise RuntimeError('Pending clock command; resume before issuing another')
                request = Request(state['generation'], state['next_id'], operation, delta_ns)
                require_bound_request(request, expected_request)
                state['pending'] = asdict(request)
                checkpoint(self.directory, 'before-pending', request)
                atomic_json(self.path, state)
                checkpoint(self.directory, 'after-pending', request)
            row = dict(request=asdict(request), source='receipt' if receipt_only else 'transport',
                       started_monotonic_ns=time.monotonic_ns())
            try:
                if endpoint.get('generation') != state['generation']:
                    raise RuntimeError('Target generation changed; old outcome remains unresolved')
                if not receipt_only and (type(endpoint.get('port')) is not int or not 0 < endpoint['port'] < 65536):
                    raise RuntimeError('Invalid loopback endpoint')
                reply = endpoint if receipt_only else self.transport(endpoint, request)
                row['reply'] = reply
                if self.validate_owner is not None: self.validate_owner(reply)
                validate_reply(reply, request)
                checkpoint(self.directory, 'after-reply', request)
                state.update(next_id=request.request_id + 1, pending=None, last_reply=reply)
                state.pop('external', None)
                state.pop('external_ui', None)
                atomic_json(self.path, state)
                checkpoint(self.directory, 'after-commit', request)
                return reply
            except BaseException as error:
                row['error'] = f'{type(error).__name__}: {error}'
                if hasattr(error, 'native_transport_failure'):
                    row['transport_failure'] = error.native_transport_failure
                raise
            finally:
                row['finished_monotonic_ns'] = time.monotonic_ns()
                with (self.directory / 'clock-commands.jsonl').open('a') as stream:
                    stream.write(json.dumps(row) + '\n')


def validate_external_pending(state):
    if 'external' in state and (state['external'] is not True or state.get('pending') is None
            or Request(**state['pending']).operation != 'advance'):
        raise RuntimeError('Invalid external clock reservation')
    if 'external_ui' in state:
        if state.get('external') is not True:raise RuntimeError('UI binding lacks external reservation')
        validate_ui_binding(state['external_ui'],Request(**state['pending']))


def validate_ui_binding(binding, request):
    # Lazy import: the generic result validator reuses this module's clock validation.
    from native_ios_generic_ui import UIKitUIRequest
    from native_ios_clock_bridge import identity
    if type(binding) is not dict or set(binding)!= {'identity','request'}:
        raise ValueError('Invalid UI reservation binding')
    identity(binding['identity'])
    ui=UIKitUIRequest(**binding['request'])
    if (ui.generation!=binding['identity']['generation'] or ui.operation!='press_target'
            or ui.arguments['clock_request']!=asdict(request) or request.operation!='advance'):
        raise ValueError('UI binding differs from reserved clock')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    target = parser.add_mutually_exclusive_group()
    target.add_argument('--simulator', help='Owned, running simulator UUID for descriptor discovery')
    target.add_argument('--descriptor', type=Path, help='Previously resolved private clock descriptor')
    parser.add_argument('--bundle', default='dev.frontcap.s18.probe')
    parser.add_argument('--directory', required=True, type=Path)
    parser.add_argument('operation', choices=['read', 'advance', 'resume', 'recover', 'outcome'])
    parser.add_argument('--delta-ns', type=int, default=0)
    args = parser.parse_args()
    if args.operation != 'advance' and args.delta_ns:
        parser.error('--delta-ns is only valid for advance')
    if args.operation == 'outcome':
        reply = recorded_outcome(args.directory, validate_reply)
        print(json.dumps(reply, indent=2))
        raise SystemExit(0 if reply['status'] == 'ok' else 1)
    if not args.simulator and not args.descriptor:
        parser.error('A simulator or descriptor is required for target operations')
    descriptor = (args.descriptor if args.descriptor else
                  app_container(args.simulator, args.bundle, args.directory, timeout=10)/'Documents/clock-control.json')
    client = ClockClient(descriptor, args.directory)
    reply = (getattr(client, args.operation)() if args.operation in ('resume', 'recover')
             else client.request(args.operation, args.delta_ns))
    print(json.dumps(reply, indent=2))
    raise SystemExit(0 if reply['status'] == 'ok' else 1)
