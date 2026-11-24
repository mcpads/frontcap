"""Build and exercise an owned Darwin rename boundary; never attaches to an existing process."""
from pathlib import Path
import hashlib
import json
import os
import signal
import subprocess
import sys
import time
import traceback

source = Path(__file__).resolve().parent
root = Path(sys.argv[1]).resolve()
root.mkdir()
def save(path, value):
    path.write_text(json.dumps(value, indent=2) + '\n')
def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()
inputs = {str(path): digest(path) for path in source.iterdir() if path.is_file()}
save(root / 'inputs.json', inputs)
checks = []
try:
    commands = [
        ['xcrun', 'clang', '-Wall', '-Wextra', '-Werror', '-dynamiclib', str(source / 'rename-stop.c'), '-o', str(root / 'rename-stop.dylib')],
        ['xcrun', 'clang', '-Wall', '-Wextra', '-Werror', str(source / 'writer.c'), '-o', str(root / 'writer')],
    ]
    save(root / 'build-commands.json', commands)
    for command in commands:
        subprocess.run(command, check=True, timeout=30)
    for mode in ['unmatched', 'resume', 'kill']:
        case = root / mode
        case.mkdir()
        pending, final, marker = [case / name for name in ['result.pending', 'result.json', 'reached.json']]
        env = dict(os.environ, DYLD_INSERT_LIBRARIES=str(root / 'rename-stop.dylib'),
                   FC_RENAME_SOURCE=str(pending), FC_RENAME_DESTINATION=str(final), FC_RENAME_MARKER=str(marker))
        if mode == 'unmatched':
            env['FC_RENAME_SOURCE'] = str(case / 'different.pending')
        save(case / 'intent.json', {key: env[key] for key in ['DYLD_INSERT_LIBRARIES', 'FC_RENAME_SOURCE', 'FC_RENAME_DESTINATION', 'FC_RENAME_MARKER']})
        process = subprocess.Popen([str(root / 'writer'), str(pending), str(final)], env=env)
        try:
            if mode != 'unmatched':
                end = time.monotonic() + 5
                while True:
                    assert process.poll() is None, 'writer exited before the boundary'
                    state = subprocess.check_output(['ps', '-p', str(process.pid), '-o', 'stat='], text=True).strip()
                    if marker.exists() and state.startswith('T'):
                        break
                    assert time.monotonic() < end, 'owned writer did not stop'
                    time.sleep(.01)
                identity = subprocess.check_output(['ps', '-p', str(process.pid), '-o', 'lstart='], text=True,
                                                   env=dict(os.environ, TZ='UTC', LC_ALL='C', LANG='C')).strip()
                assert json.loads(marker.read_text()) == dict(stage='before_rename', pid=process.pid)
                assert pending.read_bytes() == b'{"result":"prepared"}\n' and not final.exists()
                save(case / 'stopped.json', dict(pid=process.pid, started=identity, state=state, pending_sha256=digest(pending), final_absent=True))
                process.send_signal(signal.SIGCONT if mode == 'resume' else signal.SIGKILL)
            code = process.wait(timeout=5)
            assert code == (-signal.SIGKILL if mode == 'kill' else 0), code
            if mode == 'kill':
                assert pending.read_bytes() == b'{"result":"prepared"}\n' and not final.exists()
            else:
                assert final.read_bytes() == b'{"result":"prepared"}\n' and not pending.exists()
                assert marker.exists() == (mode == 'resume')
            save(case / 'verdict.json', dict(state='passed', exit_code=code, process_reaped=True,
                                           pending_exists=pending.exists(), final_exists=final.exists()))
            checks.append(mode)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
    assert all(digest(Path(path)) == sha for path, sha in inputs.items())
    save(root / 'result.json', dict(state='passed', checks=checks, inputs_unchanged=True))
except Exception:
    save(root / 'result.json', dict(state='failed', checks=checks, error=traceback.format_exc()))
    raise
