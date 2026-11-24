"""Send generation-bound clock commands to the installed Android fixture."""
import argparse
from dataclasses import asdict
import fcntl
import json
from pathlib import Path
import re
import subprocess
import time
from native_clock_control import require_bound_request, Request, atomic_json, recorded_outcome
from native_controller_checkpoint import checkpoint


def read_receipt(command, package, **options):
    return json.loads(command('shell', 'run-as', package, 'cat', 'files/clock-control.json', **options))


def validate_reply(reply, request):
    if reply.get('request') != asdict(request) or reply.get('status') not in ('ok', 'rejected'):
        raise RuntimeError('Clock outcome unavailable; retain and resume the same request')
    before, now, applied = (reply.get(k) for k in ('before_ns', 'now_ns', 'applied'))
    if (type(before) is not int or type(now) is not int or not 0 <= before <= now < 2**63
            or type(applied) is not bool or type(reply.get('last_id')) is not int or reply['last_id'] < 0
            or now != before + (request.delta_ns if applied else 0)):
        raise RuntimeError('Malformed clock receipt')
    if reply['status'] == 'ok':
        if (reply.get('generation') != request.generation or reply['last_id'] != request.request_id
                or applied != (request.operation == 'advance')):
            raise RuntimeError('Clock receipt does not confirm the requested operation')
    elif applied or not isinstance(reply.get('error'), str) or not reply['error']:
        raise RuntimeError('Malformed clock rejection')


def exchange(command, package, request, evidence, *, receipt_only=False, transport="broadcast", receipt_timeout=20):
    if transport not in ('broadcast','provider'):
        raise ValueError('Unsupported clock transport')
    if not re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]*(?:\.[A-Za-z_][A-Za-z_0-9]*)+', package):
        raise ValueError('Invalid Android package')
    row = dict(request=asdict(request), source='receipt' if receipt_only else transport,
               started_monotonic_ns=time.monotonic_ns())
    phase = 'receipt' if receipt_only else transport
    try:
        if not receipt_only and transport == 'provider':
            deadline = time.monotonic()+receipt_timeout
            args=['shell','content','call','--uri','content://'+package+'.clock','--method','execute']
            for name,value in asdict(request).items():
                kind='l' if name in ('request_id','delta_ns') else 's'
                args.extend(['--extra',f'{name}:{kind}:{value}'])
            remaining=deadline-time.monotonic()
            if remaining<=0:raise TimeoutError('Provider clock submission deadline elapsed; retain pending request')
            output=command(*args,timeout=remaining)
            row['provider_output']=output.decode(errors='replace')
            if row['provider_output'].strip()!='Result: Bundle[{submitted=true}]':
                raise RuntimeError('Clock submission unconfirmed; retain the pending request')
            phase='provider-receipt'
            while True:
                remaining=deadline-time.monotonic()
                if remaining<=0:raise TimeoutError('Provider clock receipt deadline elapsed; retain pending request')
                reply=read_receipt(command,package,timeout=remaining)
                row.setdefault('provider_receipts',[]).append(reply)
                if time.monotonic()>deadline:raise TimeoutError('Provider clock receipt arrived after deadline')
                if reply.get('generation')!=request.generation:raise RuntimeError('Target generation changed during provider submission')
                if reply.get('request')==asdict(request) and reply.get('status') in ('ok','rejected'):
                    break
                time.sleep(min(.02,max(0,deadline-time.monotonic())))
        elif not receipt_only:
            output = command('shell', 'am', 'broadcast', '--receiver-foreground', '-a', package + '.CLOCK_CONTROL',
                             '-p', package, '--es', 'generation', request.generation,
                             '--el', 'request_id', str(request.request_id), '--es', 'operation', request.operation,
                             '--el', 'delta_ns', str(request.delta_ns))
            phase = 'broadcast-validation'
            row['broadcast_output'] = output.decode(errors='replace')
            expected = f'Broadcast completed: result=-1, data="{request.generation}:{request.request_id}"'
            if expected not in row['broadcast_output'].splitlines():
                raise RuntimeError('Receiver completion unavailable; retain the pending request')
        phase = 'receipt'
        if receipt_only or transport!='provider':
            reply = read_receipt(command, package)
        phase = 'reply-validation'
        row['reply'] = reply
        validate_reply(reply, request)
        return reply
    except BaseException as error:
        row['failed_phase'] = phase
        if isinstance(error, (subprocess.TimeoutExpired, subprocess.CalledProcessError)):
            detail = dict(timeout_s=getattr(error, 'timeout', None),
                          exit_code=getattr(error, 'returncode', None))
            for stream in ('stdout', 'stderr'):
                data = getattr(error, stream, None)
                detail[stream] = (dict(text=data.decode(errors='replace'), bytes=len(data), hex=data.hex())
                                  if isinstance(data, bytes) else
                                  dict(text=data, bytes=None, hex=None))
            row['transport_failure'] = detail
        row['error'] = f'{type(error).__name__}: {error}'
        raise
    finally:
        row['finished_monotonic_ns'] = time.monotonic_ns()
        with evidence.open('a') as stream:
            stream.write(json.dumps(row) + '\n')


