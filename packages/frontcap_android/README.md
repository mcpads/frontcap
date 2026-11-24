# Source-owned Android Activity testing

Instrument an Android test build to inspect its View hierarchy, tap an observed control, edit committed text, scroll a window and advance the Activity clock.

## Attach and retire

Compile all Java sources under `src/dev/frontcap/s18/` together with
`Steps.java` and `ClockControl.java` from `spikes/s18-native-time/android/src/dev/frontcap/s18`. The shared Java package deliberately retains access to the existing runtime methods and JNI names;
it is not a new clock implementation. Controlled builds also package the existing `libvclock.so`
and `wrap.sh`, with `android:extractNativeLibs="true"` in the application manifest. Install controlled
APKs with `adb install --no-incremental`: the wrapper requires
non-incremental extraction for the wrapper. Ordinary builds do not attach or preload the runtime.

On the Activity main thread, when `FC_MANUAL=1`, create an attachment:

```java
clock = new dev.frontcap.s18.ActivityClock(this, () -> new org.json.JSONObject());
```

The callback may supply app-owned diagnostic state after a clock request. An empty object is valid
for clock-only qualification; it is not a UI observation. Call `clock.close()` from `onDestroy`. Closing twice is harmless. Each attachment issues a fresh generation; queued provider work is
fenced against a replaced attachment. This does not recover business state or adopt a lost process.

Declare a provider whose authority is the application package followed by `.clock`:

```xml
<provider android:name="dev.frontcap.s18.ActivityClockProvider"
    android:authorities="your.application.clock"
    android:exported="true" android:permission="android.permission.DUMP"/>
```

The provider also explicitly enforces DUMP permission in `call`; manifest permission alone is not
the call boundary. Submission posts to the bound Activity main queue. The returned `submitted`
flag is only admission to that queue. `files/clock-control.json` supplies the exact generation,
request, before/after time and completion. The broadcast receiver remains available under
`<package>.CLOCK_CONTROL`.

## Client and evidence

The existing `tools/native_android_control.py` accepts `--package` for either broadcast or provider
transport. Provider requests address `content://<package>.clock`. Its retained client cursor binds
both package and generation; changing the package or transport cannot redirect pending work. Older unbound cursor files are rejected rather than adopted. Frozen historical Runs keep their
matching controller implementation.

Read, explicit advance, same-request replay and receipt-only recovery use the existing protocol. A lost submission reply retains the pending request. Never allocate another advance merely because
transport timed out. Caller watchdogs use host time while app time is held. Native runtime,
producer and rendering limits are unchanged; clock completion is not global idleness or visible
frame completion.

## Read-only View observations

Compile the shared Java package with the attachment and declare
`dev.frontcap.s18.ViewObservationProvider` at `<package>.views`, exported with DUMP permission. `ActivityClock` binds/unbinds its source alongside the clock lifetime. The read operation accepts
`observe`, with `generation` and a canonical UUID `request` string. It checks DUMP itself, rejects
foreign generations before queueing, and discards queued work if the Activity attachment changes. A submitted reply is not a completed observation. Match the exact request, generation and PID in
`files/view-observation.json`; missing publication remains unavailable, never an empty UI success.

A completed read contains the current controlled `now_ns`, window focus, nullable IME-inset visibility
and a bounded projection of the actual View tree. Each row reports class, role, resource name where
available, content description, text, nullable `error`, own enabled/clickable/selected/focus flags, source visibility
and a view-object token. Tokens persist for objects retained across successful reads; a removed and
later reintroduced object can receive a new token. A token alone is not input authority. `error` reads the actual `TextView.getError()` message (at most 4,096 UTF-16 units); null means
no error is set. It is source validation state, not proof that an error popup has been presented. PasswordTransformationMethod text and errors are omitted. The projection includes instantiated offscreen Views,
not fabricated rows for unrealized adapter data. It does not expose Compose semantics or WebViews.

`layout_bounds_screen_px` combines the View's screen location and untransformed layout size. `transformed` reports any nonidentity ancestor/View matrix; these bounds are not a transformed hit
region. `visible` combines ancestor visibility/alpha and a nonempty global visible rectangle. Neither
that flag, `clickable`, nor window focus proves an unobscured input point or pixels in a screenshot. Global window overlap, hit testing, input delivery and capture correlation remain separate work.

