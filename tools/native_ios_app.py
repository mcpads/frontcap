"""Drive the installed iOS fixture from the host through XCTest input and native clock control."""
import argparse
from dataclasses import asdict
from functools import cache, partial
import json
from pathlib import Path
import subprocess
import sys
import time

from native_clock_control import Request, atomic_json
from native_ios_control import ClockClient
from native_ios_discovery import app_container
from native_ios_lifecycle import lifecycle, archive
from native_controller_recovery import exercise as controller_recovery
from native_ios_transport import exchange
from native_ios_ui import UIClient


class DiscardedReply(OSError):
    """Fault control: the target responded, but the controller did not receive the outcome."""


def discard_reply(endpoint, request, **options):
    exchange(endpoint, request, **options)
    raise DiscardedReply('Deliberately discarded completed target reply')


def find_ui(simulator, process, out, previous_generation=None, *,
            bundle='dev.frontcap.s18.uitests.xctrunner', proof='native-proof'):
    deadline = time.monotonic() + 60
    last_error = None
    descriptor = None
    rediscover_at = 0
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f'XCTest exited before its UI endpoint: {process.returncode}')
        try:
            if descriptor is None or (previous_generation is not None and time.monotonic() >= rediscover_at):
                descriptor = app_container(simulator, bundle, out) / 'Documents' / proof / 'ui-control.json'
                rediscover_at = time.monotonic() + 1
                if previous_generation is not None:
                    # xcodebuild may reinstall the runner into another data container. The
                    # old installation's cached directory is not a replacement identity.
                    out.mkdir(parents=True, exist_ok=True)
                    with (out/'ui-discovery.jsonl').open('a') as stream:
                        stream.write(json.dumps(dict(descriptor=str(descriptor), previous_generation=previous_generation,
                                                     observed_monotonic_ns=time.monotonic_ns()))+'\n')
            ready = json.loads(descriptor.read_text())
            if previous_generation is not None and ready.get('generation') == previous_generation:
                last_error = 'Waiting for replacement of UI generation '+previous_generation
                time.sleep(.2)
                continue
            if ready.get('status') != 'ready' or ready.get('last_id') != 0:
                raise RuntimeError('UI endpoint is already owned or stale')
            return descriptor
        except (subprocess.CalledProcessError, FileNotFoundError) as error:
            last_error = str(error)
        time.sleep(.2)
    raise TimeoutError('XCTest UI endpoint unavailable: ' + str(last_error))


