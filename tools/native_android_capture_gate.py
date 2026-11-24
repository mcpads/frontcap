"""Diagnostic ADB wrapper: hold one screenshot command before invoking the real transport."""
import os
from pathlib import Path

from native_observation_gate import wait_for_release


def run(adb, directory, args, *, wait_seconds=5):
    directory = Path(directory)
    if len(args) == 5 and args[:1] == ['-s'] and args[2:] == ['exec-out', 'screencap', '-p']:
        wait_for_release(directory, {'command':[str(adb), *args]}, wait_seconds=wait_seconds)
    # Preserve direct process ownership across exec; no detached transport child.
    os.execv(str(adb), [str(adb), *args])


def configure(config, root):
    """Create a per-case wrapper; ordinary profiles keep their original ADB path."""
    wrapper = root/'capture-adb.py'
    wrapper.write_text('#!'+config['python']+'\nimport sys\n'
                       +'sys.path.insert(0, '+repr(str(Path(__file__).resolve().parent))+')\n'
                       +'from native_android_capture_gate import run\n'
                       +'run('+repr(config['adb'])+', '+repr(str(root/'capture-gate'))+', sys.argv[1:])\n')
    wrapper.chmod(0o700)
    return {**config, 'adb':str(wrapper)}
