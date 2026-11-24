"""Keep step control separate from capture latency; use bounded visual readiness, then challenge holds."""
import json
from pathlib import Path
import subprocess
import sys
import time
from PIL import Image

out, device = Path(sys.argv[1]), sys.argv[2]
container = Path(subprocess.check_output(["xcrun", "simctl", "get_app_container", device, "dev.frontcap.s18.fade", "data"], text=True).strip())
state_path = container / "Documents/fade-state.json"
start = time.monotonic()


def await_phase(phase):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        if state_path.exists():
            state = json.loads(state_path.read_text())
            if state["phase"] == phase:
                return state
        time.sleep(.02)
    raise RuntimeError(f"Fade did not acknowledge {phase}")


def command(phase):
    temporary = container / "Documents/fade-command.tmp"
    temporary.write_text(json.dumps(dict(phase=phase)))
    temporary.replace(container / "Documents/fade-command.json")
    return await_phase(phase)


def capture(name, width):
    path = out / (name + ".png")
    subprocess.run(["xcrun", "simctl", "io", device, "screenshot", str(path)], check=True, timeout=15, capture_output=True)
    image = Image.open(path).convert("RGB")
    scale = image.width / width
    return dict(at_s=time.monotonic()-start,
                held=image.getpixel((int(150*scale), int(300*scale)))[0]/255,
                reference=image.getpixel((int(150*scale), int(600*scale)))[0]/255)


await_phase("ready")
samples = []
for step, expected in (("step1", .25), ("step3", .75)):
    state = command(step)
    deadline = time.monotonic() + 10
    while True:
        first = capture("fade_" + step + "_before", state["width"])
        if abs(first["held"] - expected) <= .02:
            break
        if time.monotonic() >= deadline:
            raise RuntimeError(f"Fade never reached {expected}: {first}")
    samples.append(first)
    if step == "step1":
        command("reference")  # Reference starts after the initial screenshot, independently of capture latency.
    time.sleep(2)  # Challenge a held position; this delay never establishes step application.
    samples.append(capture("fade_" + step + "_after", state["width"]))
(out / "fade.json").write_text(json.dumps(samples, indent=2))
