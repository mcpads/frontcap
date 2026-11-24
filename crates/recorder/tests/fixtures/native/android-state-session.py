#!/usr/bin/env python3
"""Protocol fixture with retained SDK-shaped evidence; it never acquires a device."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

root = Path(sys.argv[1])
workspace = root.parent.parent.parent

def save(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix('.tmp')
    temporary.write_text(json.dumps(value))
    temporary.replace(path)

lock = (root / 'session.lock').open('r+')
fcntl.flock(lock, fcntl.LOCK_EX)
save(root / 'worker.json', dict(pid=os.getpid(), started=' '.join(subprocess.check_output(['ps', '-o', 'lstart=', '-p', str(os.getpid())], env=dict(os.environ, TZ='UTC', LC_ALL='C', LANG='C'), text=True).split())))
owner = None
visibility = 'foreground'
history = None
number = 0

def transition(message):
    global number, visibility
    state = message['state']
    request = message['id']
    entry = dict(id=request, state=state, completion=None)
    history['entries'].append(entry)
    save(root / 'app-state/history.json', history)
    save(root / 'latest-view.json', None)
    attempt = root / 'device/view-app-state' / request
    save(attempt / 'intent.json', dict(owner=owner, state=state))
    commands = {}

    def command(args, output=''):
        global number
        number += 1
        path = root / 'device' / f'command-{number}'
        save(path / 'command.json', ['/fixture/adb', ['-s', owner['serial'], *args]])
        save(path / 'exchange.json', dict(pid=77, stdout=list(output.encode()), stderr=[], reply=None,
             failure=None, cleanup_failure=None, exit_code=0, signal_error=None, evidence_failure=None))
        for name in ['command.json', 'exchange.json']:
            commands[f'command-{number}/{name}'] = hashlib.sha256((path / name).read_bytes()).hexdigest()
        return number

    def process():
        pid = str(owner['pid'])
        stat = pid + ' (client main) S ' + ' '.join(['0'] * 18) + ' 50 0 0'
        package = owner['package']
        return [command(['shell', 'pidof', package], pid),
                command(['shell', 'cat', '/proc/sys/kernel/random/boot_id'], owner['generation']),
                command(['shell', 'run-as', package, 'cat', f'/proc/{pid}/stat'], stat),
                command(['shell', 'run-as', package, 'cat', f'/proc/{pid}/cmdline'], package + '\0'),
                command(['shell', 'run-as', package, 'cat', f'/proc/{pid}/stat'], stat)]

    component = owner['package'] + '/' + owner['activity']
    launcher = 'dev.example.launcher/dev.example.launcher.Home'
    before = component if visibility == 'foreground' else launcher
    after = component if state == 'foreground' else launcher
    clock_args = ['shell', 'run-as', owner['package'], 'cat', 'files/clock-control.json']
    clock_value = json.dumps(dict(generation=owner['generation'], pid=owner['pid'], status='ok', last_id=1, now_ns=500000000))
    samples = dict(before=process())
    samples['clock_before'] = command(clock_args, clock_value)
    samples['focused_before'] = command(['shell', 'dumpsys', 'window'], 'mCurrentFocus=Window{abc u0 ' + before + '}')
    args = ['shell', 'input', 'keyevent', 'KEYCODE_HOME'] if state == 'background' else [
        'shell', 'am', 'start', '-W', '--activity-reorder-to-front', '-n', component]
    samples['dispatch'] = command(args)
    visibility = state
    with (root / 'state-effects.jsonl').open('a') as effects:
        effects.write(state + '\n')
    samples['focused_after'] = command(['shell', 'dumpsys', 'window'], 'mCurrentFocus=Window{abc u0 ' + after + '}')
    samples['clock_after'] = command(clock_args, clock_value) if state == 'foreground' else None
    samples['after'] = process()
    identity = dict(pid=owner['pid'], boot_id=owner['generation'], start_ticks=50, command=owner['package'])
    result = dict(owner=owner, requested=state, process_before=identity, process_after=identity,
                  focused_before=before, focused_after=after, now_ns=500000000 if state == 'foreground' else None)
    save(attempt / 'receipt.json', dict(adb='/fixture/adb', transition=result, samples=samples, commands=commands))
    (workspace / 'state-published').touch()
    deadline = time.monotonic() + 10
    while (workspace / 'hold-state').exists():
        if time.monotonic() >= deadline:
            raise TimeoutError('test did not release the held host result')
        time.sleep(.005)
    if not (workspace / 'leave-state-pending').exists():
        entry['completion'] = result
        save(root / 'app-state/history.json', history)
    return dict(state='completed', transition=result)

for line in sys.stdin:
    message = json.loads(line)
    if message['command'] == 'start':
        config = json.loads((root / 'launch.json').read_text())['config']
        app = config['view_app']
        owner = dict(package=app['package'], activity=app['activity'], serial='emulator-5580',
                     generation='58e8003c-c32d-49f2-a4d3-628724d76b41', pid=42,
                     source_sha256=config['source_sha256'], apk_sha256=config['apk_sha256'])
        history = dict(owner=owner, entries=[])
        save(root / 'device/launch.json', config)
        save(root / 'app-state/history.json', history)
        save(root / 'requests/requests.json', dict(identity=dict(target='android_views', device=owner['serial'],
             generation=owner['generation'], pid=owner['pid']), controller_id=owner['generation'],
             source_sha256=owner['source_sha256'], next_clock=1, next_input=1, next_text=1, next_scroll=1, bindings={}))
        save(root / 'view-ready.json', owner)
        continue
    if message['command'] == 'stop':
        break
    assert message['command'] == 'set_view_state'
    save(root / 'commands' / str(message['number']) / 'result.json', transition(message))
save(root / 'result.json', dict(error=None, retirement=dict(passed=True, errors=[], signal_error=None)))
