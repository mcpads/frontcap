# Android WebView clock capability probe

An opt-in experiment, not a qualification gate or product adapter. Requires Node with built-in
WebSocket, Python, an Android SDK/AVD and a disposable APK whose single WebView enables debugging.
The host must create the WebView without further user input. The probe replaces its document with
`about:blank`; never attach it to a user's live app or a shared debugging endpoint.

Run on the Android host from a checkout containing `tools/native_emulator.py`:

```sh
python3 spikes/android-webview-clock/run.py \
  --apk /absolute/path/to/disposable-host.apk \
  --component com.example.host/.MainActivity \
  --avd Medium_Phone_API_35 \
  --output /absolute/path/to/new-evidence-directory
```

If host startup needs a local page, supply `--reverse-port PORT` for the existing loopback service.
The runner uses the shared native lock/fence, a read-only owned emulator, PID-specific debugger
socket and allocated local forwarding port. It removes forwarding and retires the emulator even
if measurement fails. An unretired previous owner blocks launch. Evidence directories are exclusive.

`probe.mjs` can also run against an already exclusively owned forwarded endpoint:

```sh
node spikes/android-webview-clock/probe.mjs http://127.0.0.1:PORT /absolute/path/clock.json
```

The output retains runtime identity, commands/replies, events and before/after observations for:

- ordinary timer delivery;
- timer, animation-frame, Date and performance-clock behavior during a wall-time hold;
- a 100 ms virtual budget and a second wall-time hold after budget expiry;
- availability of explicit virtual-time readback and headless frame dispatch.

`completed` means the measurement completed, **not** that the runtime meets a stopped-world contract.
Read deltas and unsupported commands. Frames may continue while timers and clock reads stop.
This probe does not inspect workers, audio, native messages, screenshots, navigation continuity,
app startup isolation, or actual game behavior. It is not added to default CI.

## Source-owned animation callback experiment

`--frame-gate` installs `frame-gate.js` through the Android WebView's document-start script hook.
The probe combines ordinary CDP timer control with one explicitly admitted animation frame. The
source wrapper retains the browser's native callback dispatch and microtask checkpoints, intercepts
registration/cancellation, and passes the declared timestamp to the app. Nested registrations wait
for the next explicit frame. Callback exceptions retain native reporting and appear in the result.

This controls participating JavaScript animation callbacks only. It does not stop composition, CSS
animation, workers, audio, or native messages. No `frame`-level product capability is advertised.
Installing after application startup cannot recall callbacks already scheduled through the original
API. A hidden/background WebView may not deliver an admitted native frame; a caller timeout is then
an uncertain pending operation, not proof that nothing executed. The spike has no durable request
journal and must not be exposed as a recoverable production control operation yet.

The focused callback checks run with:

```sh
node --test spikes/android-webview-clock/frame-gate.test.mjs
```

For WKWebView, build a disposable native host to load `http://127.0.0.1:18764/` on startup and allow
local networking. The source fixture loads the same wrapper before requesting animation callbacks:

```sh
python3 spikes/android-webview-clock/run_ios.py \
  --app /absolute/path/to/Runner.app \
  --bundle-id com.example.host \
  --runtime com.apple.CoreSimulator.SimRuntime.iOS-26-1 \
  --device-type com.apple.CoreSimulator.SimDeviceType.iPhone-16 \
  --output /absolute/path/to/new-evidence-directory
```

The runner owns a fresh simulator and a temporary loopback fixture server under the same native
lock/fence, captures a native screenshot after the report, and retires both on exit. `frame.json`
records callback and canvas-pixel checks and Date/performance samples. The iOS experiment deliberately
leaves timers and Date/performance uncontrolled; it cannot qualify a held game clock. Its final native
screenshot complements canvas readback; it is not an atomic state-and-presentation observation.

## Receipt-bound frame requests

The same gate now offers `receipt()` and `dispatch({document, id, timestamp})` for adapter
integration. `receipt()` identifies the current document and returns its latest producer request.
Start IDs at 1 and advance monotonically by one. Repeating the latest ID with identical arguments
returns its pending/completed/unresolved record without delivering another frame; conflicting,
old, skipped or foreign-document requests are rejected. The controller must retain completed
receipts before issuing the next request, since the page keeps only the latest one. Page destruction
loses this in-memory evidence; it is not a cross-process durable store.

Native scheduling can partially apply before failure. Such a request remains unresolved and blocks
new requests. Callback exceptions retain the existing native reporting and completed batch errors.
Direct legacy `advance` and receipt-bound dispatch cannot be mixed in one document. Existing spike
runners still use the legacy interface and do not acquire recovery guarantees automatically.

The Rust Android page adapter now journals script installation before navigating to the selected
caller's explicit entry URL. Preparation completion establishes neither loaded consumer content nor
paused timers. CDP timer budget and native frame admission remain separate effects; installed
clock/control integration still requires their explicit phases and recovery policy.

## Passive timer observation

`timer-observer.js` is installed with the Android frame/pointer/navigation attachment before app
scripts. It observes function timers while retaining the native scheduler and IDs. It bounds its
bookkeeping and returned rows, and reports untracked registrations, opaque cancellations, replaced
methods and observation failures. Read/observe exposes the document-bound `snapshot.timer_activity`;
retained progress inspection does not query it.

Run the local delegation/bookkeeping checks with
`node --test spikes/android-webview-clock/timer-observer.test.mjs` from the repository root.
These use a host double and do not qualify installed WebView behavior. Existing clock/frame evidence
predates this observer; combine its installed verification with the next affected consumer journey.
