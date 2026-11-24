"""Drive the installed fixture through adb input and retain raw hierarchy/screenshots."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import xml.etree.ElementTree as ET
from PIL import Image

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / 'tools'))
from native_android_diagnostics import run_command
from native_android_input import focused_component
from native_android_capture import node, center, observe as observe_presentation

out, sdk, port = Path(sys.argv[1]), Path(sys.argv[2]), sys.argv[3]
virtual = "--virtual" in sys.argv[4:]
input_interruption = (os.environ.get('FC_ANDROID_INPUT_INTERRUPTION') or None) if virtual else None
if input_interruption not in (None, 'controller', 'tap', 'app', 'app-controller', 'before-transport', 'transport-start', 'guest-before-input'):
    raise ValueError('Invalid Android input interruption profile')
activity = "VirtualActivity" if virtual else "ProbeActivity"
adb = [str(sdk / "platform-tools/adb"), "-s", "emulator-" + port]


def command(*args, timeout=20):
    return run_command([*adb, *args], out, timeout=timeout)


def retain_failure(kind, error, traceback):
    try:
        if not isinstance(error, Exception) or isinstance(error, InterruptedError):
            return
        captures = [
            ('app-failure.png', ('exec-out', 'screencap', '-p')),
            ('app-failure-window.txt', ('shell', 'dumpsys', 'window')),
            ('app-failure-crash.txt', ('logcat', '-b', 'crash', '-d')),
            ('app-failure-logcat.txt', ('logcat', '-d', '-t', '400', '-v', 'threadtime')),
            ('app-failure-gfxinfo.txt', ('shell', 'dumpsys', 'gfxinfo', 'dev.frontcap.s18.probe', 'framestats')),
        ]
        if virtual:
            captures.append(('app-failure-state.json', ('shell', 'run-as', 'dev.frontcap.s18.probe', 'cat', 'files/state.json')))
            captures.append(('app-failure-clock-control.json', ('shell', 'run-as', 'dev.frontcap.s18.probe', 'cat', 'files/clock-control.json')))
            captures.append(('app-failure-clock-checkpoint.json', ('shell', 'run-as', 'dev.frontcap.s18.probe', 'cat', 'files/clock-checkpoint.json')))
            captures.append(('app-failure-input-control.json', ('shell', 'run-as', 'dev.frontcap.s18.probe', 'cat', 'files/input-control.json')))
            captures.append(('app-failure-input-checkpoint.json', ('shell', 'run-as', 'dev.frontcap.s18.probe', 'cat', 'files/input-checkpoint.json')))
        for name, args in captures:
            try:
                (out / name).write_bytes(command(*args))
            except InterruptedError:
                raise
            except (OSError, subprocess.SubprocessError) as diagnostic_error:
                print(f'Could not save {name}: {diagnostic_error}', file=sys.stderr)
    finally:
        sys.__excepthook__(kind, error, traceback)


sys.excepthook = retain_failure


def observe(name, count):
    if not virtual:
        return observe_presentation(command, out, name, count)
    deadline = time.monotonic() + 20
    def remaining():
        budget = deadline - time.monotonic()
        if budget <= 0:
            raise TimeoutError('Virtual hierarchy readiness deadline reached')
        return budget
    while True:
        command("shell", "uiautomator", "dump", "/sdcard/frontcap-window.xml", timeout=remaining())
        raw = command("shell", "cat", "/sdcard/frontcap-window.xml", timeout=remaining())
        # Preserve late matching output without accepting it as on-time readiness.
        (out / (name + ".xml")).write_bytes(raw)
        remaining()
        tree = ET.fromstring(raw)
        try:
            label = node(tree, "native-count-" + str(count))
            remaining()
            break
        except RuntimeError:
            if time.monotonic() >= deadline:
                (out / (name + ".xml")).write_bytes(raw)
                raise
    (out / (name + ".xml")).write_bytes(raw)
    png = command("exec-out", "screencap", "-p")
    (out / (name + ".png")).write_bytes(png)
    # The acceptance checker runs after guest teardown; keep diagnostics even
    # when hierarchy lookup succeeds but a later system dialog dims the image.
    (out / (name + "-window.txt")).write_bytes(command("shell", "dumpsys", "window"))
    (out / (name + "-crash.txt")).write_bytes(command("logcat", "-b", "crash", "-d"))
    image = Image.open(out / (name + ".png")).convert("RGB")
    pixel = image.getpixel(center(node(tree, "native-color")))
    return tree, dict(phase=name, count=count, text=label.get("text"), pixel=pixel, size=image.size)


command("shell", "input", "keyevent", "KEYCODE_WAKEUP")
command("shell", "wm", "dismiss-keyguard")
launch_index = 0
def launch(stop_at=None):
    global launch_index
    command("shell", "am", "force-stop", "dev.frontcap.s18.probe")
    boundary = 'input_stop_at' if input_interruption in ('app', 'app-controller') else 'clock_stop_at'
    extra = ('--es', boundary, stop_at) if stop_at else ()
    if input_interruption in ('controller', 'tap'):
        from native_android_input_interruption import PHASES
        if launch_index < len(PHASES):
            extra += ('--es', 'input_stop_at', PHASES[launch_index])
    launch_index += 1
    return command("shell", "am", "start", "-W", "-n", f"dev.frontcap.s18.probe/dev.frontcap.s18.{activity}", *extra)

input_index = 0
def tap(tree, description):
    global input_index
    x, y = center(node(tree, description))
    if virtual:
        from native_android_input import InputClient, read_receipt
        from native_android_input_recovery import recover_tap, registration_controls
        from native_controller_recovery import PHASES
        ready = read_receipt(command, 'dev.frontcap.s18.probe')
        client = InputClient(command, out/'input-controllers'/ready['generation'])
        if input_interruption and input_index < len(PHASES):
            from native_android_input_interruption import PHASES as INTERRUPTIONS, interrupt_tap
            reply = interrupt_tap(out, client, adb, INTERRUPTIONS[input_index], description, x, y,
                                  mode=input_interruption)
        else:
            reply = (recover_tap(out, client, adb, PHASES[input_index], description, x, y)
                     if input_index < len(PHASES) else client.tap(description, x, y))
        if input_index == len(PHASES):
            registration_controls(out, client, reply)
        input_index += 1
        return dict(kind='adb-tap', target=description, x=x, y=y, receipt=reply)
    window = command('shell', 'dumpsys', 'window')
    (out / 'app-input-window.txt').write_bytes(window)
    focus = focused_component(window)
    if focus != f'dev.frontcap.s18.probe/dev.frontcap.s18.{activity}':
        raise RuntimeError(f'Fixture does not own input focus: {focus}')
    command("shell", "input", "tap", str(x), str(y))
    return dict(kind="adb-tap", target=description, x=x, y=y)


if virtual and input_interruption in ('transport-start','guest-before-input'):
    from native_android_input_transport import exercise
    exercise(out, command, launch, observe, node, center, adb, mode=input_interruption)
elif virtual and input_interruption == 'before-transport':
    from native_android_input_preparation import exercise
    exercise(out, command, launch, observe, node, center, adb)
elif virtual and input_interruption in ('app', 'app-controller'):
    from native_android_input_lifecycle import lifecycle
    lifecycle(out, command, launch, observe, node, center, adb, mode=input_interruption)
elif virtual:
    from virtual_app_probe import exercise
    from native_android_lifecycle import lifecycle
    lifecycle(out, command, launch, observe, tap, adb)
    from native_controller_recovery import exercise as controller_recovery
    from native_android_lifecycle import archive, stop
    from native_clock_control import atomic_json
    recovery_launch = launch().decode()
    tree, recovery_before = observe('controller-initial', 0)
    recovery_input = tap(tree, 'native-increment')
    directory = out/'controller-recovery'
    report = controller_recovery(directory,
        [sys.executable, str(Path(__file__).resolve().parents[3]/'tools/native_android_control.py'), '--adb', adb[0], '--serial', adb[2]],
        lambda: command('shell','run-as','dev.frontcap.s18.probe','cat','files/clock-control.json'))
    _, capture = observe('controller-recovered', 1)
    stopped = stop(command, report['initial']['pid'], directory/'processes.txt')
    archive(command, directory/'app')
    atomic_json(directory/'app.json', dict(launch=recovery_launch, before=recovery_before,
                                         input=recovery_input, capture=capture, stopped=stopped))
    (out / "app-launch.txt").write_bytes(launch())
    exercise(out, command, observe, node, center, tap)
    archive(command, out/'app')
else:
    (out / "app-launch.txt").write_bytes(launch())
    tree, before = observe("app-before", 0)
    delivery = tap(tree, "native-increment")
    _, after = observe("app-after", 1)
    (out / "app.json").write_text(json.dumps(dict(input=delivery, observations=[before, after]), indent=2))
command("shell", "am", "force-stop", "dev.frontcap.s18.probe")
command("uninstall", "dev.frontcap.s18.probe")
command("shell", "rm", "-f", "/sdcard/frontcap-window.xml")
