"""Exercise explicit installed-app controls while native application clocks are paused."""
import json
import os
import sys
import time
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools'))
from native_android_control import ClockClient, Request, exchange


def exercise(out, command, observe, node, center, tap):
    hold_s = int(os.environ.get('FC_HOLD_US', '200000')) / 1e6
    inputs, observations, holds = [], [], []
    active = {}
    client = ClockClient(command, out / "control")
    controls = []

    def save_progress(state='running', error=None):
        record = dict(state=state, active=dict(active), hold_s=hold_s, holds=holds,
                      inputs=inputs, observations=observations, controls=controls, host_updated=time.time())
        if error is not None:
            record['error'] = f'{type(error).__name__}: {error}'
        try:
            temporary = out / 'virtual-progress.tmp'
            temporary.write_text(json.dumps(record, indent=2))
            temporary.replace(out / 'virtual-progress.json')
        except OSError as diagnostic_error:
            print(f'Could not save virtual progress: {diagnostic_error}', file=sys.stderr)

    def action(phase, name):
        active.clear()
        active.update(phase=phase, action=name)
        save_progress()

    def deliver(tree, phase, target):
        action(phase, 'tap:' + target)
        inputs.append(tap(tree, target))
        save_progress()

    def hold(phase):
        action(phase, 'hold')
        started = time.monotonic()
        time.sleep(hold_s)
        holds.append(time.monotonic() - started)
        save_progress()

    def snapshot(phase, count, fired):
        action(phase, 'read-clocks')
        deadline = time.monotonic() + 2
        while True:
            response = client.request('read')
            clocks = response['observation']
            if clocks['count'] == count and clocks['fired'] == fired:
                break
            if time.monotonic() >= deadline:
                raise RuntimeError(f'Fresh clock observation missing: {clocks}')
            time.sleep(.02)
        action(phase, 'observe-screen')
        tree, screen = observe(phase, count)
        screen.update(clocks=clocks, clock_request=response['request'], timer_text=node(tree, 'native-time').get('text'))
        observations.append(screen)
        save_progress()
        return tree

    try:
        tree = snapshot('frozen', 0, 0)
        hold('held')
        tree = snapshot('held', 0, 0)
        deliver(tree, 'input', 'native-increment')
        tree = snapshot('input', 1, 0)
        action('before-deadline', 'advance:4999000000')
        def lose_reply(*args):
            result = command(*args)
            if args[:3] == ('shell', 'am', 'broadcast'):
                raise OSError('Deliberately discarded clock command reply')
            return result
        try:
            ClockClient(lose_reply, out / 'control').request('advance', 4999000000)
        except OSError as error:
            if str(error) != 'Deliberately discarded clock command reply':
                raise
        else:
            raise AssertionError('Reply-loss control did not execute')
        resumed = client.resume()
        controls.append(dict(name='lost-reply', reply=resumed))
        original = Request(**resumed['request'])
        for name, request in [
            ('conflict', Request(original.generation, original.request_id, 'advance', 4999000001)),
            ('retired', Request(original.generation, 1, 'advance', 1)),
            ('generation', Request('00000000-0000-0000-0000-000000000000', original.request_id + 1, 'advance', 1)),
        ]:
            reply = exchange(command, client.package, request, client.evidence)
            controls.append(dict(name=name, reply=reply))
        save_progress()
        try:
            client.request('advance', 2**63 - 1)
        except RuntimeError as error:
            if str(error) != 'Clock command rejected: clock_overflow':
                raise
        else:
            raise AssertionError('Overflowing clock command was accepted')
        controls.append(dict(name='overflow', reply=json.loads(command('shell', 'run-as', client.package,
                                                                      'cat', 'files/clock-control.json'))))
        hold('before-deadline')
        tree = snapshot('before-deadline', 1, 0)
        action('due', 'advance:1000000')
        controls.append(dict(name='deadline', reply=client.request('advance', 1000000)))
        tree = snapshot('due', 1, 1)
        hold('due-held')
        snapshot('due-held', 1, 1)
        (out / 'virtual.json').write_text(json.dumps(dict(hold_s=hold_s, holds=holds, inputs=inputs,
                                                        observations=observations, controls=controls), indent=2))
        active.clear()
        save_progress('collected')  # Acceptance still belongs to the independent checker.
    except BaseException as error:
        save_progress('failed', error)
        raise
