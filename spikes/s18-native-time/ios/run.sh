#!/bin/bash
# S18 iOS: clocks and timers under vclock, and a render-server fade held from inside the app.
# Usage: spikes/s18-native-time/ios/run.sh [simulator runtime id]
set -euo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$HERE/../../results/s18-native-time"
OUT=${FC_OUTPUT_DIR:-$(mktemp -d "$HERE/../../results/s18-native-time/ios.XXXXXX")}
RUNTIME=${1:-com.apple.CoreSimulator.SimRuntime.iOS-26-1}
mkdir -p "$OUT"
if [ -n "$(ls -A "$OUT")" ]; then echo "Output directory must be empty: $OUT" >&2; exit 1; fi
echo "Evidence: $OUT"
phase() {
  python3 "$HERE/../../../tools/native_phase.py" "$OUT" "$$" "$@" || true
}
timed_probe() {
  local name=$1 result=0
  local timing="$OUT/probe-timing/$name"
  mkdir -p "$timing" || true
  python3 "$HERE/../../../tools/native_phase.py" "$timing" "$$" command-start || true
  SIMCTL_CHILD_FC_PROBE_TIMING_PATH="$timing/native-phases.jsonl" \
    xcrun simctl spawn "$UDID" "$OUT/$name" || result=$?
  python3 "$HERE/../../../tools/native_phase.py" "$timing" "$$" command-finished "$result" || true
  return "$result"
}

phase native-build
if xcrun clang -O2 "$HERE/../../../tools/native_sampler_files.c" -o "$OUT/sampler_files" > "$OUT/sampler-files-build.log" 2>&1 \
    && "$OUT/sampler_files" --ready > "$OUT/sampler-files-ready.json"; then
  export FC_SAMPLER_FILES="$OUT/sampler_files"
else
  echo "Sampler file observations unavailable; see sampler-files-build.log" >&2
  unset FC_SAMPLER_FILES
