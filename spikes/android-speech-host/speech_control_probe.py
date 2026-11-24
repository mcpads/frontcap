"""Focused external controller for the one-utterance Android speech experiment."""
import json
import struct
import select
import subprocess
import time


class NativeTap:
    """One owned, prestarted shell-UID contact driver; never retries an uncertain event."""
    def __init__(self, executable, serial, jar, out, adb):
        self.out = out
        self.sequence = 0
        self.revision = 0
        self.events = []
        adb('push', str(jar.resolve(strict=True)), '/data/local/tmp/speech-touch.jar')
        self.log = (out / 'native-tap-stderr.txt').open('w')
        self.process = subprocess.Popen([str(executable), '-s', serial, 'shell', '-T',
            'CLASSPATH=/data/local/tmp/speech-touch.jar app_process /system/bin dev.frontcap.input.TouchInput'],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log, text=True, bufsize=1)
        try:
            self.hello = self._read()
            if self.hello.get('status') != 'ready' or self.hello.get('uid') != 2000:
                raise RuntimeError('Native contact driver not ready')
        except Exception:
            self.close()
            raise

    def _read(self):
        if not select.select([self.process.stdout], [], [], 5)[0]:
            raise TimeoutError('Native contact reply unavailable; no redispatch')
        value = json.loads(self.process.stdout.readline())
        self.events.append(value)
        (self.out / 'native-tap-replies.json').write_text(json.dumps(self.events, indent=2) + '\n')
        return value

    def _send(self, payload):
        self.sequence += 1
        command = dict(generation=self.hello['generation'], request_id=self.sequence,
                       expected_revision=self.revision, **payload)
        self.process.stdin.write(json.dumps(command) + '\n')
        self.process.stdin.flush()
        reply = self._read()
        expected = 'finished' if payload.get('finish') else 'injected'
        if reply.get('request_id') != self.sequence or reply.get('generation') != self.hello['generation'] or \
            reply.get('contact_revision') != self.revision + 1 or reply.get('status') != expected:
            raise RuntimeError('Native contact operation not confirmed; no redispatch')
        self.revision += 1

    def tap(self, x, y):
        self._send(dict(contacts=[dict(id=1, x=x, y=y)]))
        self._send(dict(contacts=[]))
        self._send(dict(finish=True))
        if self.process.wait(timeout=5) != 0:
            raise RuntimeError('Native contact driver failed after input')

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            try:
                self.process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=2)
        self.process.stdout.close()
        self.log.close()


def tap_cancel_control(adb, out, package, cancel_at, native_tap=None):
    image = adb('exec-out', 'screencap', '-p').stdout
    (out / 'before-stop.png').write_bytes(image)
    width, height = struct.unpack('>II', image[16:24])
    x, y = cancel_at
    if not (0 <= x < width and 0 <= y < height):
        raise ValueError('Stop tap outside captured display')
    focus = adb('shell', 'dumpsys', 'window').stdout.decode(errors='replace')
    (out / 'stop-focus.txt').write_text(focus)
    if not any('mCurrentFocus=' in line and package in line for line in focus.splitlines()):
        raise RuntimeError('Consumer does not own focused window')
    (out / 'stop-input.json').write_text(json.dumps(dict(x=x, y=y, width=width, height=height,
        dispatched=True, method='persistent native contacts' if native_tap else 'adb input tap')) + '\n')
    if native_tap:
        native_tap.tap(x, y)
    else:
        adb('shell', 'input', 'tap', str(x), str(y))


