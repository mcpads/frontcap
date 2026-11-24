"""Register a fixture input and recover its actual click receipt without repeating a tap."""
import argparse
from dataclasses import asdict, dataclass
import fcntl
import json
from pathlib import Path
import re
import subprocess
import time
import uuid
from typing import ClassVar

from native_clock_control import require_bound_request, atomic_json, recorded_outcome
from native_controller_checkpoint import checkpoint


@dataclass(frozen=True)
class InputRequest:
    tap_targets: ClassVar[tuple[str, ...]] = ('native-increment',)
    generation: str
    controller_id: str
    request_id: int
    operation: str
    target: str
    x: int
    y: int

    def expected_delivery(self):
        return 'android-onclick'

    def __post_init__(self):
        for value in (self.generation, self.controller_id):
            if str(uuid.UUID(value)) != value:
                raise ValueError('Invalid input identity')
        if (type(self.request_id) is not int or not 0 < self.request_id < 2**63
                or self.operation != 'tap' or self.target not in self.tap_targets
                or any(type(v) is not int or not 0 < v < 2**31 for v in (self.x, self.y))):
            raise ValueError('Invalid input request')


def read_receipt(command, package, **options):
    return json.loads(command('shell', 'run-as', package, 'cat', 'files/input-control.json', **options))


def register_request(command, package, request):
    tail = []
    for name, value in asdict(request).items():
        tail.extend(['--el' if name == 'request_id' else '--ei' if name in ('x', 'y') else '--es', name, str(value)])
    return command('shell', 'am', 'broadcast', '--receiver-foreground', '-a', package+'.INPUT_CONTROL',
                   '-p', package, *tail).decode(errors='replace')


def validate_reply(reply, request):
    if (reply.get('status') != 'ok' or reply.get('request') != asdict(request)
            or reply.get('generation') != request.generation or type(reply.get('last_id')) is not int
            or reply['last_id'] != request.request_id or reply.get('delivery') != request.expected_delivery()
            or reply.get('target') != request.target or type(reply.get('pid')) is not int or reply['pid'] <= 0
            or type(reply.get('clicks')) is not int or reply['clicks'] != request.request_id
            or not isinstance(reply.get('observation'), dict)):
        raise RuntimeError('Input outcome unavailable; retain the pending request without another tap')


def focused_component(window):
    focus = re.findall(r'^\s*mCurrentFocus=(.*)$', window.decode(), re.MULTILINE)
    components = re.findall(r'([A-Za-z_][A-Za-z_0-9.]*)/([A-Za-z_.$][A-Za-z_0-9.$]*)', focus[0]) if len(focus) == 1 else []
    components = [package+'/'+(package+activity if activity.startswith('.') else activity)
                  for package, activity in components]
    return components[0] if len(components) == 1 else None


def require_uncovered_tap(window, x, y):
    """Reject known IME overlap; this snapshot is not a general hit-test guarantee."""
    for line in window.decode().splitlines():
        if not re.search(r'\btype=ime\b', line) or not re.search(r'\bvisible=true\b', line):
            continue
        frame = re.search(r'\bframe=\[(-?\d+),(-?\d+)\]\[(-?\d+),(-?\d+)\]', line)
        if frame is None:
            raise RuntimeError('Visible keyboard bounds unavailable; retain the prepared request')
        left, top, right, bottom = map(int, frame.groups())
        if right < left or bottom < top:
            raise RuntimeError('Invalid visible keyboard bounds; retain the prepared request')
        if left <= x < right and top <= y < bottom:
            raise RuntimeError('Tap coordinate covered by visible keyboard; retain the prepared request')


