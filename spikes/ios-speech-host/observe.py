"""Run the existing external page-clock probe, then observe ordinary native speech."""
import argparse
from dataclasses import asdict
import json
from pathlib import Path
import shutil
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'webview-timeline'))
from control_probe import main as clock_probe
from native_clock_control import Request
from native_ios_transport import exchange


def main():
    # Reuse its startup, generation, held/read and duplicate/conflict checks unchanged.
    clock_probe()
    parser = argparse.ArgumentParser()
    parser.add_argument('--device', required=True)
    parser.add_argument('--bundle-id', required=True)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    out = args.output
    container = Path(subprocess.check_output(
        ['xcrun', 'simctl', 'get_app_container', args.device, args.bundle_id, 'data'],
        text=True).strip())
    endpoint = json.loads((out/'endpoint.json').read_text())
    previous = json.loads((out/'frame.json').read_text())['after']
    request_id = previous['last_id']
    observations = []
    try:
        # Native wall time only. Reads do not deliver page frames or timers.
        for _ in range(30):
            time.sleep(.5)
            request_id += 1
            request = Request(endpoint['generation'], request_id, 'read', 0)
            reply = exchange(endpoint, request)
            if reply.get('request') != asdict(request) or reply['status'] != 'ok':
                raise RuntimeError('Speech observation lost the page-clock binding')
            speech = container/'Documents/native-speech.jsonl'
            rows = [json.loads(line) for line in speech.read_text().splitlines()] if speech.exists() else []
            observations.append(dict(host_monotonic_s=time.monotonic(), clock=reply,
                                     speech_records=len(rows), last_speech=rows[-1] if rows else None))
            (out/'speech-observations.json').write_text(json.dumps(observations,indent=2)+'\n')
    finally:
        for name in ('native-speech.jsonl','webview-messages.jsonl'):
            source = container/'Documents'/name
            if source.exists():
                shutil.copy2(source,out/name)


if __name__ == '__main__':
    main()
