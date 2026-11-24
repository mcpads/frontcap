"""Observe one actual media player and inspect this simulator's WebKit services."""
import argparse
from dataclasses import asdict
import json
from pathlib import Path
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'tools'))
from native_clock_control import Request
from native_ios_transport import exchange


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--device', required=True)
    parser.add_argument('--bundle-id', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--inspect-debugger', action='store_true',
                        help='Briefly attach LLDB to bound processes, read symbols and detach')
    args = parser.parse_args()
    out = args.output
    container = Path(subprocess.check_output(
        ['xcrun', 'simctl', 'get_app_container', args.device, args.bundle_id, 'data'],
        text=True).strip())
    descriptor = container / 'Documents/clock-control.json'
    deadline = time.monotonic() + 60
    while not descriptor.exists():
        if time.monotonic() > deadline:
            raise TimeoutError('Clock endpoint unavailable')
        time.sleep(.1)
    endpoint = json.loads(descriptor.read_text())
    (out / 'endpoint.json').write_text(json.dumps(endpoint, indent=2))
    index = 0

    def command(operation='read', delta=0):
        nonlocal index
        index += 1
        request = Request(endpoint['generation'], index, operation, delta)
        reply = exchange(endpoint, request)
        with (out / 'media-control.jsonl').open('a') as stream:
            stream.write(json.dumps(dict(request=asdict(request), reply=reply)) + '\n')
        if reply.get('request') != asdict(request):
            raise RuntimeError('Request binding lost')
        return reply

    while True:
        first = command()
        if first['status'] == 'ok' and first['media']['players']:
            break
        if time.monotonic() > deadline:
            raise TimeoutError('No consumer media player observed')
        if first['status'] == 'ok':
            step = command('advance', 50_000_000)
            if step['status'] != 'ok' and step.get('error') != 'speech_preparing':
                raise RuntimeError('Page advance failed: ' + json.dumps(step))
        elif first['status'] != 'not_ready':
            raise RuntimeError('Observation failed: ' + json.dumps(first))
        time.sleep(.02)

    # Keep the bootstrap list as supporting evidence. XPC children may be absent from this list.
    listing = subprocess.run(['xcrun', 'simctl', 'spawn', args.device, 'launchctl', 'list'],
                             capture_output=True, text=True, timeout=20)
    (out / 'simulator-services.txt').write_text(listing.stdout + listing.stderr)
    listing.check_returncode()
    services = []
    # Use identities from the controlled WebView, never sample arbitrary host WebKit processes.
    for label, pid in first.get('media_processes', {}).items():
        if not isinstance(pid, int) or pid <= 0:
            continue
        destination = out / f'webkit-{pid}-sample.txt'
        sampled = subprocess.run(['/usr/bin/sample', str(pid), '1', '-file', str(destination)],
                                 capture_output=True, text=True, timeout=20)
        services.append(dict(pid=pid, label=label, exit_code=sampled.returncode,
                             output=sampled.stdout + sampled.stderr,
                             sample_file=destination.name if destination.exists() else None))
        if args.inspect_debugger:
            report_path = out / f'webkit-{pid}-debugger.json'
            helper = Path(__file__).with_name('media_debugger_probe.py')
            inspected = subprocess.run(['xcrun', 'lldb', '--no-lldbinit', '--batch',
                '-o', 'command script import ' + json.dumps(str(helper)),
                '-o', f'script media_debugger_probe.inspect({pid}, {str(report_path)!r})'],
                capture_output=True, text=True, timeout=60)
            (out / f'webkit-{pid}-debugger.log').write_text(inspected.stdout + inspected.stderr)
            services[-1]['debugger_exit_code'] = inspected.returncode
            services[-1]['debugger'] = json.loads(report_path.read_text()) if report_path.exists() else None
    held = command()
    checks = dict(
        observed_player=bool(first['media']['players']),
        held_page=held['status'] == 'ok' and held['now_ns'] == first['now_ns'],
        same_binding=held.get('pid') == first['pid'] and held.get('document') == first['document'],
        same_media_processes=bool(first.get('media_processes')) and
            held.get('media_processes') == first.get('media_processes'),
        webkit_services_found=bool(services),
        samples_available=bool(services) and all(s['exit_code'] == 0 and s['sample_file'] for s in services),
    )
    if args.inspect_debugger:
        checks['debugger_attached_and_detached'] = bool(services) and all(
            s.get('debugger_exit_code') == 0 and s.get('debugger') and
            s['debugger'].get('attached') and s['debugger'].get('detached') and
            not s['debugger'].get('error') for s in services)
    result = dict(passed=all(checks.values()), checks=checks, first=first, held=held, services=services,
                  scope='Service/process inspection; loaded frameworks do not prove active media backend')
    (out / 'media-process-result.json').write_text(json.dumps(result, indent=2) + '\n')
    (out / 'frame.json').write_text(json.dumps(dict(passed=result['passed'], checks=checks), indent=2))
    if not result['passed']:
        raise RuntimeError('Media process inspection incomplete; inspect retained report')


if __name__ == '__main__':
    main()