fi
SDK=$(xcrun --sdk iphonesimulator --show-sdk-path)
CC=(xcrun clang -target arm64-apple-ios18.0-simulator -isysroot "$SDK")
"${CC[@]}" -c -o "$OUT/vclock.o" "$HERE/vclock.c"
"${CC[@]}" -c -fobjc-arc -o "$OUT/dispatch_activity.o" "$HERE/dispatch_activity.m"
"${CC[@]}" -c -fobjc-arc -o "$OUT/nstimer_activity.o" "$HERE/nstimer_activity.m"
"${CC[@]}" -dynamiclib -framework Foundation -o "$OUT/vclock.dylib" "$OUT/vclock.o" "$OUT/dispatch_activity.o" "$OUT/nstimer_activity.o"
"${CC[@]}" -fobjc-arc -dynamiclib -lobjc -Wl,-undefined,dynamic_lookup -o "$OUT/ui_schedule.dylib" "$HERE/ui_schedule.m"
"${CC[@]}" -fobjc-arc -framework Foundation -framework QuartzCore -o "$OUT/clocks" "$HERE/clocks.m"
"${CC[@]}" -O2 -o "$OUT/posix_clocks" "$HERE/posix_clocks.c"
"${CC[@]}" -O2 -o "$OUT/host_clock_scope" "$HERE/host_clock_scope.c"
"${CC[@]}" -fobjc-arc -framework Foundation -framework QuartzCore -o "$OUT/step" "$HERE/step.m"
"${CC[@]}" -fobjc-arc -framework Foundation -framework QuartzCore -o "$OUT/dispatch_work" "$HERE/dispatch_work.m"
"${CC[@]}" -O2 -fobjc-arc -framework Foundation -o "$OUT/dispatch_wait" "$HERE/dispatch_wait.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/dispatch_capture" "$HERE/dispatch_capture.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/dispatch_sync_work" "$HERE/dispatch_sync_work.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/source_registration" "$HERE/source_registration.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/source_release" "$HERE/source_release.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/source_cancel" "$HERE/source_cancel.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/source_data" "$HERE/source_data.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/dispatch_after_work" "$HERE/dispatch_after_work.m"
"${CC[@]}" -fobjc-arc -framework Foundation -framework QuartzCore -o "$OUT/callback_work" "$HERE/callback_work.m"
"${CC[@]}" -fobjc-arc -framework Foundation -framework QuartzCore -o "$OUT/nstimer_work" "$HERE/nstimer_work.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/deadline_work" "$HERE/deadline_work.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/nstimer_return" "$HERE/nstimer_return.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/nstimer_activation" "$HERE/nstimer_activation.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/nstimer_mutation" "$HERE/nstimer_mutation.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/nstimer_admission" "$HERE/nstimer_admission.m"
"${CC[@]}" -fobjc-arc -framework Foundation -o "$OUT/nstimer_teardown" "$HERE/nstimer_teardown.m"
"${CC[@]}" -dynamiclib -framework CoreFoundation -o "$OUT/nstimer_read_probe.dylib" "$HERE/nstimer_read_probe.c"
mkdir -p "$OUT/fade.app"
cp "$HERE/Info.plist" "$OUT/fade.app/"
"${CC[@]}" -fobjc-arc -framework UIKit -framework QuartzCore -o "$OUT/fade.app/fade" "$HERE/fade.m"
codesign -s - -f "$OUT/vclock.dylib" "$OUT/ui_schedule.dylib" "$OUT/clocks" "$OUT/step" "$OUT/dispatch_work" "$OUT/dispatch_capture" "$OUT/source_release" "$OUT/source_cancel" "$OUT/source_data" "$OUT/dispatch_after_work" "$OUT/callback_work" "$OUT/nstimer_work" "$OUT/nstimer_return" "$OUT/nstimer_mutation" "$OUT/nstimer_admission" "$OUT/nstimer_teardown" "$OUT/nstimer_read_probe.dylib" "$OUT/deadline_work" "$OUT/fade.app" 2>/dev/null
codesign -s - -f "$OUT/source_registration" "$OUT/nstimer_activation" "$OUT/dispatch_sync_work" "$OUT/dispatch_wait" 2>/dev/null
codesign -s - -f "$OUT/posix_clocks" 2>/dev/null

