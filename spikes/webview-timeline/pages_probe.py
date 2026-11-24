"""Opt-in installed page-lifetime/legacy check against pages_fixture.m, not a consumer API."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'tools'))
from native_clock_control import Request, atomic_json
from native_ios_transport import exchange


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', required=True)
    parser.add_argument('--bundle-id', required=True)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--page-registry', action='store_true')
    args = parser.parse_args()
    root = Path(subprocess.check_output(['xcrun', 'simctl', 'get_app_container',
                args.device, args.bundle_id, 'data'], text=True).strip()) / 'Documents'
    history = args.output / 'page-control.jsonl'
    indices, checks = {}, {}
    sequence = 0

    def record(kind, value):
        with history.open('a') as stream:
            stream.write(json.dumps({kind: value}) + '\n')

    def wait(probe, timeout=20):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            value = probe()
            if value:
                return value
            time.sleep(.1)
        raise TimeoutError('Page probe condition timed out')

    def load(path):
        return json.loads(path.read_text()) if path.exists() else None

    def action(action, name):
        nonlocal sequence
        sequence += 1
        command = dict(sequence=sequence, action=action, name=name)
        atomic_json(root / 'page-command.json', command)
        wait(lambda: load(root / 'page-command-result.json') == dict(sequence=sequence))
        record('fixture', command)

    def send(page, request):
        record('request', request.__dict__)
        reply = exchange(page['endpoint'], request)
        record('reply', reply)
        assert reply['request'] == request.__dict__, reply
        return reply

    def call(page, operation='read', delta=0):
        generation = page['endpoint']['generation']
        indices[generation] = indices.get(generation, 0) + 1
        request = Request(generation, indices[generation], operation, delta)
        return request, send(page, request)

    def ready(page):
        def sample():
            _, reply = call(page)
            assert reply['status'] in ('ok', 'not_ready'), reply
            return reply if reply['status'] == 'ok' else None
        return wait(sample)

    def check(name, condition):
        checks[name] = bool(condition)
        atomic_json(args.output / 'checks.json', checks)
        assert condition, name

    def snapshot():
        state = load(root / 'webview-pages.json')
        if state:
            assert type(state['capacity_exceeded']) is bool, state
            assert all(type(p['alive']) is bool and type(p['control_stopped']) is bool
                       for p in state['pages']), state
            record('discovery', state)
        return state

    def page_count(count):
        def sample():
            state = snapshot()
            return state['pages'] if state and len(state['pages']) == count else None
        return wait(sample)

    def retire(page):
        def sample():
            state = snapshot()
            return next((p for p in state['pages'] if p['id'] == page['id']
                         and not p['alive'] and p['control_stopped']), None)
        wait(sample)
        try:
            with socket.create_connection(('127.0.0.1', page['endpoint']['port']), timeout=1):
                closed = False
        except ConnectionRefusedError:
            closed = True
        check('listener_closed_' + page['id'], closed)

    try:
        # The runner selects the mode. Discovery exists before any fixture page.
        multiple = args.page_registry
        if multiple:
            wait(lambda: (root / 'webview-pages.json').exists())
            check('empty_host_inventory', snapshot()['pages'] == [])
        else:
            wait(lambda: root.exists())
        action('create', 'first')
        first = page_count(1)[0] if multiple else dict(endpoint=wait(lambda: load(root / 'clock-control.json')))
        initial = ready(first)
        request, advanced = call(first, 'advance', 100_000_000)
        check('exact_progress', advanced['state']['elapsedMs'] == initial['state']['elapsedMs'] + 100)
        check('original_retry', send(first, request) == dict(advanced, replayed=True))
        conflict = send(first, Request(request.generation, request.request_id, 'advance', 200_000_000))
        check('conflicting_retry_rejected', conflict.get('error') == 'request_conflict')
        _, before = call(first)
        time.sleep(.4)
        _, held = call(first)
        check('held_clock', held['state'] == before['state'])
        action('create', 'second')
        if not multiple:
            _, rejection = call(first)
            check('legacy_second_view_fenced', rejection.get('error') == 'replacement_required')
        else:
            second = page_count(2)[1]
            ready(second)
            _, second_advance = call(second, 'advance', 40_000_000)
            _, first_held = call(first)
            check('independent_frontiers', second_advance['state']['elapsedMs'] == 40
                  and first_held['state'] == held['state'])
            receipt = root / 'webview-pages' / first['id'] / 'clock-control.json'
            original_hash = hashlib.sha256(receipt.read_bytes()).hexdigest()
            action('remove', 'first')
            retire(first)
            action('create', 'third')
            third = page_count(3)[2]
            ready(third)
            check('unique_generations', len({p['endpoint']['generation'] for p in page_count(3)}) == 3)
            wrong = send(third, Request(first['endpoint']['generation'], 1, 'advance', 500_000_000))
            check('old_generation_rejected', wrong.get('error') == 'generation_mismatch')
            _, third_advance = call(third, 'advance', 25_000_000)
            check('replacement_fresh_progress', third_advance['state']['elapsedMs'] == 25)
            action('navigate', 'second')
            def changed_document():
                _, reply = call(second)
                assert reply['status'] in ('ok', 'not_ready', 'rejected'), reply
                return reply if reply.get('error') == 'document_changed' else None
            wait(changed_document)
            _, fenced = call(second, 'advance', 1_000_000)
            check('navigation_fenced', fenced.get('error') == 'replacement_required')
            _, third_held = call(third)
            check('other_page_unaffected', third_held['state'] == third_advance['state'])
            action('remove', 'second')
            action('remove', 'third')
            retire(second)
            retire(third)
            check('original_receipt_unchanged', hashlib.sha256(receipt.read_bytes()).hexdigest() == original_hash)
            check('retired_inventory_preserved', len(page_count(3)) == 3)
        atomic_json(args.output / 'frame.json', dict(passed=True, mode='pages' if multiple else 'legacy', checks=checks))
    finally:
        # Preserve diagnostics even when an assertion fails, before simulator retirement.
        if root.exists():
            shutil.copytree(root, args.output / 'app-documents')


if __name__ == '__main__':
    main()