class InputClient:
    """One input owner per Activity generation. Dispatched intent is never retransmitted."""
    registration_transport = 'broadcast'

    def __init__(self, command, directory, package='dev.frontcap.s18.probe', *, receipt_timeout=2,
                 activity='dev.frontcap.s18.VirtualActivity', request_type=InputRequest):
        if not re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]*(?:\.[A-Za-z_][A-Za-z_0-9]*)+', package):
            raise ValueError('Invalid Android package')
        if not re.fullmatch(r'(?:[A-Za-z_$][A-Za-z_0-9$]*\.)*[A-Za-z_$][A-Za-z_0-9$]*', activity):
            raise ValueError('Invalid Android Activity')
        self.component = package+'/'+activity
        self.request_type = request_type
        self.command, self.directory, self.package = command, Path(directory), package
        self.receipt_timeout = receipt_timeout
        self.directory.mkdir(parents=True, exist_ok=True)
        self.path = self.directory/'input-client.json'

    def _read_receipt(self, **options):
        return read_receipt(self.command, self.package, **options)

    def _register_request(self, request):
        return register_request(self.command, self.package, request)

    def _make_request(self, state, target, x, y):
        return self.request_type(state['generation'], state['controller_id'], state['next_id'], 'tap', target, x, y)

    def _dispatch(self, request):
        self.command('shell', 'input', 'tap', str(request.x), str(request.y))

    def _validate_reply(self, reply, request):
        validate_reply(reply, request)

    def _ready(self, receipt):
        return receipt.get('status') == 'ready' and receipt.get('last_id') == 0 and receipt.get('clicks') == 0

    def _current(self, state, **options):
        current = self._read_receipt(**options)
        if current.get('generation') != state['generation'] or current.get('pid') != state['pid']:
            raise RuntimeError('Target generation changed; old input remains unresolved')
        return current

    def _await_receipt(self, state, request, row, current=None):
        deadline = time.monotonic()+self.receipt_timeout
        label = 'Text' if request.operation == 'text' else 'Input'
        row['receipt_timeout_s'] = self.receipt_timeout
        row['receipt_deadline_monotonic_s'] = deadline
        row['receipt_started_monotonic_ns'] = time.monotonic_ns()
        while True:
            if current is None:
                remaining = deadline-time.monotonic()
                if remaining <= 0:
                    raise RuntimeError(f'{label} outcome unavailable: receipt deadline elapsed; retain pending request')
                attempt = dict(started_monotonic_s=time.monotonic(), timeout_s=remaining)
                row.setdefault('receipt_reads', []).append(attempt)
                try:
                    current = self._read_receipt(timeout=remaining)
                    attempt['receipt'] = current
                except subprocess.TimeoutExpired as error:
                    attempt['error'] = row['receipt_error'] = f'{type(error).__name__}: {error}'
                    partial = error.output
                    row['receipt_partial_output'] = partial.decode(errors='replace') if isinstance(partial, bytes) else partial
                    raise RuntimeError(f'{label} outcome unavailable: receipt transport exceeded deadline; retain pending request') from error
                except BaseException as error:
                    attempt['error'] = f'{type(error).__name__}: {error}'
                    raise
                finally:
                    attempt['finished_monotonic_s'] = time.monotonic()
                row['reply'] = current
                if current.get('generation') != state['generation'] or current.get('pid') != state['pid']:
                    raise RuntimeError('Target generation changed; old input remains unresolved')
                if time.monotonic() > deadline:
                    raise RuntimeError(f'{label} outcome unavailable: receipt arrived after deadline; retain pending request')
            if current.get('request') != asdict(request) or current.get('status') not in ('armed', 'executing'):
                return current
            remaining = deadline-time.monotonic()
            if remaining <= 0:
                raise RuntimeError(f'{label} outcome unavailable: receipt deadline elapsed; retain pending request')
            time.sleep(min(.02, remaining))
            current = None

    def _register(self, state, request, row):
        output = self._register_request(request)
        row['registration'] = output
        expected = f'Broadcast completed: result=-1, data="{request.generation}:{request.request_id}"'
        if expected not in output.splitlines():
            raise RuntimeError('Input registration unconfirmed; retain the prepared request')
        current = self._current(state)
        if (current.get('request') != asdict(request) or current.get('status') not in ('armed', 'ok')
                or current.get('last_id') != request.request_id - (current.get('status') == 'armed')):
            raise RuntimeError('Input registration receipt does not match the request')
        return current

    def _focus(self, request):
        window = self.command('shell', 'dumpsys', 'window')
        (self.directory/f'input-{request.request_id}-window.txt').write_bytes(window)
        focus = focused_component(window)
        if focus != self.component:
            raise RuntimeError(f'Fixture does not own input focus: {focus}')
        if request.operation == 'tap':
            require_uncovered_tap(window, request.x, request.y)

    def _load_state(self, *, controller_id=None):
        if controller_id is not None and (type(controller_id) is not str or str(uuid.UUID(controller_id)) != controller_id):
            raise ValueError('Invalid input controller identity')
        if self.path.exists():
            state = json.loads(self.path.read_text())
            if controller_id is not None and state.get('controller_id') != controller_id:
                raise RuntimeError('Input controller changed; retain the original client')
        else:
            ready = self._read_receipt()
            if not self._ready(ready):
                raise RuntimeError('Target already has input history; use its retained client directory')
            state = dict(generation=ready['generation'], pid=ready['pid'], controller_id=controller_id or str(uuid.uuid4()),
                         next_id=1, pending=None, delivery_phase=None, component=self.component)
            if self.registration_transport != 'broadcast':
                state['registration_transport'] = self.registration_transport
            atomic_json(self.path, state)
        if state.get('registration_transport', 'broadcast') != self.registration_transport:
            raise RuntimeError('Input registration transport changed; retain the original client')
        # Older probe records predate component binding and belong only to that probe.
        component = state.get('component', 'dev.frontcap.s18.probe/dev.frontcap.s18.VirtualActivity')
        if component != self.component:
            raise RuntimeError('Input Activity changed; retain the original client and pending request')
        return state

    def initialize(self, *, controller_id=None):
        """Persist a stable controller identity without registering or dispatching input."""
        with (self.directory/'input-client.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            state = self._load_state(controller_id=controller_id)
            self._current(state)
            return state

    def _perform(self, target=None, x=None, y=None, *, receipt_only=False, expected_request=None):
        row = dict(started_monotonic_ns=time.monotonic_ns(), receipt_only=receipt_only)
        with (self.directory/'input-client.lock').open('a') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            try:
                state = self._load_state()
                if target is None:
                    if not state['pending']:
                        raise RuntimeError('No pending input command')
                    request = self.request_type(**state['pending'])
                    require_bound_request(request, expected_request)
                else:
                    if state['pending']:
                        raise RuntimeError('Pending input command; resolve it before another tap')
                    request = self._make_request(state, target, x, y)
                    require_bound_request(request, expected_request)
                    checkpoint(self.directory, 'before-pending', request, operation=request.operation)
                    state.update(pending=asdict(request), delivery_phase='prepared')
                    atomic_json(self.path, state)
                    checkpoint(self.directory, 'after-pending', request, operation=request.operation)
                row.update(request=asdict(request), delivery_phase=state['delivery_phase'])
                if state['delivery_phase'] not in ('prepared', 'dispatched'):
                    raise RuntimeError('Invalid retained input phase')
                current = self._current(state) if state['delivery_phase'] == 'prepared' else None
                if state['delivery_phase'] == 'prepared' and not receipt_only:
                    current = self._register(state, request, row)
                    checkpoint(self.directory, 'after-registration', request, operation=request.operation)
                    if current['status'] == 'armed':
                        self._focus(request)
                        # Persist BEFORE invoking input. Death in this gap retains uncertainty.
                        state['delivery_phase'] = 'dispatched'
                        atomic_json(self.path, state)
                        checkpoint(self.directory, 'after-dispatch', request, operation=request.operation)
                        row[request.operation+'_started_monotonic_ns'] = time.monotonic_ns()
                        self._dispatch(request)
                        row[request.operation+'_finished_monotonic_ns'] = time.monotonic_ns()
                        current = None
                current = self._await_receipt(state, request, row, current)
                row['reply'] = current
                self._validate_reply(current, request)
                checkpoint(self.directory, 'after-reply', request, operation=request.operation)
                state.update(next_id=request.request_id+1, pending=None, delivery_phase=None, last_reply=current)
                atomic_json(self.path, state)
                checkpoint(self.directory, 'after-commit', request, operation=request.operation)
                return current
            except BaseException as error:
                row['error'] = f'{type(error).__name__}: {error}'
                raise
            finally:
                row['finished_monotonic_ns'] = time.monotonic_ns()
                with (self.directory/'input-commands.jsonl').open('a') as stream:
                    stream.write(json.dumps(row)+'\n')

    def tap(self, target, x, y, *, expected_request=None):
        return self._perform(target, x, y, expected_request=expected_request)

    def resume(self, *, expected_request=None):
        return self._perform(expected_request=expected_request)

    def recover(self, *, expected_request=None):
        return self._perform(receipt_only=True, expected_request=expected_request)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--adb', required=True)
    parser.add_argument('--serial', required=True)
    parser.add_argument('--directory', required=True, type=Path)
    parser.add_argument('--package', default='dev.frontcap.s18.probe')
    parser.add_argument('--record-tap-process', action='store_true',
                        help='Record the actual adb tap lifetime for interruption diagnostics')
    parser.add_argument('--tap-start-suspended', action='store_true',
                        help='Diagnostic Darwin launch of the actual tap child before user-code execution')
    parser.add_argument('--tap-guest-gate', nargs=2, metavar=('EXECUTABLE','DIRECTORY'),
                        help='Diagnostic owned guest gate before exec of the input command')
    parser.add_argument('operation', choices=['tap', 'resume', 'recover', 'outcome'])
    parser.add_argument('--target', default='native-increment')
    parser.add_argument('--x', type=int)
    parser.add_argument('--y', type=int)
    args = parser.parse_args()
    if args.tap_start_suspended and not args.record_tap_process:
        parser.error('--tap-start-suspended requires --record-tap-process')
    if args.tap_guest_gate and (not args.record_tap_process or args.tap_start_suspended):
        parser.error('--tap-guest-gate requires --record-tap-process and excludes --tap-start-suspended')
    if args.operation == 'tap' and (args.x is None or args.y is None):
        parser.error('tap requires --x and --y')
    if args.operation != 'tap' and (args.x is not None or args.y is not None):
        parser.error('coordinates are only valid for tap')
    if args.operation == 'outcome':
        reply = recorded_outcome(args.directory, validate_reply, request_type=InputRequest, prefix='input')
    else:
        def command(*tail, timeout=20):
            if args.record_tap_process and tail[:3] == ('shell', 'input', 'tap'):
                from native_android_tap import run_tap, run_suspended_tap
                if args.tap_guest_gate:
                    from native_android_guest_input import gate_command
                    request=json.loads((args.directory/'input-client.json').read_text())['pending']
                    argv=gate_command([args.adb,'-s',args.serial],*args.tap_guest_gate,request)
                    return run_tap(argv,args.directory,record_output=True)
                transport = run_suspended_tap if args.tap_start_suspended else run_tap
                return transport([args.adb, '-s', args.serial, *tail], args.directory)
            return subprocess.check_output([args.adb, '-s', args.serial, *tail], timeout=timeout)
        client = InputClient(command, args.directory, args.package)
        reply = client.tap(args.target, args.x, args.y) if args.operation == 'tap' else getattr(client, args.operation)()
    print(json.dumps(reply, indent=2))
