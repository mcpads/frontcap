"""Read-only source View/screen correlation for an explicitly owned Android app.

No input dispatch or clock advance. Partial captures stay on disk and never publish a manifest.
"""
import hashlib
import json
from pathlib import Path
import re
import subprocess
import time
import uuid
from native_clock_control import atomic_json
from native_android_input import focused_component
from flutter_capture_artifacts import png_dimensions

FILES = ('views-before.json', 'views-after.json', 'clock-before.json', 'clock-after.json',
         'pid-before.txt', 'pid-after.txt', 'window-before.txt', 'window-after.txt', 'screen.png',
         'views-before-request.json', 'views-after-request.json',
         'views-before-submission.txt', 'views-after-submission.txt')


def capture(command, directory, *, package, activity, generation, pid, source_sha256, timeout=30):
    if not re.fullmatch(r'[A-Za-z_][A-Za-z_0-9]*(?:\.[A-Za-z_][A-Za-z_0-9]*)+', package):
        raise ValueError('Invalid application package')
    if not re.fullmatch(r'[A-Za-z_$][A-Za-z_0-9$]*(?:\.[A-Za-z_$][A-Za-z_0-9$]*)+', activity):
        raise ValueError('Expected a full Activity class name')
    if str(uuid.UUID(generation)) != generation or type(pid) is not int or not 0 < pid < 2**31:
        raise ValueError('Invalid application identity')
    if not re.fullmatch('[a-f0-9]{64}', source_sha256) or type(timeout) not in (int, float) or not 0 < timeout <= 300:
        raise ValueError('Invalid source digest or capture budget')
    root = Path(directory); root.mkdir(parents=True, exist_ok=False)
    started = time.clock_gettime_ns(time.CLOCK_MONOTONIC); deadline = time.monotonic()+timeout
    progress = dict(state='capturing', phase='starting')
    def phase(name):
        progress['phase'] = name; atomic_json(root/'progress.json', progress)
    def run(*args):
        remaining = deadline-time.monotonic()
        if remaining <= 0: raise TimeoutError('View capture deadline elapsed')
        result = command(*args, timeout=remaining)
        if len(result) > 16*1024*1024: raise RuntimeError('Capture artifact exceeds byte budget')
        if time.monotonic() > deadline: raise TimeoutError('Capture reply arrived after deadline')
        return result
    def save(name, data):
        (root/name).write_bytes(data); return data
    def source(name):
        nonce = str(uuid.uuid4())
        atomic_json(root/(name+'-request.json'), dict(request=nonce, generation=generation, pid=pid))
        ack = save(name+'-submission.txt', run('shell','content','call','--uri','content://'+package+'.views',
            '--method','observe','--extra','generation:s:'+generation,'--extra','request:s:'+nonce))
        if ack.strip() != b'Result: Bundle[{submitted=true}]': raise RuntimeError('View submission unconfirmed')
        while True:
            phase(name)
            try:
                raw = run('shell','run-as',package,'cat','files/view-observation.json')
                if len(raw) > 1024*1024: raise RuntimeError('View receipt exceeds byte budget')
                value = json.loads(raw)
                if value.get('request') == nonce: break
            except (subprocess.CalledProcessError, ValueError): pass
            if time.monotonic() >= deadline: raise TimeoutError('View observation not published')
            time.sleep(min(.02, max(0, deadline-time.monotonic())))
        save(name+'.json',raw)
        if value.get('generation') != generation or value.get('pid') != pid or value.get('state') != 'observed':
            raise RuntimeError('View observation unavailable or foreign')
        return nonce, value
    def owner(side):
        phase('owner-'+side)
        raw = save('pid-'+side+'.txt', run('shell','pidof',package))
        if raw.decode().split() != [str(pid)]: raise RuntimeError('Application PID changed')
        clock = save('clock-'+side+'.json',run('shell','run-as',package,'cat','files/clock-control.json'))
        value = json.loads(clock)
        if value.get('generation') != generation or value.get('pid') != pid or value.get('status') not in ('ready','ok'):
            raise RuntimeError('Clock owner unavailable or changed')
        return clock, value
    def foreground(side):
        phase('window-'+side)
        window = save('window-'+side+'.txt',run('shell','dumpsys','window'))
        if focused_component(window) != package+'/'+activity: raise RuntimeError('Expected Activity is not foreground')
    try:
        clock_before, stamp = owner('before')
        # Activity start acceptance can precede its first window-focus callback. Wait only
        # through read-only source observations, retaining every probe within this budget.
        # A missing/foreign receipt remains an error; it is never retried as readiness.
        for attempt in range(128):
            probe = 'focus-probe-'+str(attempt)
            before_request, before = source(probe)
            if before['catalog']['window_focused']:
                for suffix in ('.json', '-request.json', '-submission.txt'):
                    save('views-before'+suffix, (root/(probe+suffix)).read_bytes())
                break
            if time.monotonic() >= deadline:
                raise TimeoutError('View window focus deadline elapsed')
            time.sleep(min(.02, max(0, deadline-time.monotonic())))
        else:
            raise TimeoutError('View window focus observation budget exhausted')
        foreground('before')
        phase('screen'); screen = save('screen.png',run('exec-out','screencap','-p'))
        size = png_dimensions(screen)
        if max(size)>8192 or size[0]*size[1]>16000000: raise RuntimeError('Screenshot dimensions exceed budget')
        foreground('after')
        after_request, after = source('views-after')
        clock_after, _ = owner('after')
        if (clock_before != clock_after or before['now_ns'] != stamp['now_ns']
                or after['now_ns'] != before['now_ns'] or not before['catalog']['window_focused']):
            raise RuntimeError('Source state changed during sequential capture')
        manifest = dict(capture_id=str(uuid.uuid4()), package=package, activity=activity,
            generation=generation,pid=pid,source_sha256=source_sha256,now_ns=before['now_ns'],
            before_request=before_request,after_request=after_request,catalog=before['catalog'],size=list(size),
            started_monotonic_ns=started,finished_monotonic_ns=time.clock_gettime_ns(time.CLOCK_MONOTONIC),
            artifacts=[dict(path=name,bytes=(root/name).stat().st_size,
                sha256=hashlib.sha256((root/name).read_bytes()).hexdigest()) for name in FILES])
        if time.monotonic()>deadline: raise TimeoutError('Capture publication exceeded deadline')
        atomic_json(root/'manifest.json',manifest)
        changed=before['catalog']!=after['catalog']
        progress.update(state='unavailable' if changed else 'complete');atomic_json(root/'progress.json',progress)
        return dict(source_changed=changed,manifest=manifest,manifest_sha256=hashlib.sha256((root/'manifest.json').read_bytes()).hexdigest())
    except BaseException as error:
        progress.update(state='partial',error=f'{type(error).__name__}: {error}')
        atomic_json(root/'progress.json',progress)
        raise