def qualify(adb, out, package, cancel_at=None, during_preparation=False, utterances=1):
    def read_file(name):
        raw = adb('exec-out', 'run-as', package, 'cat', 'files/' + name).stdout
        return json.loads(raw)

    initial = read_file('speech-control.json')
    token = initial['token']
    exchanges = []
    workers = [read_file('speech-worker.json')]

    def request(identity, operation, delta=0, request_token=None):
        command = dict(token=token if request_token is None else request_token,
                       request=identity, op=operation, delta_ns=delta)
        path = out / 'speech-command.json'
        path.write_text(json.dumps(command) + '\n')
        adb('push', str(path), '/data/local/tmp/speech-command.json')
        adb('shell', 'run-as', package, 'rm', '-f', 'files/speech-response.json')
        adb('shell', 'run-as', package, 'cp', '/data/local/tmp/speech-command.json',
            'files/speech-request.tmp')
        adb('shell', 'run-as', package, 'mv', 'files/speech-request.tmp', 'files/speech-request.json')
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            raw = adb('exec-out', 'run-as', package, 'cat',
                      'files/speech-response.json', required=False).stdout
            try:
                response = json.loads(raw)
            except json.JSONDecodeError:
                time.sleep(.05)
                continue
            if response.get('request') == identity:
                exchanges.append(dict(command=command, response=response))
                (out / 'speech-exchanges.json').write_text(json.dumps(exchanges, indent=2) + '\n')
                return response
            time.sleep(.05)
        raise TimeoutError('Speech command response unavailable; no redispatch')

    checks = {}
    first = request(1, 'read')
    basis = first['state']
    if during_preparation:
        worker = read_file('speech-worker.json')
        boundary = (worker['frames'] * 1_000_000_000 + worker['sample_rate'] - 1) // worker['sample_rate']
        checks['stopped_during_preparation'] = basis['stop_phase'] == 0 and basis['phase'] == 3
        checks['late_preparation_discarded'] = basis['discarded_preparations'] == 1 and worker['status'] == 'prepared'
        checks['cancelled_before_start'] = basis['start_count'] == 0 and basis['done_count'] == 0 and basis['stop_count'] == 1
        checks['unprepared_position_preserved'] = basis['frame'] == 0 and basis['fraction'] == 0 and basis['frames'] == 0
        after = request(2, 'advance', boundary)
        checks['cancel_cannot_complete'] = after['outcome'] == 'inactive' and after['state'] == basis
        time.sleep(2)
        request(3, 'read')
        checks['cancel_advance_replay'] = request(2, 'advance', boundary) == after
        checks['cancel_advance_conflict'] = request(2, 'advance', boundary + 1)['outcome'] == 'conflict'
        checks['cancel_remains_terminal'] = request(4, 'read')['state'] == basis
        (out / 'speech-control-checks.json').write_text(json.dumps(checks, indent=2) + '\n')
        return checks
    checks['cursor_prepared_at_zero'] = basis['phase'] == 1 and basis['frame'] == 0 and \
        basis['fraction'] == 0 and basis['frames'] > 0 and basis['start_count'] == 0
    boundary = (basis['frames'] * 1_000_000_000 + basis['sample_rate'] - 1) // basis['sample_rate']
    checks['actual_sample_boundary'] = basis['remaining_ns'] == boundary
    crossing = request(2, 'advance', boundary + 1)
    checks['crossing_rejected_without_effect'] = crossing['outcome'] == 'crossing_rejected' and \
        crossing['state'] == basis
    advance = boundary // 3 if cancel_at else boundary - 1
    before = request(3, 'advance', advance)
    checks['partial_progress' if cancel_at else 'one_ns_before_completion'] = before['outcome'] == 'advanced' and \
        before['state']['phase'] == 1 and before['state']['remaining_ns'] == boundary - advance and \
        before['state']['start_count'] == 1 and before['state']['done_count'] == 0
    time.sleep(2)
    held = request(4, 'read')
    checks['partial_cursor_held'] = held['state'] == before['state']
    native = adb('exec-out', 'run-as', package, 'cat', 'files/native-speech.jsonl').stdout
    (out / 'speech-before-completion.jsonl').write_bytes(native)
    records = [json.loads(line) for line in native.splitlines() if line]
    checks['native_start_without_early_done'] = sum(row['kind'] == 'playing' for row in records) == 1 and \
        not any(row['kind'] in ('done', 'stopped', 'error') for row in records)
    if cancel_at:
        tap_cancel_control(adb, out, package, cancel_at)
        deadline = time.monotonic() + 5
        while read_file('speech-control.json')['phase'] != 3:
            if time.monotonic() >= deadline:
                raise TimeoutError('Cancellation tap did not cancel controlled speech; no repeated tap')
            time.sleep(.1)
        cancelled = request(5, 'read')['state']
        checks['cancel_preserves_position'] = all(cancelled[k] == before['state'][k]
            for k in ('frame', 'fraction', 'frames', 'token'))
        checks['cancel_terminal'] = cancelled['phase'] == 3 and cancelled['stop_count'] == 1 and \
            cancelled['done_count'] == 0
        after = request(6, 'advance', boundary)
        checks['cancel_cannot_complete'] = after['outcome'] == 'inactive' and after['state'] == cancelled
        time.sleep(2)
        request(7, 'read')
        checks['cancel_advance_replay'] = request(6, 'advance', boundary) == after
        checks['cancel_advance_conflict'] = request(6, 'advance', boundary + 1)['outcome'] == 'conflict'
        checks['cancel_remains_terminal'] = request(8, 'read')['state'] == cancelled
        (out / 'speech-control-checks.json').write_text(json.dumps(checks, indent=2) + '\n')
        return checks
    done = request(5, 'advance', 1)
    checks['exact_completion'] = done['outcome'] == 'completed' and done['state']['phase'] == 2 and \
        done['state']['frame'] == basis['frames'] and done['state']['fraction'] == 0 and \
        done['state']['done_count'] == 1
    request(6, 'read')
    replay = request(5, 'advance', 1)
    checks['completion_replay'] = replay == done
    conflict = request(5, 'advance', 2)
    checks['completion_conflict'] = conflict['outcome'] == 'conflict'
    final = request(7, 'read')
    if utterances == 1:
        checks['completion_not_duplicated'] = final['state'] == done['state']
    else:
        previous_token = token
        deadline = time.monotonic() + 20
        while True:
            following = read_file('speech-control.json')
            if following['token'] != previous_token and following['phase'] == 1:
                break
            if time.monotonic() >= deadline:
                raise TimeoutError('App did not produce a prepared following utterance')
            time.sleep(.1)
        token = following['token']
        workers.append(read_file('speech-worker.json'))
        (out / 'sequential-workers.json').write_text(json.dumps(workers, indent=2) + '\n')
        checks['two_actual_preparations'] = len({row['identity'] for row in workers}) == 2 and all(
            row['status'] == 'prepared' and row['audio_deleted'] and row['frames'] > 0 for row in workers)
        second = request(8, 'read')['state']
        checks['following_cursor_at_zero'] = second['utterance'] == 2 and second['frame'] == 0 and \
            second['fraction'] == 0 and second['start_count'] == 0 and second['done_count'] == 0
        stale = request(9, 'advance', 1, previous_token)
        checks['old_token_cannot_advance_next'] = stale['outcome'] == 'wrong_token' and stale['state'] == second
        checks['old_completion_replay_after_next'] = request(5, 'advance', 1, previous_token) == done
        checks['old_replay_does_not_move_next'] = request(10, 'read')['state'] == second
        boundary2 = (workers[1]['frames'] * 1_000_000_000 + workers[1]['sample_rate'] - 1) // workers[1]['sample_rate']
        checks['following_actual_sample_boundary'] = second['remaining_ns'] == boundary2 and \
            workers[1]['identity'] == token and second['frames'] == workers[1]['frames']
        rejected = request(11, 'advance', boundary2 + 1)
        checks['following_crossing_rejected'] = rejected['outcome'] == 'crossing_rejected' and rejected['state'] == second
        partial2 = request(12, 'advance', boundary2 - 1)
        checks['following_one_ns_remaining'] = partial2['outcome'] == 'advanced' and \
            partial2['state']['remaining_ns'] == 1 and partial2['state']['done_count'] == 0
        time.sleep(2)
        checks['following_cursor_held'] = request(13, 'read')['state'] == partial2['state']
        native = adb('exec-out', 'run-as', package, 'cat', 'files/native-speech.jsonl').stdout
        (out / 'second-before-completion.jsonl').write_bytes(native)
        records = [json.loads(line) for line in native.splitlines() if line]
        ids = [row['utterance_id'] for row in records if row['kind'] == 'speak_requested']
        checks['following_no_early_native_done'] = len(ids) >= 2 and not any(
            row['kind'] == 'done' and row.get('utterance_id') == ids[1] for row in records)
        done2 = request(14, 'advance', 1)
        checks['following_exact_completion'] = done2['outcome'] == 'completed' and done2['state']['phase'] == 2 and \
            done2['state']['frame'] == second['frames'] and done2['state']['done_count'] == 1
        request(15, 'read')
        checks['following_completion_replay'] = request(14, 'advance', 1) == done2
        checks['following_completion_conflict'] = request(14, 'advance', 2)['outcome'] == 'conflict'
        checks['following_completion_not_duplicated'] = request(16, 'read')['state'] == done2['state']
    (out / 'speech-control-checks.json').write_text(json.dumps(checks, indent=2) + '\n')
    return checks
