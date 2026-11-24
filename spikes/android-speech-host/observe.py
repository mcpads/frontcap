"""Observe native speech callbacks on one disposable Android emulator."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / 'tools'))
from native_emulator import OwnedEmulator
from native_host_process import write_json
from speech_flow import completed_flow_ids, LIFECYCLE


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--apk', type=Path, required=True)
    parser.add_argument('--component', required=True)
    parser.add_argument('--avd', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--sdk', type=Path, default=Path.home() / 'Library/Android/sdk')
    parser.add_argument('--data-image', type=Path)
    parser.add_argument('--encryption-key', type=Path)
    parser.add_argument('--agent-library', type=Path,
                        help='Optional platform-owned standard TTS boundary observer')
    parser.add_argument('--require-flow', action='store_true',
                        help='Require lifecycle-only observation and unambiguous request/start/completion')
    parser.add_argument('--require-pcm', action='store_true',
                        help='Require actual PCM format/chunk evidence from the observer')
    parser.add_argument('--worker-jar', type=Path,
                        help='Optional platform DEX for separate, silent file preparation')
    parser.add_argument('--hold-speech', action='store_true',
                        help='Qualify hold-worker: real preparation without ordinary playback/completion')
    parser.add_argument('--control-speech', action='store_true',
                        help='After the hold, qualify external cursor advancement and listener delivery')
    parser.add_argument('--cancel-speech-at', type=int, nargs=2, metavar=('X', 'Y'),
                        help='After partial progress, tap the observed cancellation control to stop speech')
    parser.add_argument('--cancel-during-preparation', action='store_true',
                        help='Tap the cancellation control while real synthesis is preparing; require the actual stop phase')
    parser.add_argument('--native-input-jar', type=Path,
                        help='Prestart the existing native contact driver for preparation-stop input')
    parser.add_argument('--speech-utterances', type=int, choices=(1, 2), default=1,
                        help='Qualify two app-requested utterances with the sequential-worker build')
    parser.add_argument('--service-port', type=int, metavar='PORT',
                        help='Forward this emulator TCP port to a local HTTP service')
    args = parser.parse_args()
    if args.service_port is not None and not 1 <= args.service_port <= 65535:
        parser.error('--service-port must be between 1 and 65535')
    if args.require_flow and (not args.agent_library or args.require_pcm or args.worker_jar or args.hold_speech):
        parser.error('--require-flow requires --agent-library and cannot use PCM or worker modes')
    if args.require_pcm and not args.agent_library:
        parser.error('--require-pcm requires --agent-library')
    if args.worker_jar and not args.agent_library:
        parser.error('--worker-jar requires --agent-library')
    if args.hold_speech and (not args.worker_jar or args.require_pcm):
        parser.error('--hold-speech requires --worker-jar and cannot use --require-pcm')
    if args.control_speech and not args.hold_speech:
        parser.error('--control-speech requires --hold-speech')
    if args.cancel_speech_at and not args.control_speech:
        parser.error('--cancel-speech-at requires --control-speech')
    if args.cancel_during_preparation and not args.cancel_speech_at:
        parser.error('--cancel-during-preparation requires --cancel-speech-at')
    if args.native_input_jar and not args.cancel_during_preparation:
        parser.error('--native-input-jar requires --cancel-during-preparation')
    if args.speech_utterances == 2 and (not args.control_speech or args.cancel_speech_at):
        parser.error('--speech-utterances 2 requires control without a cancellation probe')
    apk = args.apk.resolve(strict=True)
    package, separator, activity = args.component.partition('/')
    if not separator or not package or not activity or args.component.startswith('-'):
        parser.error('Expected PACKAGE/ACTIVITY')
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    emulator = OwnedEmulator(args.sdk, out, os.environ.copy(), avd=args.avd,
                             data_image=args.data_image, encryption_key=args.encryption_key)
    lockdir = Path.home() / '.cache/frontcap'
    lockdir.mkdir(parents=True, exist_ok=True)
    fence = lockdir / 'native-session.json'
    native_tap = None

    def adb(*command, required=True):
        result = subprocess.run([str(args.sdk / 'platform-tools/adb'), '-s', emulator.serial,
                                 *command], capture_output=True, timeout=90)
        if required and result.returncode:
            raise RuntimeError(result.stderr.decode(errors='replace'))
        return result

    with (lockdir / 'native-tests.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if fence.exists() and not json.loads(fence.read_text()).get('retired'):
            raise RuntimeError('Previous native owner is not retired')
        write_json(fence, dict(directory=str(out), retired=False))
        try:
            emulator.start()
            emulator.wait_for_boot()
            (out / 'packages.txt').write_bytes(adb('shell', 'pm', 'list', 'packages').stdout)
            if args.service_port:
                port = f'tcp:{args.service_port}'
                adb('reverse', port, port)
            adb('install', '-r', str(apk))
            if args.native_input_jar:
                from speech_control_probe import NativeTap
                native_tap = NativeTap(args.sdk / 'platform-tools/adb', emulator.serial,
                                       args.native_input_jar, out, adb)
            adb('logcat', '-c')
            adb('shell', 'am', 'start', '-n', args.component)
            if args.agent_library:
                deadline = time.monotonic() + 15
                while True:
                    pid = adb('shell', 'pidof', package, required=False).stdout.decode().strip()
                    if pid.isdigit():
                        break
                    if time.monotonic() >= deadline:
                        raise TimeoutError('Target PID unavailable for speech observer')
                    time.sleep(.1)
                adb('push', str(args.agent_library.resolve(strict=True)), '/data/local/tmp/speech-observer.so')
                adb('shell', 'run-as', package, 'mkdir', '-p', 'files')
                adb('shell', 'run-as', package, 'cp', '/data/local/tmp/speech-observer.so',
                    'files/libspeech_observer.so')
                if args.worker_jar:
                    adb('push', str(args.worker_jar.resolve(strict=True)), '/data/local/tmp/speech-worker.jar')
                    adb('shell', 'run-as', package, 'cp', '/data/local/tmp/speech-worker.jar',
                        'files/speech-worker.jar')
                    adb('shell', 'run-as', package, 'chmod', '444', 'files/speech-worker.jar')
                write_json(out / 'agent-intent.json', dict(pid=int(pid), package=package, dispatched=True))
                # One dispatch only. A missing receipt never authorizes reattachment.
                adb('shell', 'am', 'attach-agent', package,
                    f'/data/data/{package}/files/libspeech_observer.so=/data/data/{package}/files')
                current = adb('shell', 'pidof', package).stdout.decode().strip()
                if current != pid:
                    raise RuntimeError('Target replaced during observer attachment')
            deadline = time.monotonic() + 45
            records = []
            hold_started = None
            hold_samples = []
            preparation_stop_dispatched = False
            while time.monotonic() < deadline:
                raw = adb('exec-out', 'run-as', package, 'cat', 'files/native-speech.jsonl', required=False)
                (out / 'native-speech.jsonl').write_bytes(raw.stdout)
                try:
                    records = [json.loads(line) for line in raw.stdout.splitlines() if line]
                except json.JSONDecodeError:
                    # The append may be in progress; retain it and observe again.
                    time.sleep(.1)
                    continue
                if any(row['kind'] in ('done', 'error', 'voice_unavailable') or
                       row.get('state') == 'failed' for row in records):
                    break
                if args.hold_speech:
                    raw_worker = adb('exec-out', 'run-as', package, 'cat',
                                     'files/speech-worker.json', required=False).stdout
                    try:
                        held_worker = json.loads(raw_worker)
                    except json.JSONDecodeError:
                        held_worker = {}
                    if held_worker.get('status') == 'failed':
                        break
                    if args.cancel_during_preparation and not preparation_stop_dispatched and held_worker:
                        if held_worker.get('status') != 'preparing':
                            raise RuntimeError('Preparation finished before stop dispatch; no retry')
                        from speech_control_probe import tap_cancel_control
                        preparation_stop_dispatched = True
                        tap_cancel_control(adb, out, package, args.cancel_speech_at, native_tap)
                    if held_worker.get('status') == 'prepared':
                        now = time.monotonic()
                        if hold_started is None:
                            hold_started = now
                        hold_samples.append(dict(elapsed=now - hold_started,
                            frames=held_worker.get('frames'),
                            callbacks=[row['kind'] for row in records
                                       if row['kind'] in ('playing', 'done', 'stopped', 'error')]))
                        if now - hold_started >= 6:
                            break
                time.sleep(.25)
            if args.hold_speech:
                write_json(out / 'hold-samples.json', hold_samples)
            control_checks = {}
            if args.control_speech:
                from speech_control_probe import qualify
                control_checks = qualify(adb, out, package, args.cancel_speech_at,
                                         args.cancel_during_preparation, args.speech_utterances)
            if args.control_speech:
                deadline = time.monotonic() + 5
                while True:
                    raw = adb('exec-out', 'run-as', package, 'cat', 'files/native-speech.jsonl').stdout
                    (out / 'native-speech.jsonl').write_bytes(raw)
                    records = [json.loads(line) for line in raw.splitlines() if line]
                    terminal = 'stopped' if args.cancel_speech_at else 'done'
                    if sum(row['kind'] == terminal for row in records) >= args.speech_utterances:
                        break
                    if time.monotonic() >= deadline:
                        break
                    time.sleep(.1)
            started = {row.get('utterance_id') for row in records if row['kind'] == 'playing'}
            completed = {row.get('utterance_id') for row in records if row['kind'] == 'done'}
            checks = dict(engine_ready=any(row.get('state') == 'ready' for row in records),
                native_playing_and_done=bool((started & completed) - {None}),
                no_native_error=not any(row['kind'] in ('error', 'voice_unavailable') for row in records))
            if args.hold_speech:
                checks.pop('native_playing_and_done')
                checks['one_native_request'] = sum(row['kind'] == 'speak_requested' for row in records) == 1
                checks['native_callbacks_held'] = bool(hold_samples) and not any(
                    row['kind'] in ('playing', 'done', 'stopped', 'error') for row in records)
                checks['prepared_hold_six_seconds'] = bool(hold_samples) and \
                    hold_samples[-1]['elapsed'] >= 6 and all(not row['callbacks'] for row in hold_samples)
            if args.control_speech:
                checks.update(control_checks)
                checks['native_callbacks_held'] = bool(hold_samples) and all(
                    not row['callbacks'] for row in hold_samples)
                first_id = next((row.get('utterance_id') for row in records
                                 if row['kind'] == 'speak_requested'), None)
                selected = [row for row in records if row.get('utterance_id') == first_id]
                checks['no_native_error'] = first_id is not None and not any(
                    row['kind'] == 'error' for row in selected)
                checks['one_native_request'] = sum(row['kind'] == 'speak_requested'
                                                  for row in selected) == 1
                checks['native_completion_once'] = sum(row['kind'] == 'done' for row in selected) == 1
                if args.speech_utterances == 2:
                    ids = [row['utterance_id'] for row in records if row['kind'] == 'speak_requested'][:2]
                    checks['two_native_completions_once'] = len(set(ids)) == 2 and all(
                        sum(row['kind'] == 'done' and row.get('utterance_id') == identity for row in records) == 1
                        for identity in ids)
                    checks['two_native_requests_without_errors'] = len(set(ids)) == 2 and not any(
                        row['kind'] == 'error' and row.get('utterance_id') in ids for row in records)
                if args.cancel_speech_at:
                    checks.pop('native_completion_once')
                    stops = [row for row in selected if row['kind'] == 'stopped']
                    checks['native_interrupted_once'] = len(stops) == 1 and stops[0].get('interrupted') is True
                    checks['native_done_absent'] = not any(row['kind'] == 'done' for row in selected)
                    if args.cancel_during_preparation:
                        checks.pop('native_interrupted_once')
                        checks['native_flushed_once'] = len(stops) == 1 and stops[0].get('interrupted') is False
                        checks['native_start_absent'] = not any(row['kind'] == 'playing' for row in selected)
                        no_progress = bool(hold_samples) and all(
                            all(kind == 'stopped' for kind in sample['callbacks']) for sample in hold_samples)
                        checks['native_callbacks_held'] = no_progress
                        checks['prepared_hold_six_seconds'] = no_progress and hold_samples[-1]['elapsed'] >= 6
            pcm = None
            worker = None
            if args.worker_jar:
                deadline = time.monotonic() + 25
                while True:
                    raw = adb('exec-out', 'run-as', package, 'cat',
                              'files/speech-worker.json', required=False)
                    if raw.returncode == 0:
                        (out / 'speech-worker.json').write_bytes(raw.stdout)
                        try:
                            worker = json.loads(raw.stdout)
                        except json.JSONDecodeError:
                            worker = None
                        if worker and worker.get('status') in ('prepared', 'failed'):
                            break
                    agent_raw = adb('exec-out', 'run-as', package, 'cat',
                                    'files/speech-observer.jsonl', required=False).stdout
                    try:
                        events = [json.loads(line) for line in agent_raw.splitlines() if line]
                    except json.JSONDecodeError:
                        events = []
                    if any(row.get('event') == 'worker_dispatch' and not row.get('passed')
                           for row in events):
                        break
                    if time.monotonic() >= deadline:
                        break
                    time.sleep(.1)
                checks['worker_prepared'] = bool(worker and worker.get('status') == 'prepared')
                checks['worker_verified_frames'] = bool(worker and worker.get('frames', 0) > 0 and
                    worker.get('pcm_bytes') == worker['frames'] * worker.get('channels', 0) * 2)
                checks['worker_audio_deleted'] = bool(worker and worker.get('audio_deleted')) and \
                    adb('shell', 'run-as', package, 'test', '!', '-e',
                        'files/speech-worker.wav', required=False).returncode == 0
            if args.agent_library:
                raw = adb('exec-out', 'run-as', package, 'cat', 'files/speech-observer.jsonl').stdout
                (out / 'speech-observer.jsonl').write_bytes(raw)
                observed = [json.loads(line) for line in raw.splitlines() if line]
                by_kind = {}
                for row in observed:
                    if row.get('event') == 'boundary' and row.get('identity', 0) > 0:
                        by_kind.setdefault(row['kind'], set()).add(row['identity'])
                checks['observer_ready'] = any(row.get('event') == 'ready' and row.get('pid') == int(pid)
                                               for row in observed)
                checks['standard_speech_callbacks'] = bool(by_kind.get('speak', set()) &
                    by_kind.get('onStart', set()) & by_kind.get('onSuccess', set()))
                flow_mode = any(row.get('event') == 'ready' and row.get('flow_only') is True
                                for row in observed)
                if args.require_flow or flow_mode:
                    checks['flow_observer_mode'] = flow_mode
                    checks['flow_lifecycle_only'] = all(row.get('kind') in LIFECYCLE
                        for row in observed if row.get('event') in ('boundary', 'bind'))
                    checks['standard_speech_callbacks'] = bool(completed_flow_ids(observed))
                if args.hold_speech:
                    checks.pop('standard_speech_callbacks')
                    holds = [row for row in observed if row.get('event') == 'speech_hold']
                    checks['platform_request_held'] = len(holds) == args.speech_utterances and all(
                        row['early_return_error'] == 0 and row['identity'] > 0 for row in holds)
                    checks['ordinary_synthesis_absent'] = not any(row.get('event') == 'boundary' and
                        row.get('kind') in ('onStart', 'onSuccess', 'onError', 'onStop',
                                            'onBeginSynthesis', 'onAudioAvailable') for row in observed)
                checks['observer_no_errors'] = not any(row.get('event') == 'failed' or
                    row.get('observer_exception') or row.get('limit_reached') or
                    row.get('pcm_error') or row.get('pcm_omitted') or
                    (row.get('event') == 'boundary' and row.get('kind') != 'stop' and row.get('local_error'))
                    for row in observed)
                checks['observer_same_pid'] = adb('shell', 'pidof', package).stdout.decode().strip() == pid
                if args.cancel_speech_at:
                    stops = [row for row in observed if row.get('event') == 'controlled_stop']
                    checks['framework_stop_accepted'] = bool(stops) and all(
                        row.get('accepted') and row.get('early_return_error') == 0 for row in stops)
                if args.worker_jar:
                    dispatches = [row for row in observed if row.get('event') == 'worker_dispatch']
                    checks['worker_dispatch_count'] = len(dispatches) == args.speech_utterances and all(
                        row['passed'] for row in dispatches)
                if args.require_pcm:
                    terminal = next((row for row in observed if row.get('kind') == 'onSuccess'
                                     and row.get('identity', 0) > 0), None)
                    utterance = [row for row in observed if terminal and
                                 row.get('identity') == terminal['identity']]
                    formats = [row for row in utterance if row.get('kind') == 'onBeginSynthesis']
                    chunks = [row for row in utterance if row.get('kind') == 'onAudioAvailable']
                    valid_format = len(formats) == 1 and formats[0]['audio_format'] == 2 and \
                        formats[0]['sample_rate'] > 0 and formats[0]['channels'] > 0
                    valid_chunks = bool(chunks) and all(row.get('pcm_bytes', -1) >= 0 and
                        row.get('nonzero_bytes', -1) >= 0 and not row.get('pcm_error') and
                        not row.get('pcm_omitted') for row in chunks)
                    checks['pcm_signed_16bit_format'] = valid_format
                    checks['pcm_complete_order'] = bool(terminal and valid_format and valid_chunks) and all(
                        formats[0]['sequence'] < row['sequence'] < terminal['sequence'] for row in chunks)
                    checks['pcm_nonzero_data'] = valid_chunks and sum(row['nonzero_bytes'] for row in chunks) > 0
                    checks['pcm_frame_alignment'] = bool(valid_format and valid_chunks) and all(
                        row['pcm_bytes'] % (2 * formats[0]['channels']) == 0 for row in chunks)
                    if valid_format and valid_chunks:
                        byte_count = sum(row['pcm_bytes'] for row in chunks)
                        frame_count = byte_count // (2 * formats[0]['channels'])
                        pcm = dict(identity=terminal['identity'], sample_rate=formats[0]['sample_rate'],
                            channels=formats[0]['channels'], audio_format=formats[0]['audio_format'],
                            bytes=byte_count, frames=frame_count, chunks=len(chunks),
                            nonzero_bytes=sum(row['nonzero_bytes'] for row in chunks),
                            duration_seconds=frame_count / formats[0]['sample_rate'],
                            first_chunk_ns=chunks[0]['monotonic_ns'],
                            last_chunk_ns=chunks[-1]['monotonic_ns'],
                            success_ns=terminal['monotonic_ns'])
            (out / 'frame.png').write_bytes(adb('exec-out', 'screencap', '-p').stdout)
            write_json(out / 'result.json', dict(passed=all(checks.values()), checks=checks,
                                               pcm=pcm, worker=worker))
            if not all(checks.values()):
                raise RuntimeError('Ordinary speech qualification failed; inspect result and native log')
        finally:
            try:
                if args.agent_library and emulator.process is not None and emulator.process.poll() is None:
                    captured = adb('exec-out', 'run-as', package, 'cat', 'files/speech-observer.jsonl',
                                   required=False)
                    if captured.returncode == 0:
                        (out / 'speech-observer.jsonl').write_bytes(captured.stdout)
            finally:
                try:
                    if native_tap:
                        native_tap.close()
                finally:
                    cleanup = emulator.retire()
                write_json(out / 'retirement.json', cleanup)
                if cleanup['passed']:
                    write_json(fence, dict(directory=str(out), retired=True))
                else:
                    raise RuntimeError('Owned emulator retirement failed')


if __name__ == '__main__':
    main()