The source rejects an unlaid-out window, excessive View/text counts or an oversized response rather
than truncating a catalog. Limits are 2,048 Views, 4,096 UTF-16 units for labels/resource names,
16,384 units for text and one MiB for the complete receipt. `unavailable` is a distinct result. [Core's typed decoder](../../crates/core/src/native/android_views.rs) checks owner/request identity,
roles, Boolean types, unique tokens, single source focus, string/geometry budgets and secure-text
redaction. The `android_views` adapter example independently validates a collected receipt against
its recorded request and owner.

This read-only endpoint has a single latest-publication file. Reissuing `observe`, even with the same
UUID, takes a new snapshot; it is not the clock's replay protocol. A collector must retain a matched
receipt before requesting another read. Observation itself authorizes no UI input and advances no
app time. Committed editing is a separate source operation
described below, alongside observed window taps and scrolling.

## Sequential source/screen capture

`tools/native_android_view_capture.py` collects one read-only capture for an explicitly configured
package, full Activity class, generation/PID and frozen-source digest. The caller supplies the owned
adb command function and a fresh output directory; device acquisition and the host lock remain with
the caller. It does not allocate clock requests or inject input. A verified catalog change across the screenshot
is a terminal unavailable read, with diagnostic files but no input targets. Recorder/MCP can request
a new observation in the same session; an exact-ID retry returns the original result. Clock/owner
changes and uncertain collection remain failures.

The collector reads PID/clock and source Views, checks the foreground Activity, captures a fresh
`screencap` PNG, and repeats the window/source/PID/clock checks. A complete capture requires matching
catalogs and app time; clock receipts must remain byte-identical. Initial source window focus and
both actual foreground components must match the configured app. Source catalog changes become
unavailable only after verifying all remaining evidence. A terminal read retains a manifest and digest; failed reads retain
`progress.json`, the original error and whatever request/receipt/window/image files were obtained. Changed state or uncertain submission never triggers another input or an automatic capture retry.

The manifest binds 13 named evidence files, source/owner identity, independent View request UUIDs,
controlled time, screenshot size and the host monotonic collection interval. The native adapter's
[verifier](../../crates/adapter-native/src/android_capture.rs) requires the separately retained
manifest digest and expected owner. It rereads regular non-symlink files, checks hashes/inventory,
decodes both typed View receipts, compares source and clock stamps, checks both foreground windows
and decodes the PNG under the image budget. A coherently rehashed file cannot hide contradictory
source/clock/window evidence. The `android_capture` example exposes this offline verification; its
optional `source-changed` argument explicitly verifies unavailable diagnostic evidence instead of
accepting it as an actionable capture.

This is sequential correlation, not an atomic snapshot or proof that every source View is already
painted. The screen can contain system UI and overlaps. Source visibility is not compositor
visibility, and neither a verified capture nor its View tokens authorize input. Installed consumers
still need visible-outcome checks for their workflows. The recorder/MCP integration retains and
verifies these captures; input requires separate operation-specific validation.

`frontcap_native::android_collection::Collector` runs this collector as a supervised Python child
for an already-owned emulator/app. Supply absolute Python/script/adb paths, the owner's serial,
independently retained app identity/source digest, a fresh evidence directory and a deadline/cancel
flag. The parent records the typed request and process exchange, retires its child process group,
then independently verifies the manifest and artifacts. Cleanup failure remains separately visible
to the device owner. It neither acquires a device nor retries a partial attempt. The caller must
retain emulator ownership and serialize captures against clock/input operations. This is an adapter
building block used by the profile-selected Android session and recorder/MCP acquisition.

## Committed text editing

The DUMP-protected `<package>.views` provider also accepts `edit` with `generation` and `payload_sha256`
strings. First stage bounded UTF-8 JSON in the app-private
`files/view-edit-<sha256>.json` file using the stdin-based submission helper. The provider checks the
file's SHA-256 before parsing, refuses linked files and limits the raw payload to 196,608 bytes. Staging has no UI effect. This avoids putting large Unicode strings in an adb command line. The JSON contains `generation`, `controller_id` (UUID),
positive sequential `request_id`, the latest source `observation` UUID, target View token, observed
`now_ns`, half-open UTF-16 `start`/`end` and replacement `text`. Each Activity generation binds one
controller. The generic recorder/MCP adapter exposes this as `replace_text` with `observed_view_text`. Use the recorder observation request ID for `observation` and the source token for `reference`; the
adapter resolves and validates the source basis and owns the low-level sequence. Its durable store
retains older completed outcomes, beyond the source endpoint's latest-result cache.

On the main thread the endpoint requires the same observation/time and unchanged source catalog,
then resolves the actual View object. Only a visible, enabled, untransformed, nonpassword EditText
with a nonnull key listener and actual source focus in a focused window can be edited. It rejects reversed/out-of-bounds ranges,
split surrogate pairs, malformed Unicode and results over 16,384 UTF-16 units before mutation. `edit_limit_utf16` in the source catalog also reports the minimum of that transport limit and the
field's actual LengthFilter limits. A tighter field limit rejects before mutation; the adapter does
not remove application/framework filters. A missing limit grants no editing authority. Delivery calls the field's Editable range replacement and records the actual resulting text; it does
not simulate IME composition, suggestions or hardware keys. Consumers must independently verify the
resulting observation and business state. Filters/watchers can affect the actual result; the typed
receipt verifier rejects a completion whose actual text differs from the requested replacement.

`files/view-edit.json` is authoritative only when request, generation and PID match. The provider's
submitted reply acknowledges queueing. Before editing it durably publishes `pending`; an exception
in mutation or final publication fences further edits in that Activity generation. A pending receipt
is uncertainty, never a successful edit or permission to resend. Completed and rejected attempts
consume a sequence number. Repeating the latest exact request republishes its retained result without
editing. Different arguments or skipped/old sequence numbers are refused. The endpoint retains only
the latest outcome; callers must preserve earlier receipts. An Activity replacement has a new
generation and cannot recover the prior process's unfinished mutation.

The Core `android_edit` types and the adapter's `android_edit` example validate retained requests,
owner identity, state fields and the actual UTF-16 replacement. Its setup does not qualify generic tap
authority or recorder/controller recovery.

## Observed window taps

The generic source provider accepts `tap` with `generation` and a bounded Base64-encoded JSON `request_base64` string (avoiding the content CLI extra delimiter). The
request binds generation/controller UUIDs, positive sequential `request_id`, latest source
`observation`, target token, held `now_ns` and integer screen-pixel `x`/`y`. This source operation is
exposed by recorder/MCP as `tap_target` when `observed_view_tap` is advertised. Use the completed
recorder observation request ID as `observation` and a source token as `reference`; the adapter
resolves coordinates from verified retained evidence and owns the source sequence. The catalog's optional `tap_point_screen_px` identifies a candidate
inside the currently clipped visible area of an enabled, clickable Button/EditText. It refuses
transformed targets and overlapping unrelated View branches rather than guessing their draw order. A point alone is not authorization: registration and actual DOWN revalidate the source basis.

Activity attachment wraps the existing `Window.Callback`, forwards ordinary callbacks and restores
the original callback on detach if it still owns the wrapper. No app click/touch listeners are
replaced. The provider publishes `armed` only after verifying the same observation/time, unchanged
catalog, focused window and supported point. The transport checks foreground Activity and known
keyboard overlap, then sends one real `adb shell input tap`. At the actual DOWN, the wrapper repeats
source validation and checks touchscreen/pointer/coordinates. A stale or unsupported DOWN is consumed
before forwarding to the app, with its corresponding UP suppressed and a durable no-effect rejection.

Before forwarding a valid DOWN, the source publishes `pending`. Only its matching UP at unchanged
app time can publish `completed`, recording the actual window handler's two Boolean results. Completion proves window delivery, not a click callback, navigation, business effect or presentation
settlement; observe the app separately. Malformed/cancelled streams or callback/publication failures
after DOWN remain unresolved. Pending tap/edit effects fence the other source input channel. A new
observation does not clear uncertainty. Retire the generation if no terminal receipt can be recovered.

`tools/native_android_view_tap.py` retains the full owner/request before registration and marks
physical dispatch before invoking input. Lost delivery replies only poll the matching receipt. A
reopened pending bridge never registers or touches again, including the gap before physical dispatch. Latest completed source registration retries republish the retained result without another touch;
callers retain older results separately. Core `android_tap` types validate identity and state/edge
consistency. Custom interception, other system overlays and business effects need their own evidence;
no generic Compose, multiwindow, arbitrary ViewGroup hit testing or IME composition is claimed.

## Observed source scrolling

`ViewScrolling` supplies observed `scroll_target` through recorder/MCP when `observed_view_scroll`
is advertised. Profiles configure `scroll_script` with the supervised Python bridge. Exact stock `ScrollView` and
`HorizontalScrollView` rows carry nullable `scroll`: current pixel offset, maximum, viewport and
available direction/start/end trajectories. Subclasses, lists and custom containers do not inherit
this support. Geometry follows the stock widget's child extent minus padded viewport. It describes
current source layout, not global idleness or completed inertia. Out-of-range offsets, disabled,
hidden, transformed, too-small or overlapping unrelated source branches advertise no gesture.

Register `scroll` at the existing `.views` DUMP provider with `generation` and Base64 JSON
`request_base64`. The request binds generation, controller, sequential ID, latest observation,
source target/time and the exact advertised start/end coordinates and direction. The window hook
revalidates before DOWN, suppresses a proven rejected stream, persists pending before application
callbacks and forwards the real gesture to the original window callback. Registered DOWN requests
unbuffered delivery so held app frame time does not defer MOVE events. Matching DOWN/MOVE/UP at held
time publishes `files/view-scroll.json` with handled flags, move count and source offsets before/after. No callback/listener replacement, accessibility scroll or direct `scrollTo` substitutes for delivery. Malformed/cancelled input after DOWN remains unresolved; new tap/edit/scroll work cannot pass that
fence. A completed gesture does not promise movement, animation settlement, or that a searched
record is visible: inspect a new catalog and correlated screen. A held fling can consume a later
tap; callers must explicitly advance app time for subsequent motion and verify navigation separately.

`tools/native_android_view_scroll.py` uses the shared touch registration/recovery transport with
one 600 ms `input swipe`. It reserves receipt-readback time when delivery times out, retains the
latest validated source receipt, and never re-registers or resends an unresolved gesture. Exact
completed retries return the retained result. `android_scroll` independently validates typed source
receipts; `android_views` and `android_capture` validate geometry and correlated evidence. Generic
scroll invocation uses the recorder observation request ID, source token and advertised direction;
coordinates and source offsets are resolved from saved evidence. New input requires a fresh
observation after scroll or clock advance.