class ClockRejected(RuntimeError):
    """A validated non-consuming rejection, persisted before the exception is returned."""
    def __init__(self, reply):
        self.reply = reply
        super().__init__('Clock command rejected: ' + str(reply.get('error')))


class ClockClient:
    """One serialized controller. Uncertain transport outcomes retain their exact request."""
    def __init__(self, command, directory, package='dev.frontcap.s18.probe', *, transport='broadcast', receipt_timeout=20):
        if not re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]*(?:\.[A-Za-z_][A-Za-z_0-9]*)+', package):
            raise ValueError('Invalid Android package')
        if transport not in ('broadcast','provider'):
            raise ValueError('Unsupported clock transport')
        if type(receipt_timeout) not in (int,float) or not 0<receipt_timeout<=3600:
            raise ValueError('Invalid clock receipt timeout')
        self.transport, self.receipt_timeout = transport, receipt_timeout
        self.command, self.directory, self.package = command, Path(directory), package
        self.directory.mkdir(parents=True, exist_ok=True)
        self.path = self.directory / 'clock-client.json'
        self.evidence = self.directory / 'clock-commands.jsonl'

    def _perform(self, operation=None, delta_ns=0, receipt_only=False, expected_request=None):
        with (self.directory / 'clock-client.lock').open('a') as lock:
            # A competing controller must not outlive the caller's operation budget.
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            if self.path.exists():
                state = json.loads(self.path.read_text())
                if state.get('package') != self.package:
                    raise RuntimeError('Clock package changed or missing; retain the original client')
            else:
                ready = read_receipt(self.command, self.package)
                if ready.get('status') != 'ready' or ready.get('last_id') != 0:
                    raise RuntimeError('Target already has a controller; use its retained client directory')
                state = dict(package=self.package, generation=ready['generation'], next_id=1, pending=None)
                if self.transport!='broadcast':state['transport']=self.transport
            if state.get('transport','broadcast')!=self.transport:
                raise RuntimeError('Clock transport changed; retain the original client')
            if operation is None:
                if not state['pending']:
                    raise RuntimeError('No pending clock command')
                request = Request(**state['pending'])
                require_bound_request(request, expected_request)
            else:
                if state['pending']:
                    raise RuntimeError('Pending clock command; resume it before sending another')
                request = Request(state['generation'], state['next_id'], operation, delta_ns)
                require_bound_request(request, expected_request)
                state['pending'] = asdict(request)
                checkpoint(self.directory, 'before-pending', request)
                atomic_json(self.path, state)
                checkpoint(self.directory, 'after-pending', request)
            current = read_receipt(self.command, self.package)
            if current.get('generation') != state['generation']:
                raise RuntimeError('Target generation changed; old request remains unresolved')
            reply = exchange(self.command, self.package, request, self.evidence, receipt_only=receipt_only,
                             transport=self.transport,receipt_timeout=self.receipt_timeout)
            if reply.get('generation') != state['generation']:
                raise RuntimeError('Target generation changed during the request')
            if reply['status'] != 'ok':
                if (reply.get('error') in ('invalid_request', 'clock_overflow') and reply['applied'] is False
                        and reply.get('last_id') == request.request_id - 1):
                    state.update(pending=None, last_rejection=reply)
                    atomic_json(self.path, state)
                    raise ClockRejected(reply)
                raise RuntimeError('Clock command rejected: ' + str(reply.get('error')))
            if reply.get('last_id') != request.request_id or reply['applied'] != (request.operation == 'advance'):
                raise RuntimeError('Clock receipt does not confirm the requested operation')
            checkpoint(self.directory, 'after-reply', request)
            state.update(next_id=request.request_id + 1, pending=None, last_reply=reply)
            atomic_json(self.path, state)
            checkpoint(self.directory, 'after-commit', request)
            return reply

    def request(self, operation, delta_ns=0, *, expected_request=None):
        return self._perform(operation, delta_ns, expected_request=expected_request)

    def resume(self, *, expected_request=None):
        return self._perform(expected_request=expected_request)

    def recover(self, *, expected_request=None):
        """Read an exact terminal receipt without broadcasting to a possibly dead target."""
        return self._perform(receipt_only=True, expected_request=expected_request)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--adb', required=True)
    parser.add_argument('--serial', required=True)
    parser.add_argument('--directory', required=True, type=Path)
    parser.add_argument('--package', default='dev.frontcap.s18.probe')
    parser.add_argument('--transport',choices=('broadcast','provider'),default='broadcast')
    parser.add_argument('operation', choices=['read', 'advance', 'resume', 'recover', 'outcome'])
    parser.add_argument('--delta-ns', type=int, default=0)
    args = parser.parse_args()
    if args.operation != 'advance' and args.delta_ns:
        parser.error('--delta-ns is only valid for advance')
    if args.operation == 'outcome':
        print(json.dumps(recorded_outcome(args.directory, validate_reply), indent=2))
        raise SystemExit(0)
    def command(*tail,timeout=20):
        return subprocess.check_output([args.adb, '-s', args.serial, *tail], timeout=timeout)
    client = ClockClient(command, args.directory, args.package,transport=args.transport)
    reply = (getattr(client, args.operation)() if args.operation in ('resume', 'recover')
             else client.request(args.operation, args.delta_ns))
    print(json.dumps(reply, indent=2))
