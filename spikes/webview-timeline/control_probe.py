"""Exercise external clock control in an unchanged consumer host.

This probe measures startup and held progress, with optional interactive input.
"""
import argparse
import os
from urllib.parse import urlsplit
from dataclasses import asdict
from dataclasses import dataclass
import json
from pathlib import Path
import subprocess
import sys
import time
import shutil
from review import review

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'tools'))
from native_clock_control import Request
from native_ios_transport import exchange


@dataclass(frozen=True)
class InputRequest:
    generation: str
    request_id: int
    operation: str
    arguments: dict


def native_input(args, command, continuation=None):
    """Attach XCTest and run the default journey or a caller-owned continuation."""
    out = args.output
    plans = list(args.input_build.glob('Build/Products/*.xctestrun'))
    if len(plans) != 1:
        raise RuntimeError('Expected one frozen XCTest build plan')
    log = (out/'input-runner.log').open('w')
    process = subprocess.Popen(['xcodebuild','test-without-building','-xctestrun',str(plans[0]),
        '-destination','platform=iOS Simulator,id='+args.device,
        '-parallel-testing-enabled','NO','-only-testing:WebViewInputTests/WebViewInputTests/testExternalInput',
        '-resultBundlePath',str(out/'input.xcresult')], stdout=log, stderr=subprocess.STDOUT)
    directory = None
    try:
        deadline = time.monotonic()+90
        while True:
            if process.poll() is not None:
                raise RuntimeError('Input runner exited before attachment')
            discovery = subprocess.run(['xcrun','simctl','get_app_container',args.device,
                'dev.frontcap.webview.input.tests.xctrunner','data'],capture_output=True,text=True)
            if discovery.returncode == 0:
                directory = Path(discovery.stdout.strip())/'Documents/webview-input'
                descriptor = directory/'ui-control.json'
                if descriptor.exists():
                    endpoint = json.loads(descriptor.read_text())
                    break
            if time.monotonic()>deadline:
                raise TimeoutError('Input runner did not publish an endpoint')
            time.sleep(.2)
        index = 0
        def ui(operation, arguments=None):
            nonlocal index
            index += 1
            request = InputRequest(endpoint['generation'],index,operation,arguments or {})
            (out/f'input-{index}.request.json').write_text(json.dumps(asdict(request),indent=2))
            reply = exchange(endpoint,request,timeout=30)
            (out/f'input-{index}.reply.json').write_text(json.dumps(reply,indent=2))
            if reply.get('status') != 'ok':
                raise RuntimeError('Input request failed')
            shutil.copy2(directory/'last.png',out/f'input-{index}.png')
            shutil.copy2(directory/'last-tree.txt',out/f'input-{index}.tree.txt')
            return reply
        ui('launch')
        if continuation is not None:
            result = continuation(ui, process)
            ui('finish')
            process.wait(timeout=30)
            if process.returncode:
                raise RuntimeError('XCTest failed')
            return result
        result = review(args, command, ui, process)
        ui('finish')
        process.wait(timeout=30)
        if process.returncode:
            raise RuntimeError('XCTest failed')
        return result
    finally:
        try:
            if directory and directory.exists():
                diagnostics = out/'input-runner-records'
                diagnostics.mkdir(exist_ok=True)
                for path in directory.iterdir():
                    if path.is_file():
                        shutil.copy2(path,diagnostics/path.name)
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=5)
            log.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', required=True)
    parser.add_argument('--bundle-id', required=True)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--input-build', type=Path)
    parser.add_argument('--review-actions', action='store_true')
    parser.add_argument('--startup-steps', type=int, default=80)
    args = parser.parse_args()
    if args.review_actions and not args.input_build:
        parser.error('--review-actions requires --input-build')
    if not 1 <= args.startup_steps <= 600:
        parser.error('--startup-steps must be between 1 and 600')
    out = args.output
    container = Path(subprocess.check_output(
        ['xcrun', 'simctl', 'get_app_container', args.device, args.bundle_id, 'data'], text=True).strip())
    descriptor = container / 'Documents/clock-control.json'
    deadline = time.monotonic() + 60
    while not descriptor.exists():
        if time.monotonic() > deadline:
            raise TimeoutError('External WebView endpoint unavailable')
        time.sleep(.1)
    endpoint = json.loads(descriptor.read_text())
    (out / 'endpoint.json').write_text(json.dumps(endpoint, indent=2))
    index = 0
    history = out / 'control.jsonl'

    def send(request):
        with history.open('a') as stream:
            stream.write(json.dumps({'request':asdict(request)})+'\n')
        response = exchange(endpoint, request)
        with history.open('a') as stream:
            stream.write(json.dumps({'reply':response})+'\n')
        if response.get('request') != asdict(request):
            raise RuntimeError('Mismatched reply')
        return response

    def command(operation='read', delta_ns=0):
        nonlocal index
        index += 1
        request = Request(endpoint['generation'], index, operation, delta_ns)
        return request, send(request)

    while True:
        _, first = command()
        if first['status'] == 'ok':
            break
        if first['status'] != 'not_ready' or time.monotonic() > deadline:
            raise RuntimeError('Document not ready: '+json.dumps(first))
        time.sleep(.1)
    for _ in range(args.startup_steps):
        _, current = command('advance', 50_000_000)
        if current['status'] != 'ok' or current['state']['failure']:
            raise RuntimeError('Startup advance failed: '+json.dumps(current))
    subprocess.run(['xcrun','simctl','io',args.device,'screenshot',str(out/'startup.png')],check=True)
    _, before = command()
    time.sleep(.7)
    _, held = command()
    subprocess.run(['xcrun','simctl','io',args.device,'screenshot',str(out/'held.png')],check=True)
    request, advanced = command('advance', 100_000_000)
    replay = send(request)
    conflict = send(Request(request.generation, request.request_id, 'advance', 200_000_000))
    _, after = command()
    checks = {
        'stable_document':first['document'] == after['document'],
        'held':before['state'] == held['state'],
        'exact_advance':after['state']['elapsedMs'] == before['state']['elapsedMs'] + 100,
        'duplicate_preserved':replay == dict(advanced, replayed=True),
        'conflict_rejected':conflict.get('error') == 'request_conflict',
        'healthy':not after['state']['failure'],
    }
    expected_origin = os.environ.get('FRONTCAP_CONSUMER_ORIGIN')
    if expected_origin:
        checks['consumer_document'] = urlsplit(first['url'])[:2] == urlsplit(expected_origin)[:2]
    if args.review_actions:
        checks.update(native_input(args, command))
    result = dict(passed=all(checks.values()), checks=checks, first=first, before=before, after=after,
                  startup_steps=args.startup_steps, startup_delta_ms=50)
    (out/'frame.json').write_text(json.dumps(result, indent=2)+'\n')
    if not result['passed']:
        raise RuntimeError('External control checks failed')


if __name__ == '__main__':
    main()
