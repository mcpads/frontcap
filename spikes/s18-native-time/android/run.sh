#!/bin/bash
# S18 Android: the emulator's guest clock across a pause, and an ART process's clocks and timers under
# vclock. Usage: spikes/s18-native-time/android/run.sh [avd name]
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$HERE/../../results/s18-native-time"
OUT=${FC_OUTPUT_DIR:-$(mktemp -d "$HERE/../../results/s18-native-time/android.XXXXXX")}
AVD=${1:-Medium_Phone_API_35}
SDK=${ANDROID_HOME:-$HOME/Library/Android/sdk}
BT=$(ls -d "$SDK"/build-tools/* | sort -V | tail -1)
NDK=$(ls -d "$SDK"/ndk/* | sort -V | tail -1)
JAR=$SDK/platforms/android-35/android.jar
PORT=${FC_EMULATOR_PORT:-5580}
adb() { "$SDK/platform-tools/adb" -s emulator-$PORT "$@"; }
mkdir -p "$OUT"
if [ -n "$(ls -A "$OUT")" ]; then echo "Output directory must be empty: $OUT" >&2; exit 1; fi
echo "Evidence: $OUT"
phase() {
  python3 "$HERE/../../../tools/native_phase.py" "$OUT" "$$" "$@" || true
}
python3 - "$PORT" <<'PY'
import socket, sys
ports = [socket.socket(), socket.socket()]
for s, p in zip(ports, [int(sys.argv[1]), int(sys.argv[1]) + 1]):
    # Reuse closed connections in TIME_WAIT, while an active listener still prevents binding.
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", p))
PY

phase native-build
if [ "${FC_ANDROID_INPUT_INTERRUPTION:-}" = guest-before-input ]; then
  "$NDK"/toolchains/llvm/prebuilt/*/bin/aarch64-linux-android30-clang -Wall -Wextra -Werror -O1 -o "$OUT/input-gate" "$HERE/input_gate.c"
fi
"$NDK"/toolchains/llvm/prebuilt/*/bin/aarch64-linux-android30-clang -shared -fPIC -O1 -o "$OUT/libvclock.so" "$HERE/vclock.c" -ldl
"$NDK"/toolchains/llvm/prebuilt/*/bin/aarch64-linux-android30-clang -shared -fPIC -O1 -o "$OUT/libclock_observation.so" "$HERE/clock_observation.c"
mkdir -p "$OUT/classes" "$OUT/dex"
javac -source 11 -target 11 -classpath "$JAR" -d "$OUT/classes" "$HERE/src/dev/frontcap/s18/Clocks.java" "$HERE/src/dev/frontcap/s18/Steps.java" "$HERE/src/dev/frontcap/s18/ClockHandoff.java" "$HERE/src/dev/frontcap/s18/ProbeActivity.java" "$HERE/src/dev/frontcap/s18/VirtualActivity.java" "$HERE/src/dev/frontcap/s18/ClockControl.java" "$HERE/src/dev/frontcap/s18/InputControl.java"
"$BT/d8" --min-api 30 --lib "$JAR" --output "$OUT/dex" $(find "$OUT/classes" -name '*.class')
(cd "$OUT/dex" && zip -q ../clocks.jar classes.dex)
"$BT/aapt2" link -o "$OUT/app-unsigned.apk" --manifest "$HERE/AndroidManifest.xml" -I "$JAR"
(cd "$OUT/dex" && zip -q "$OUT/app-unsigned.apk" classes.dex)
"$BT/zipalign" -p -f 4 "$OUT/app-unsigned.apk" "$OUT/app.apk"
keytool -genkeypair -keystore "$OUT/fixture.keystore" -alias fixture -keyalg RSA -keysize 2048 -validity 30 \
  -storepass android -keypass android -dname "CN=Native Test Fixture" > "$OUT/keytool.log" 2>&1
"$BT/apksigner" sign --ks "$OUT/fixture.keystore" --ks-pass pass:android "$OUT/app.apk"
"$BT/apksigner" verify "$OUT/app.apk"

if [ "${FC_ANDROID_VIRTUAL_UI:-0}" = 1 ]; then
  python3 - "$HERE/AndroidManifest.xml" "$OUT/virtual-manifest.xml" <<'PYMANIFEST'
import sys,xml.etree.ElementTree as ET
ET.register_namespace('android', 'http://schemas.android.com/apk/res/android')
tree=ET.parse(sys.argv[1]); app=tree.getroot().find('application')
for key in ('debuggable','extractNativeLibs'): app.set('{http://schemas.android.com/apk/res/android}'+key,'true')
tree.write(sys.argv[2])
PYMANIFEST
  "$BT/aapt2" link -o "$OUT/virtual-unsigned.apk" --manifest "$OUT/virtual-manifest.xml" -I "$JAR"
  mkdir -p "$OUT/package/lib/arm64-v8a"
  cp "$OUT/libvclock.so" "$HERE/wrap.sh" "$OUT/package/lib/arm64-v8a/"
  (cd "$OUT/package" && zip -qr "$OUT/virtual-unsigned.apk" lib)
  (cd "$OUT/dex" && zip -q "$OUT/virtual-unsigned.apk" classes.dex)
  "$BT/zipalign" -p -f 4 "$OUT/virtual-unsigned.apk" "$OUT/virtual.apk"
  "$BT/apksigner" sign --ks "$OUT/fixture.keystore" --ks-pass pass:android "$OUT/virtual.apk"
  "$BT/apksigner" verify "$OUT/virtual.apk"
fi




# netsimd writes its discovery/config files here but does not create the macOS fallback.
NETSIM_DISCOVERY_DIR=${TMPDIR:-"$HOME/Library/Caches/TemporaryItems"}
mkdir -p "$NETSIM_DISCOVERY_DIR"
printf '%s\n' "$NETSIM_DISCOVERY_DIR" > "$OUT/netsim-discovery-directory"

EMULATOR_ARGS=(-avd "$AVD")
if [ -n "${FC_ANDROID_CORES:-}" ]; then EMULATOR_ARGS+=(-cores "$FC_ANDROID_CORES"); fi
if [ -n "${FC_ANDROID_MEMORY_MB:-}" ]; then EMULATOR_ARGS+=(-memory "$FC_ANDROID_MEMORY_MB"); fi
phase emulator-start
"$SDK/emulator/emulator" "${EMULATOR_ARGS[@]}" -no-window -no-audio -no-snapshot -read-only -no-boot-anim -port $PORT > "$OUT/emulator.log" 2>&1 &
EMULATOR_PID=$!
printf '%s\n' "$EMULATOR_PID" > "$OUT/emulator-pid"
printf '%s\n' "$PORT" > "$OUT/emulator-port"
cleanup() {
  phase cleanup "$?"
  # Only stop the emulator this script launched, never an unrelated device on the port.
  kill "$EMULATOR_PID" >/dev/null 2>&1 || true
  wait "$EMULATOR_PID" 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
adb wait-for-device
deadline=$((SECONDS + 180))
until [ "$(adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ]; do
  if [ "$SECONDS" -ge "$deadline" ] || ! kill -0 "$EMULATOR_PID" 2>/dev/null; then
    echo "Emulator did not boot" >&2; exit 1
  fi
  sleep 2
done
phase emulator-booted
echo "== build type $(adb shell getprop ro.build.type | tr -d '\r'), Android $(adb shell getprop ro.build.version.release | tr -d '\r')"
# Wait for the native radio backend instead of disabling the crashing service.
python3 - "$SDK/platform-tools/adb" "$PORT" "$OUT/bluetooth-state.txt" <<'PYRADIO'
import subprocess,sys,time
from pathlib import Path
path=Path(sys.argv[3]); deadline=time.monotonic()+15
while True:
    with path.open('w') as stream:
        subprocess.run([sys.argv[1],'-s','emulator-'+sys.argv[2],'shell','dumpsys','bluetooth_manager'],
                       stdout=stream,stderr=subprocess.STDOUT,timeout=5,check=True)
    if '\n  state: ON\n' in path.read_text(): break
    if time.monotonic()>=deadline: raise SystemExit('Bluetooth backend did not reach ON; inspect bluetooth-state.txt')
    time.sleep(.2)
PYRADIO

# Receiver history can disappear before the final pause diagnostics.
phase startup-diagnostics
python3 "$HERE/../../../tools/native_android_diagnostics.py" "$OUT" "$SDK" "$PORT" startup

phase native-fixtures
adb push "$OUT/clocks.jar" "$OUT/libvclock.so" "$OUT/libclock_observation.so" /data/local/tmp/ >/dev/null
run() {
  FC_CLOCK_TRACE_EVENTS="$OUT/$1-reader.jsonl" python3 "$HERE/../../../tools/native_clock_trace.py" "$SDK/platform-tools/adb" -s emulator-$PORT \
    shell "FC_CLOCK_OBSERVATIONS=1 $2 CLASSPATH=/data/local/tmp/clocks.jar app_process /system/bin dev.frontcap.s18.Clocks"
}
echo "== ART, real clock (host-stamped)"
run real "" | tee "$OUT/real.txt"
echo "== ART, vclock at 10x (host-stamped)"
run scaled "FC_SCALE=10 LD_PRELOAD=/data/local/tmp/libvclock.so" | tee "$OUT/scaled.txt"
for mode in real scaled; do
  if ! python3 "$HERE/../../../tools/native_timer_observations.py" "$OUT/$mode-reader.jsonl" > "$OUT/$mode-timers.json"; then
    echo "Timer boundary analysis unavailable: $OUT/$mode-timers.json" >&2
  fi
done
echo "== ART, frozen clock and explicit Handler deadlines"
adb shell "FC_MANUAL=1 FC_HOLD_MS=$(( ${FC_HOLD_US:-200000} / 1000 )) LD_PRELOAD=/data/local/tmp/libvclock.so CLASSPATH=/data/local/tmp/clocks.jar app_process /system/bin dev.frontcap.s18.Steps" | tee "$OUT/step.jsonl"
adb shell rm -f /data/local/tmp/clocks.jar /data/local/tmp/libvclock.so /data/local/tmp/libclock_observation.so

echo "== installed Android app input and screen evidence"
phase installed-app
adb install "$OUT/app.apk"
python3 "$HERE/app_probe.py" "$OUT" "$SDK" "$PORT"
if [ "${FC_ANDROID_VIRTUAL_UI:-0}" = 1 ]; then
  echo "== installed Android app with manual clocks and host-paced rendering"
  phase virtual-app
  adb install --no-incremental "$OUT/virtual.apk"
  mkdir -p "$OUT/virtual"
  python3 "$HERE/app_probe.py" "$OUT/virtual" "$SDK" "$PORT" --virtual
fi

# Whole-guest suspension can disrupt unrelated system services. Keep this
# diagnostic after the app/process probes so they never reuse a paused guest.
phase guest-pause
python3 "$HERE/../../../tools/native_android_diagnostics.py" "$OUT" "$SDK" "$PORT" before
adb logcat -b crash -d > "$OUT/pause-crash-before.txt"
echo "== guest clocks across a 5 s emulator pause (uptime, realtime; host time)"
if python3 "$HERE/../../../tools/native_android_pause.py" "$OUT" "$SDK" "$PORT"; then
  :
else
  pause_status=$?
  python3 "$HERE/../../../tools/native_android_diagnostics.py" "$OUT" "$SDK" "$PORT" after || true
  exit "$pause_status"
fi
adb logcat -b crash -d > "$OUT/pause-crash-after.txt"

phase guest-diagnostics
python3 "$HERE/../../../tools/native_android_diagnostics.py" "$OUT" "$SDK" "$PORT" after

# Preserve the final native service state, including any restart/crash counter.
python3 - "$SDK/platform-tools/adb" "$PORT" "$OUT/bluetooth-final.txt" <<'PYRADIO'
import subprocess,sys
with open(sys.argv[3],'w') as stream:
    subprocess.run([sys.argv[1],'-s','emulator-'+sys.argv[2],'shell','dumpsys','bluetooth_manager'],
                   stdout=stream,stderr=subprocess.STDOUT,timeout=5,check=True)
PYRADIO
