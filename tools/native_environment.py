#!/usr/bin/env python3
"""Read-only native prerequisite checks; never launches a device or installs a dependency."""
import json
import fcntl
import os
from pathlib import Path
import platform
import re
import subprocess
import sys


def inspect_host_ownership(directory):
    """Observe availability without creating locks, clearing fences or adopting targets."""
    directory = Path(directory)
    fence = directory/'native-session.json'
    try:
        lock = (directory/'native-tests.lock').open('rb')
    except FileNotFoundError:
        return (False, f'Ownership record exists without host lock: {fence}') if fence.exists() else (True, 'No prior native host ownership')
    except OSError as error:
        return False, f'Cannot inspect native host lock: {error}'
    with lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return False, 'Native host is in use; keep tracking the existing run'
        except OSError as error:
            return False, f'Cannot inspect native host lock: {error}'
        try:
            if not fence.exists(): return True, 'Native host lock is available; no adapter ownership record'
            state = json.loads(fence.read_text())
            if (type(state) is not dict or set(state) != {'directory','retired'}
                    or type(state['retired']) is not bool or type(state['directory']) is not str
                    or not Path(state['directory']).is_absolute()):
                raise ValueError('Invalid native session ownership record')
            if not state['retired']:
                return False, f'Unretired native session; inspect {state["directory"]}. No replacement or PID signalling is authorized.'
            return True, f'Native host lock is available; previous session retired: {state["directory"]}'
        except (OSError, ValueError) as error:
            return False, f'Cannot verify native ownership: {error}'


def inspect_environment(platforms, env=None, *, check_ownership=False):
    env = dict(os.environ if env is None else env)
    checks, settings = [], {}

    def record(name, passed, detail):
        checks.append(dict(name=name, passed=bool(passed), detail=str(detail)))

    def command(name, args):
        try:
            result = subprocess.run(args, env=env, capture_output=True, text=True, timeout=30)
            detail = (result.stdout + result.stderr).strip()
            record(name, result.returncode == 0, detail or f"exit {result.returncode}")
            return result.stdout.strip() if result.returncode == 0 else None
        except (OSError, subprocess.TimeoutExpired) as error:
            record(name, False, error)
            return None

    def file(name, path, executable=False):
        path = Path(path)
        record(name, path.is_file() and (not executable or os.access(path, os.X_OK)), path)

    def newest(path):
        directories = [p for p in path.glob("*") if p.is_dir()]
        return max(directories, key=lambda p: tuple(int(n) for n in re.findall(r"\d+", p.name)), default=None)

    record("host", platform.system() == "Darwin" and platform.machine() == "arm64", platform.platform())
    record("python", sys.version_info >= (3, 12), sys.version)
    if check_ownership:
        record("native_host_ownership", *inspect_host_ownership(Path.home()/".cache/frontcap"))
    command("shell", ["bash", "--version"])
    if "ios" in platforms:
        command("xcode", ["xcodebuild", "-version"])
        first_launch = command("xcode_first_launch", ["xcodebuild", "-checkFirstLaunchStatus"])
        if first_launch is None:
            checks[-1]['detail'] += '; complete the selected Xcode initial component setup on the host'
        command("simulator_sdk", ["xcrun", "--sdk", "iphonesimulator", "--show-sdk-path"])
        command("clang", ["xcrun", "clang", "--version"])
        file("codesign", "/usr/bin/codesign", executable=True)
        if first_launch is None:
            # simctl may itself attempt component installation after an Xcode update.
            record('simulator_inventory', False, 'Not queried until Xcode initial component setup is complete')
            raw = None
        else:
            raw = command("simulator_inventory", ["xcrun", "simctl", "list", "-j"])
        if raw is not None:
            try:
                inventory = json.loads(raw)
                runtime = "com.apple.CoreSimulator.SimRuntime.iOS-26-1"
                record("ios_runtime", any(r.get("identifier") == runtime and r.get("isAvailable") for r in inventory["runtimes"]), runtime)
                device = "com.apple.CoreSimulator.SimDeviceType.iPhone-16"
                record("ios_device_type", any(d.get("identifier") == device for d in inventory["devicetypes"]), device)
            except (ValueError, KeyError, TypeError) as error:
                record("simulator_inventory_format", False, error)
    if "android" in platforms:
        java = env.get("JAVA_HOME")
        if java:
            record("java_home", Path(java).is_dir(), f"{java} (JAVA_HOME)")
        else:
            java = command("java_home", ["/usr/libexec/java_home"])
        if java:
            settings["JAVA_HOME"] = java
            env["JAVA_HOME"] = java
            command("java", [str(Path(java) / "bin/java"), "-version"])
            command("javac", [str(Path(java) / "bin/javac"), "-version"])
            file("keytool", Path(java) / "bin/keytool", executable=True)
        sdk = Path(env.get("ANDROID_HOME", str(Path.home() / "Library/Android/sdk")))
        settings["ANDROID_HOME"] = str(sdk)
        file("android_platform", sdk / "platforms/android-35/android.jar")
        command("adb", [str(sdk / "platform-tools/adb"), "version"])
        command("emulator", [str(sdk / "emulator/emulator"), "-version"])
        avds = command("avd_inventory", [str(sdk / "emulator/emulator"), "-list-avds"])
        record("android_avd", avds is not None and "Medium_Phone_API_35" in avds.splitlines(), "Medium_Phone_API_35")
        build = newest(sdk / "build-tools")
        record("android_build_tools", build is not None, build or sdk / "build-tools")
        if build:
            for tool in ("d8", "aapt2", "apksigner", "zipalign"):
                file(tool, build / tool, executable=True)
        ndk = newest(sdk / "ndk")
        compilers = list(ndk.glob("toolchains/llvm/prebuilt/*/bin/aarch64-linux-android30-clang")) if ndk else []
        record("android_ndk", len(compilers) == 1, compilers or sdk / "ndk")
        if len(compilers) == 1:
            command("android_clang", [str(compilers[0]), "--version"])
        command("zip", ["zip", "-v"])
    return dict(passed=all(c["passed"] for c in checks), host=platform.node(),
                platforms=platforms, checks=checks, settings=settings)


if __name__ == "__main__":
    # Always print the full report; callers own the exit status so SSH can return failed diagnostics.
    print(json.dumps(inspect_environment(sys.argv[1:] or ["ios", "android"], check_ownership=True), indent=2))