def scenario(simulator, out, descriptor, hold_us):
    ui_directory, clock_directory = out/'ui-controller', out/'clock-controller'
    ui = UIClient(descriptor, ui_directory)
    # This workflow relaunches one installation; only its endpoint contents change.
    # Scope the successful directory lookup to this invocation, never across installs.
    container = cache(partial(app_container, directory=out))
    lifecycle(simulator, out, ui, container)
    recovery_launch = ui.request('launch')
    recovery_input = ui.request('tap', target='native-increment')
    documents = container(simulator, 'dev.frontcap.s18.probe')/'Documents'
    directory = out/'controller-recovery'
    controller_recovery(directory, [sys.executable, str(Path(__file__).with_name('native_ios_control.py')),
                                   '--descriptor', str(documents/'clock-control.json')],
                        lambda: (documents/'clock-control.json').read_bytes())
    capture = ui.request('capture', name='controller-recovered', clock='Time: 4000000', text='Pending')
    terminated = ui.request('terminate')
    archive(documents, directory/'app')
    atomic_json(directory/'app.json', dict(launch=recovery_launch, input=recovery_input,
                                         capture=capture['result'], terminate=terminated))
    state = dict(controller='python-host', input='xctest-tap', hold_us=hold_us, observations=[], controls=[], inputs=[])
    def save(phase):
        state['progress'] = phase
        atomic_json(out/'host-workflow.json', state)
    save('launch')
    state['launch'] = ui.request('launch')
    # Launch replaces the endpoint generation, not this installation's data directory.
    endpoint = documents/'clock-control.json'
    clock = ClockClient(endpoint, clock_directory)
    state['inputs'].append(ui.request('tap', target='native-increment'))
    phases = ['frozen','before','due','ahead','after-ahead','blocked','recovered','running','running-recovered','drain','drain-recovered']
    for i, phase in enumerate(phases):
        save(phase)
        if i >= 5:
            arguments = dict(target='native-hold-due' if i == 9 else 'native-hold-work')
            if i == 7:
                arguments['label'] = 'Release work'
            if i == 5:
                try:
                    UIClient(descriptor, ui_directory, discard_reply).request('tap', **arguments)
                except DiscardedReply:
                    ui = UIClient(descriptor, ui_directory)
                    tapped = ui.resume()
                else:
                    raise RuntimeError('Tap reply-loss control did not run')
                if tapped.get('replayed') is not True:
                    raise RuntimeError('Tap was not recovered from its cached outcome')
                state['tap_recovery'] = tapped
            else:
                tapped = ui.request('tap', **arguments)
            state['inputs'].append(tapped)
        command = None
        if i:
            delta = 4999000000 if i == 1 else 86400000000000 if i == 3 else 1000000
            if i in (1, 9):
                try:
                    ClockClient(endpoint, clock_directory, discard_reply).request('advance', delta)
                except DiscardedReply:
                    clock = ClockClient(endpoint, clock_directory)
                    command = clock.resume()
                else:
                    raise RuntimeError('Clock reply-loss control did not run')
                state['controls'].append(dict(name='lost-reply' if i==1 else 'lost-drain-reply', reply=command))
            else:
                command = clock.request('advance', delta)
            if i == 1:
                request = command['request']
                rejected = [Request(request['generation'], request['request_id'], 'advance', 1),
                            Request(request['generation'], 1, 'advance', 1),
                            Request('00000000-0000-0000-0000-000000000000', 1, 'advance', 1)]
                for name, attempt in zip(('conflict','retired','generation'), rejected):
                    reply = exchange(json.loads(endpoint.read_text()), attempt)
                    state['controls'].append(dict(name=name, request=asdict(attempt), reply=reply))
            expected_failure = i in (5, 7, 9)
            if (command['status'] != ('failed' if expected_failure else 'ok')
                    or command['applied'] is not (i not in (5, 7))
                    or command['advance_result'] != (60 if i in (5, 7) else 0)
                    or command['drain_result'] != (None if i in (5, 7) else 60 if i == 9 else 0)):
                raise RuntimeError('Unexpected advance/drain outcome: ' + json.dumps(command))
        read = clock.request('read')
        expected_clock = 'Error: 60' if i in (5, 7, 9) else f'Time: {read["now_ns"]}'
        start = time.monotonic()
        time.sleep(hold_us / 1e6)
        held = time.monotonic() - start
        capture = ui.request('capture', name='virtual-'+phase, clock=expected_clock,
                             text='Pending' if i < 2 else 'Count: 1')
        row = dict(capture['result'], control=command, read=read, real_hold_s=held, capture_request=capture['request'])
        state['observations'].append(row)
        save(phase+'-captured')
    overflow = clock.request('advance', 2**63-1)
    after = clock.request('read')
    state['controls'].append(dict(name='overflow', reply=overflow, after=after))
    save('finish')
    state['finish'] = ui.request('finish')
    save('finished')
    atomic_json(out/'virtual-state.json', state)


def run(simulator, out, command, hold_us):
    # Keep xcodebuild in the runner's owned process group. The runner remains the final
    # descendant-cleanup owner, even if the host or xcodebuild exits before its children.
    with (out/'app-test.log').open('wb') as log:
        process = subprocess.Popen(command, stdout=log, stderr=subprocess.STDOUT)
        try:
            descriptor = find_ui(simulator, process, out)
            scenario(simulator, out, descriptor, hold_us)
            result = process.wait(timeout=30)
            if result:
                raise RuntimeError(f'XCTest failed after UI workflow: {result}')
        except BaseException as error:
            try:
                atomic_json(out/'host-workflow-error.json', dict(error=f'{type(error).__name__}: {error}',
                            xcodebuild_pid=process.pid, xcodebuild_exit_code=process.poll()))
            except OSError as evidence_error:
                print(f'Cannot save host failure evidence: {evidence_error}', file=sys.stderr)
            raise
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--simulator', required=True)
    parser.add_argument('--directory', required=True, type=Path)
    parser.add_argument('--hold-us', required=True, type=int)
    parser.add_argument('command', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ['--'] else args.command
    if not command or args.hold_us <= 0:
        parser.error('A positive hold and xcodebuild command are required')
    run(args.simulator, args.directory, command, args.hold_us)