def main():
    """One supervised collection; the parent owns and serializes the emulator/app."""
    import argparse
    import sys
    from native_android_diagnostics import run_command
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--adb', required=True)
    parser.add_argument('--serial', required=True)
    parser.add_argument('--directory', required=True)
    parser.add_argument('--timeout', type=float, required=True)
    args = parser.parse_args()
    if not Path(args.adb).is_absolute():
        raise ValueError('Expected absolute adb path')
    if not re.fullmatch(r'emulator-[1-9][0-9]*', args.serial):
        raise ValueError('Expected owned emulator serial')
    port = int(args.serial.split('-')[1])
    if port % 2 or port > 65534:
        raise ValueError('Invalid emulator port')
    raw = sys.stdin.buffer.read(65537)
    if len(raw) > 65536:
        raise ValueError('Capture request exceeds byte budget')
    owner = json.loads(raw)
    if type(owner) is not dict or set(owner) != {'package', 'activity', 'generation', 'pid', 'source_sha256'}:
        raise ValueError('Unexpected capture owner fields')
    root = Path(args.directory)
    # The supervisor reserves root and records the call before starting us. capture() reserves
    # its own new subdirectory; neither side reuses a partial collection after reply loss.
    if not root.is_dir() or root.is_symlink():
        raise ValueError('Expected reserved collection directory')
    def command(*arguments, timeout):
        return run_command([args.adb, '-s', args.serial, *arguments], root, timeout=timeout)
    result = capture(command, root/'capture', timeout=args.timeout, **owner)
    print(json.dumps(dict(manifest_sha256=result['manifest_sha256'],source_changed=result['source_changed'])))


if __name__ == '__main__':
    main()