phase simulator-create
UDID=$(xcrun simctl create s18-native-time "iPhone 16" "$RUNTIME")
printf '%s\n' "$UDID" > "$OUT/simulator-id"
collect_app_evidence() {
  local container
  if container=$(xcrun simctl get_app_container "$UDID" dev.frontcap.s18.uitests.xctrunner data 2>/dev/null); then
    if [ -d "$container/Documents/native-proof" ]; then cp "$container/Documents/native-proof/"* "$OUT/" || true; fi
  fi
  if container=$(xcrun simctl get_app_container "$UDID" dev.frontcap.s18.probe data 2>/dev/null); then
    for name in dispatch-pending.txt input-events.txt input-events-real.txt ui-schedule-manual.txt ui-schedule-real.txt clock-advances.jsonl clock-control.json clock-checkpoint.json native-console.log native-timers.txt; do
      if [ -f "$container/Documents/$name" ]; then cp "$container/Documents/$name" "$OUT/" || true; fi
    done
  fi
}
SAMPLE_PID=
stop_diagnostics() {
  if [ -n "$SAMPLE_PID" ]; then
    kill "$SAMPLE_PID" >/dev/null 2>&1 || true
    wait "$SAMPLE_PID" 2>/dev/null || true
    SAMPLE_PID=
  fi
}
cleanup() {
  local result=$?
  phase cleanup "$result"
  stop_diagnostics
  collect_app_evidence
  xcrun simctl shutdown "$UDID" >/dev/null 2>&1 || true
  xcrun simctl delete "$UDID" >/dev/null 2>&1 || true
  phase finished "$result"
  return "$result"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
phase simulator-boot
xcrun simctl boot "$UDID"
xcrun simctl bootstatus "$UDID" >/dev/null

phase native-fixtures
echo "== POSIX elapsed clocks, native CPU budgets and invalid IDs"
xcrun simctl spawn "$UDID" "$OUT/posix_clocks" real | tee "$OUT/posix-real.jsonl"
SIMCTL_CHILD_FC_SCALE=10 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/posix_clocks" scaled | tee "$OUT/posix-scaled.jsonl"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/posix_clocks" manual | tee "$OUT/posix-manual.jsonl"
echo "== host-clock scopes on fresh threads"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/host_clock_scope" | tee "$OUT/host-clock-scope.log"
echo "== clocks, real clock"
xcrun simctl spawn "$UDID" "$OUT/clocks" | tee "$OUT/real.json"
echo "== workloop timers, real clock"
SIMCTL_CHILD_FC_WORKLOOP=1 SIMCTL_CHILD_FC_WORKLOOP_WALL=1 SIMCTL_CHILD_FC_WORKLOOP_WALL_SOURCE=1 \
  xcrun simctl spawn "$UDID" "$OUT/clocks" | tee "$OUT/workloop-real.json"
echo "== clocks, vclock at 10x"
SIMCTL_CHILD_FC_SCALE=10 SIMCTL_CHILD_FC_REPORT=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/clocks" | tee "$OUT/scaled.json"

echo "== native timer rearming, callback acknowledgments, repetition and cancellation"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_FC_HOLD_US=${FC_HOLD_US:-200000} \
  SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/step" | tee "$OUT/step.jsonl"

echo "== the same native timer boundaries on a dispatch workloop"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_FC_WORKLOOP=1 SIMCTL_CHILD_FC_HOLD_US=${FC_HOLD_US:-200000} \
  SIMCTL_CHILD_FC_TRACE_TIMERS="$OUT/step-workloop-timers.txt" \
  SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/step" | tee "$OUT/step-workloop.jsonl"

echo "== queued and transitive async work while the native clock is frozen"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  SIMCTL_CHILD_FC_TRACE_ACTIVITY="$OUT/dispatch-pending-control.txt" xcrun simctl spawn "$UDID" "$OUT/dispatch_work" | tee "$OUT/dispatch_work.jsonl"

echo "== background dependency waits, concurrent controllers and capture retirement"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  timed_probe dispatch_wait | tee "$OUT/dispatch_wait.jsonl"

echo "== native delayed work admitted before queue entry"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  timed_probe dispatch_after_work | tee "$OUT/dispatch_after_work.jsonl"

echo "== owned captures after one-shot dispatch bodies"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  SIMCTL_CHILD_FC_TRACE_ACTIVITY="$OUT/dispatch-capture-pending.txt" \
  timed_probe dispatch_capture | tee "$OUT/dispatch_capture.jsonl"

echo "== synchronous dispatch admission, native ordering and borrowed lifetimes"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  SIMCTL_CHILD_FC_TRACE_ACTIVITY="$OUT/dispatch-sync-pending.txt" \
  timed_probe dispatch_sync_work | tee "$OUT/dispatch_sync_work.jsonl"

echo "== coalesced data source readiness before callback entry"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  timed_probe source_data | tee "$OUT/source_data.jsonl"

echo "== source cancellation before callback entry"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  SIMCTL_CHILD_FC_TRACE_ACTIVITY="$OUT/source-cancel-pending.txt" \
  timed_probe source_cancel | tee "$OUT/source_cancel.jsonl"

echo "== source handler capture retirement"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/source_release" | tee "$OUT/source_release.jsonl"

echo "== entered delayed/source callbacks and their descendants"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  SIMCTL_CHILD_FC_TRACE_ACTIVITY="$OUT/registration-pending.txt" \
  xcrun simctl spawn "$UDID" "$OUT/callback_work" | tee "$OUT/callback_work.jsonl"

echo "== activated source registration before callback entry"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  SIMCTL_CHILD_FC_TRACE_ACTIVITY="$OUT/source-registration-pending.txt" \
  xcrun simctl spawn "$UDID" "$OUT/source_registration" | tee "$OUT/source_registration.jsonl"

echo "== NSTimer factory callbacks and their descendants"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/nstimer_work" | tee "$OUT/nstimer_work.jsonl"

echo "== NSTimer completion after native rearm and run-loop exit"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/nstimer_return" | tee "$OUT/nstimer_return.jsonl"

echo "== entered NSTimer callbacks at tracking activation"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  SIMCTL_CHILD_FC_TRACE_ACTIVITY="$OUT/nstimer-activation-pending.txt" \
  xcrun simctl spawn "$UDID" "$OUT/nstimer_activation" | tee "$OUT/nstimer_activation.jsonl"

echo "== NSTimer mutation callouts and clock-boundary exclusion"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/nstimer_mutation" | tee "$OUT/nstimer_mutation.jsonl"

echo "== due NSTimer admission before native callback entry"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/nstimer_read_probe.dylib:$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/nstimer_admission" | tee "$OUT/nstimer_admission.jsonl"

echo "== NSTimer memberships after native loop destruction"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  SIMCTL_CHILD_FC_TRACE_ACTIVITY="$OUT/nstimer-teardown-pending.txt" xcrun simctl spawn "$UDID" "$OUT/nstimer_teardown" | tee "$OUT/nstimer_teardown.jsonl"

echo "== due deadlines before callback entry"
SIMCTL_CHILD_FC_MANUAL=1 SIMCTL_CHILD_DYLD_INSERT_LIBRARIES="$OUT/vclock.dylib" \
  xcrun simctl spawn "$UDID" "$OUT/deadline_work" | tee "$OUT/deadline_work.jsonl"

phase fade
echo "== fade held at 1.0 s then 3.0 s of a 4 s linear fade"
xcrun simctl install "$UDID" "$OUT/fade.app"
xcrun simctl launch --console-pty "$UDID" dev.frontcap.s18.fade > "$OUT/fade.log" 2>&1 &
python3 "$HERE/fade_probe.py" "$OUT" "$UDID"
xcrun simctl terminate "$UDID" dev.frontcap.s18.fade

phase app-build
echo "== installed iOS app input and screen evidence"
xcodebuild build-for-testing -project "$HERE/NativeProbe.xcodeproj" -scheme NativeProbe \
  -destination "platform=iOS Simulator,id=$UDID" -derivedDataPath "$OUT/xcode" \
  CODE_SIGNING_ALLOWED=NO > "$OUT/app-build.log" 2>&1
cp "$OUT/vclock.dylib" "$OUT/xcode/Build/Products/Debug-iphonesimulator/NativeProbe.app/"
cp "$OUT/ui_schedule.dylib" "$OUT/xcode/Build/Products/Debug-iphonesimulator/NativeProbe.app/"
APP_RESULT=0
UI_TESTS=(-only-testing:NativeProbeTests/NativeProbeTests/testTapChangesState)
phase xctest
if [ "${FC_IOS_VIRTUAL_UI:-0}" = 1 ]; then
  UI_TESTS+=(-only-testing:NativeProbeTests/NativeProbeTests/testVirtualTimeInput)
  python3 "$HERE/../../../tools/native_ui_diagnostics.py" "$UDID" "$OUT" > "$OUT/ui-diagnostics.log" 2>&1 &
  SAMPLE_PID=$!
fi
# Xcode verbose sysdiagnose can stall after tests finish; our failure collectors are bounded.
UI_COMMAND=(xcodebuild test-without-building -project "$HERE/NativeProbe.xcodeproj" -scheme NativeProbe \
  -destination "platform=iOS Simulator,id=$UDID" -parallel-testing-enabled NO -collect-test-diagnostics never \
  -derivedDataPath "$OUT/xcode" -resultBundlePath "$OUT/app.xcresult" \
  CODE_SIGNING_ALLOWED=NO "${UI_TESTS[@]}")
if [ "${FC_IOS_INPUT_INTERRUPTION:-}" = xctest ]; then
  python3 "$HERE/../../../tools/native_ios_xctest.py" --simulator "$UDID" --directory "$OUT" \
    --hold-us "${FC_HOLD_US:-500000}" -- "${UI_COMMAND[@]}" > "$OUT/host-ui.log" 2>&1 || APP_RESULT=$?
elif [ "${FC_IOS_VIRTUAL_UI:-0}" = 1 ]; then
  python3 "$HERE/../../../tools/native_ios_app.py" --simulator "$UDID" --directory "$OUT" \
    --hold-us "${FC_HOLD_US:-500000}" -- "${UI_COMMAND[@]}" > "$OUT/host-ui.log" 2>&1 || APP_RESULT=$?
else
  "${UI_COMMAND[@]}" > "$OUT/app-test.log" 2>&1 || APP_RESULT=$?
fi
phase app-evidence "$APP_RESULT"
stop_diagnostics
collect_app_evidence
if [ "$APP_RESULT" -ne 0 ]; then exit "$APP_RESULT"; fi
python3 - "$OUT" "${FC_IOS_VIRTUAL_UI:-0}" "${FC_IOS_INPUT_INTERRUPTION:-}" <<'PYAPP'
import hashlib, json, sys
from pathlib import Path
from PIL import Image
out = Path(sys.argv[1])
for prefix in (["app", "virtual", "clock-lifecycle"] if sys.argv[2] == "1" and not sys.argv[3] else ["app"]):
    state = json.loads((out / (prefix + "-state.json")).read_text())
    observations = [row['capture']['result'] for row in state['cases']] if prefix == 'clock-lifecycle' else state['observations']
    for row in observations:
        image = Image.open(out / (row["phase"] + ".png")).convert("RGB")
        width, height = row["screen_points"]
        x, y = row["sample"]
        row["pixel"] = list(image.getpixel((int(x * image.width / width), int(y * image.height / height))))
        row["size"] = list(image.size)
    (out / (prefix + ".json")).write_text(json.dumps(state, indent=2))
if sys.argv[3] == 'xctest':
    root = out/'xctest-recovery'
    for case in json.loads((root/'report.json').read_text())['cases']:
        directory = root/case['phase']
        screens = {}
        for capture in case['captures']:
            row = capture['result']; path = directory/'new-ui'/(row['phase']+'.png')
            image = Image.open(path).convert('RGB')
            width, height = row['screen_points']; x,y = row['sample']
            screens[row['phase']] = dict(sha256=hashlib.sha256(path.read_bytes()).hexdigest(),
                pixel=list(image.getpixel((int(x*image.width/width),int(y*image.height/height)))),
                size=list(image.size), sample=row['sample'], screen_points=row['screen_points'])
        (directory/'screens.json').write_text(json.dumps(screens,indent=2)+'\n')
elif sys.argv[2] == "1":
    path = out/'controller-recovery/app.json'
    state = json.loads(path.read_text())
    row = state['capture']
    image = Image.open(out/(row['phase']+'.png')).convert('RGB')
    width, height = row['screen_points']; x, y = row['sample']
    row['pixel'] = list(image.getpixel((int(x*image.width/width), int(y*image.height/height))))
    path.write_text(json.dumps(state, indent=2))
    path = out/'ui-recovery/report.json'
    state = json.loads(path.read_text())
    for case in state['cases']:
        row = case['capture']['result']
        image = Image.open(out/(row['phase']+'.png')).convert('RGB')
        width, height = row['screen_points']; x, y = row['sample']
        row['pixel'] = list(image.getpixel((int(x*image.width/width), int(y*image.height/height))))
    path.write_text(json.dumps(state, indent=2))
PYAPP
xcrun simctl uninstall "$UDID" dev.frontcap.s18.probe
xcrun simctl uninstall "$UDID" dev.frontcap.s18.uitests.xctrunner
