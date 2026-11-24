"""Focused installed proof for the experimental muted speech queue."""
import argparse
from dataclasses import asdict
import json
from pathlib import Path
import shutil
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
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--utterances', type=int, choices=(1, 2), default=1,
                        help='Optionally follow the consumer into its next automatic utterance')
    args = parser.parse_args()
    out = args.output
    container = Path(subprocess.check_output(
        ['xcrun', 'simctl', 'get_app_container', args.device, args.bundle_id, 'data'], text=True).strip())
    descriptor = container / 'Documents/clock-control.json'
    deadline = time.monotonic() + 60
    while not descriptor.exists():
        if time.monotonic() > deadline:
            raise TimeoutError('Speech clock endpoint unavailable')
        time.sleep(.1)
    endpoint = json.loads(descriptor.read_text())
    (out/'endpoint.json').write_text(json.dumps(endpoint, indent=2)+'\n')
    index = 0

    def send(request):
        reply = exchange(endpoint, request)
        with (out/'speech-control.jsonl').open('a') as stream:
            stream.write(json.dumps(dict(request=asdict(request), reply=reply))+'\n')
        if reply.get('request') != asdict(request):
            raise RuntimeError('Request binding lost')
        return reply

    def command(operation='read', delta=0):
        nonlocal index
        index += 1
        request = Request(endpoint['generation'], index, operation, delta)
        return request, send(request)

    def read():
        _, reply = command()
        if reply['status'] != 'ok':
            raise RuntimeError('Read failed: '+json.dumps(reply))
        return reply

    try:
        while True:
            _, reply = command()
            if reply['status'] == 'ok':
                break
            if reply['status'] != 'not_ready' or time.monotonic() > deadline:
                raise RuntimeError('Consumer unavailable')
            time.sleep(.05)
        for _ in range(100):
            _, reply = command('advance', 50_000_000)
            if reply['status'] == 'rejected' and reply.get('error') == 'speech_preparing':
                break
            if reply['status'] != 'ok':
                raise RuntimeError('Startup advance failed: '+json.dumps(reply))
            if reply['speech']['queued']:
                break
        deadline = time.monotonic() + 15
        while True:
            ready = read()
            if ready['speech']['queued'] == 1 and ready['speech']['phase'] == 1:
                break
            if time.monotonic() > deadline:
                raise TimeoutError('Speech did not prepare')
            time.sleep(.05)
        _, started = command('advance', 0)
        if started['status'] != 'ok':
            raise RuntimeError('Cannot admit speech start')
        held = []
        for _ in range(24):
            time.sleep(.25)
            held.append(read())
        speech = held[-1]['speech']
        units = (speech['frames'] - speech['frame']) * 1_000_000_000 - speech['fraction']
        remaining = (units + speech['sample_rate'] - 1) // speech['sample_rate']
        if remaining < 2:
            raise RuntimeError('Insufficient speech boundary for diagnostic')
        _, rejected = command('advance', remaining + 1)
        after_rejection = read()
        _, before_end = command('advance', remaining - 1)
        time.sleep(.4)
        before_end_held = read()
        final_request, finished = command('advance', 1)
        replay = send(final_request)
        delivered = read()
        checks = dict(
            queue_available=started['speech']['available'],
            held_cursor=all(item['speech'] == started['speech'] for item in held),
            held_page=all(item['state'] == started['state'] for item in held),
            no_host_completion=all(item['speech']['completed'] == 0 for item in held),
            crossing_rejected=rejected.get('error') == 'speech_boundary' and not rejected['applied'],
            rejection_unchanged=after_rejection['speech'] == speech and
                after_rejection['state'] == held[-1]['state'],
            one_ns_before=before_end['status'] == 'ok' and before_end_held['speech']['queued'] == 1
                and before_end_held['speech']['completed'] == 0,
            exact_completion=finished['status'] == 'ok' and finished['speech']['completed'] == 1
                and finished['speech']['queued'] == 0,
            duplicate_preserved=replay == dict(finished, replayed=True),
            same_binding=all(item['document'] == started['document'] and item['pid'] == started['pid']
                for item in [*held, before_end_held, delivered]),
        )
        following = None
        if args.utterances == 2:
            # Advance the actual consumer's transition; do not synthesize a new app request.
            for _ in range(160):
                following = read()
                if following['speech']['accepted'] >= 2:
                    break
                _, transition = command('advance', 50_000_000)
                if transition['status'] != 'ok' and transition.get('error') != 'speech_preparing':
                    raise RuntimeError('Next-utterance transition failed: '+json.dumps(transition))
                time.sleep(.02)
            else:
                raise TimeoutError('Consumer did not request the next utterance')
            first_following = following
            deadline = time.monotonic() + 15
            while following['speech']['phase'] == 0:
                if time.monotonic() > deadline:
                    raise TimeoutError('Next utterance did not prepare')
                time.sleep(.05)
                following = read()
            _, second_start = command('advance', 0)
            if second_start['status'] != 'ok':
                raise RuntimeError('Next utterance start was not admitted')
            second_held = []
            for _ in range(4):
                time.sleep(.25)
                second_held.append(read())
            head = second_held[-1]['speech']
            units = (head['frames'] - head['frame']) * 1_000_000_000 - head['fraction']
            next_remaining = (units + head['sample_rate'] - 1) // head['sample_rate']
            if next_remaining < 2:
                raise RuntimeError('Next utterance has no remaining diagnostic interval')
            _, next_before = command('advance', next_remaining - 1)
            time.sleep(.2)
            next_before_held = read()
            next_request, next_finished = command('advance', 1)
            next_replay = send(next_request)
            next_delivered = read()
            checks.update(
                next_identity=head['utterance_id'] != started['speech']['utterance_id'],
                next_starts_at_zero=first_following['speech']['frame'] == 0 and
                    first_following['speech']['fraction'] == 0,
                next_held=all(item['speech'] == second_start['speech'] for item in second_held),
                next_before_boundary=next_before['status'] == 'ok' and
                    next_before_held['speech']['completed'] == 1 and next_before_held['speech']['queued'] == 1,
                next_completed=next_finished['status'] == 'ok' and next_finished['speech']['completed'] == 2
                    and next_finished['speech']['queued'] == 0,
                next_replay_preserved=next_replay == dict(next_finished, replayed=True),
                next_same_binding=next_delivered['document'] == delivered['document']
                    and next_delivered['pid'] == delivered['pid'],
            )
            following = dict(first=first_following, started=second_start, held=second_held[-1],
                             remaining_ns=next_remaining, before_end=next_before_held,
                             finished=next_finished, delivered=next_delivered)
        result = dict(passed=all(checks.values()), checks=checks, remaining_ns=remaining,
                      started=started, held=held[-1], rejected=rejected,
                      before_end=before_end_held, finished=finished, delivered=delivered,
                      following=following)
        (out/'speech-result.json').write_text(json.dumps(result, indent=2)+'\n')
        (out/'frame.json').write_text(json.dumps(dict(passed=result['passed'], checks=checks), indent=2)+'\n')
        if not result['passed']:
            raise RuntimeError('Controlled speech checks failed: '+json.dumps(checks))
    finally:
        for name in ('native-speech.jsonl', 'webview-messages.jsonl'):
            path = container/'Documents'/name
            if path.exists():
                shutil.copy2(path, out/name)


if __name__ == '__main__':
    main()
