# UIKit test-build clock integration

Use this module to instrument a UIKit test build. Compile `clock.m` and the existing `spikes/s18-native-time/ios/clock_control.m` into the app. Build and embed the native `vclock.dylib` and `ui_schedule.dylib` runtimes, and launch with
`FC_MANUAL=1`, `FC_UC_HOST_TIME=1` and
`DYLD_INSERT_LIBRARIES=@executable_path/vclock.dylib:@executable_path/ui_schedule.dylib`.

Call `start_native_clock(metadata, diagnosticFiles)` once during app startup. Without `FC_MANUAL`
it returns nil and leaves ordinary behavior intact. In manual mode missing runtime symbols abort
startup. The returned descriptor identifies the app's loopback endpoint; its durable receipt is
written to `Documents/clock-control.json`.

Optional metadata supplies app diagnostics on reads and runs on the untracked control thread. Do
not access UIKit from it or use protocol field names. Diagnostic filenames select existing files
under Documents to retain when advance/drain fails.

Producer tracking is explicit: call `begin_native_clock_tracking()` at the chosen instrumentation
boundary. Starting the endpoint does not activate tracking; advances before tracking return an unapplied
`ENOTSUP` result. This only covers the declared
native runtime producers and does not establish global idleness. The clock module does not supply generic observation/input, an XCTest
bridge, service control or control over arbitrary native producers.

## XCTest observation and tap module

Compile `observed_ui.m` into the UI-test target, not the application. `FCObservedUI` enumerates
supported button, text-field, text-view, cell, link and switch targets from a public XCTest
snapshot, along with read-only static text. Static text reports its label/value, frame and selector
but never advertises `hittable`; it is not a tap candidate even when UIKit could hit its geometry. The module rejects direct static-text taps as `unsupported_target`. For supported input roles,
it queries live hittability only for unique, enabled selectors intersecting the screen. Each target
reports role, identifier, label, value, enabled/hittable state and frame in thousandths of a point. Offscreen cells remain discoverable, but their descendants are not traversed until the cell
intersects the screen. UIKit's virtualized offscreen child snapshots can report recycled text and
frames from another row even between two snapshot-only reads. Those descendants are not reliable
observations of rendered content. All published rows still participate in exact before/after checks. Uniqueness in the catalog describes this projection; live queries over the full app must also find
exactly one match before an input target becomes hittable and again before delivery. A projection above 2,048 hierarchy elements is rejected rather than silently truncated.

The caller supplies a verified live generation/time context. Observing invalidates the previous
basis and issues new observation-scoped references. Tap consumes its basis, requires matching context
and exactly one identified target, then re-reads the catalog and keyboard state before dispatch. Resolution uses type plus identifier (or label when no identifier exists), never an index from a
previous hierarchy. Ambiguous, disabled, unhittable, unknown or changed targets return `not_applied`. XCTest errors during tap propagate; the caller must preserve dispatch uncertainty.

The observation module does not own lifecycle/generation verification, clock reads, screenshots,
durable request identity or transport. A repeated call with a consumed observation rejects; that is not a substitute for replaying the
original durable request result. Sequential XCTest checks do not imply an atomic UIKit snapshot,
native display presentation or arbitrary toolkit support.

The two consumer `observed-targets.json` profiles run focused installed XCTest checks through
`tools/ios_xctest_probe.py`. Their test selectors stay in consumer code; the shared implementation
contains no app bundle, screen name or business action.

## Shared generic bridge

Use `FCClockWindow` for the instrumented app window. In manual mode it exposes the clock endpoint
and current time through accessibility metadata, without consuming clock request IDs or depending
on a business label. Ordinary mode retains the superclass accessibility value.

The UI-test target includes `ui_bridge.h` and calls `serve_uikit_ui` with the app bundle, launch
arguments, additional environment and its owned output directory. The caller installs the native
XCTest animation-wait control. The server supports launch, target observation, observed tap,
termination and finish. Captures retain PNG/hierarchy hashes; clock and catalog checks bracket
capture publication. Catalogs include typed `text_input` first-responder state. The bridge enables
`FC_UI_INSPECT`, binds that state into the observation context and checks it again around capture. A changed focus/selection/text invalidates the old context. Catalogs also expose `transitioning`
from active source view-controller transition coordinators (bounded traversal of 2,048 controllers). Recorder/bridge admission refuses new taps during a transition, even when XCTest reports hittability;
callers can explicitly advance time and reobserve within their chosen budget. This is not a general
animation or display-settlement guarantee. Post-tap verification compares only
clock identity/time, since a successful tap can legitimately change focus. These sequential checks
do not make capture atomic.

`tools/native_ios_generic_ui_bridge.py` connects this server to the durable bridge protocol. Its
owner records the UIKit surface, so a saved client cannot switch to the fixture protocol. Published
receipts resolve lost replies; an unresolved dispatch remains fenced. Generic replies have an
explicit 1 MiB transport budget; legacy replies retain their existing budget.

The initial runner descriptor advertises `system_alerts`. `observe_system_alerts` records bounded
SpringBoard alert text/buttons and PNG/hierarchy evidence without querying app foreground/clock
metadata. `tap_system_button` accepts only a fresh stable observation and its actionable button
reference, rechecks the alert hierarchy and consumes that observation before delivery. App UI work
displaces system input bases. These are sequential runner/device observations, not an app-clock or
atomic system-UI guarantee.

For installed bridge qualification, add `--generic-ui --source-sha256 <frozen-archive-sha256>` to
the clock probe command. It exercises both consumers, held time, stale-time rejection, successful
tap, discarded-reply recovery and reopening the Python bridge store. This does not qualify MCP
process recovery. Range editing and scrolling
remain unfinished. Static-text discovery and its
admission checks pass both consumers. Recorder and executor reject new taps on unknown, unready
or read-only references before allocating intents; exact existing IDs retain their saved recovery path.

## Text focus inspection

The optional
`FC_UI_INSPECT=1` test launch exposes `text_input` in the window accessibility value. This works
without manual-clock injection and does not enable a mutation endpoint.

The source hook walks the window's view tree on the main thread (2,048-view budget), identifies the
actual first responder, and reports supported UITextField/UITextView text, a view-lifetime token,
selection offsets and marked-text presence. Text is bounded to 16,384 UTF-16 units. Unsupported,
secure, read-only and unavailable inputs have explicit states; secure values are not exported. The opt-in editing endpoint below has a separate ordinary test profile. Generic observations now
carry the source state through Python and Rust validation to MCP. This is source focus evidence;
it does not assign an editable target reference or resolve duplicate labels. The timed-press check
below qualifies held-time source editing; duplicate-label editing remains unqualified. No text editing MCP operation is claimed yet.

The retained report compares source first-responder state with
both live and snapshot XCTest focus attributes.

## Committed text endpoint

`FC_UI_EDIT=1` enables focus inspection and an app-owned loopback endpoint, exposed as
`text_endpoint` in window metadata. Its worker dispatches UIKit input on the main thread; it does
not reuse the clock or XCTest control thread. `replace_text` carries the exact focused-state basis,
UTF-16 `start`/`end` offsets and replacement `text`. A changed responder, text or selection rejects
before input, as do active marked text, split surrogate pairs and a result exceeding 16,384 units. The operation selects the range and calls UIKit `insertText:`; its reply reports actual before/after
state, not a promise that an app delegate accepted or persisted the text.

Requests bind a generation and increasing integer request ID. The latest identical request returns
its saved result without dispatch; conflicting, retired and skipped IDs reject. A receipt is written
before dispatch and after completion to `Documents/native-text/text-control.json`; restarting the app
creates a new generation, not continuation of an unresolved operation. Request/reply bounds are
256 KiB/1 MiB, while existing control requests keep their 8 KiB bound.

This ordinary installed check exercises Korean/emoji ranges, rejection controls, duplicate replay,
the maximum text size and saved content after app relaunch. The generic bridge now binds editable
references and routes committed range edits through recorder/MCP as described below. The timed-press
check adds held-time source-editing evidence.

## Explicit time advancement during a text-field press

`FCObservedUI.pressReference` reuses observed-target readiness and consumes the observation. The
caller supplies the exact reserved clock request, app endpoint and source input-boundary descriptor. The helper presses for one host second, observes the next source-delivered touch begin, and sends
that one clock advance. It verifies the matching source touch end and clock receipt. The requested
virtual delta is explicit; a regular `tapReference` never advances time.

The generic XCTest server exposes this helper through `press_target` with `reference`, `observation`
and `clock_request` arguments. Its Python bridge and Rust receipt types validate the source begin/end
records and exact clock effect. This wire operation requires caller-owned clock reservation and
reconciliation.

`FCClockWindow` publishes a generation-bound input file after event delivery. Records include a
serial, virtual instant and the nearest UITextField/UITextView ancestor's selector and role. The
controller reads that file while XCTest is delivering the press; polling does not consume clock
IDs. Missing, skipped, wrong-target or unordered delivery is unresolved after input may have begun. A replayed clock receipt cannot prove a new timed press. The result preserves the raw clock outcome
even when ordering or advance fails. It does not authorize retrying the gesture.

UIKit profiles now advertise `observed_uikit_press`. MCP `press_target` accepts `launch_id`,
`request_id`, `reference`, `observation`, explicit `advance_ms` and optional `budget_ms`. The recorder
binds both channel requests, reserves the clock before UI dispatch and retains both outcomes. A
completed no-effect rejection releases only its own unused reservation; uncertain dispatch stays
fenced. Unchanged caller-ID retries read retained results. Observe again to verify actual focus;
press delivery alone does not promise focus or settled keyboard/presentation. The clock bridge's `reserve` call records
an unbound external advance without sending it; `reserve_ui` binds an exact UI request and enables
`release_ui` only with its committed no-effect receipt and an unchanged target clock cursor. Its
`recover` call can consume the exact published outcome. That acknowledgement is not permission to
repeat a press. The tested 70 ms is a declared qualification input, not a universal focus threshold.

## Observed range editing through MCP

UIKit profiles advertise `observed_uikit_text`. After observing a focused editable target, call
`replace_text` with `launch_id`, `request_id`, `reference`, `observation`, UTF-16 `start`/`end`,
replacement `text` and optional `budget_ms`. Offsets describe a half-open range. The complete
result must fit 16,384 UTF-16 units. Active composition and surrogate-splitting ranges reject.

The bridge retains the exact observed focus basis and clock before admission. The XCTest server
consumes the reference and sends one generation-bound source request through `text_delivery.h`. A lost source reply may be recovered from its exact published receipt; it never triggers a second
transport send. The recorder validates the original UI receipt and source basis before completing. Unchanged caller-ID retries return the recorded outcome. Missing evidence stays unresolved.

Editing does not advance virtual time. The actual source result can reflect delegate formatting
or loss of focus; it does not establish saved business data, IME composition, keyboard presentation
or navigation completion.

## Observed scrolling

UIKit profiles advertise `observed_uikit_scroll`. `scroll_target` accepts a fresh `reference`,
`observation` and `direction` (`up`, `down`, `left`, `right`) alongside the usual launch/request IDs. Use the matching `catalog.targets[].reference` for input; `catalog.scrolls[].token` identifies source
geometry for readback and continuity. Directions describe movement through content: `down` sends one slow upward XCTest swipe. There is no
implicit clock advance, settle loop or automatic second swipe. The existing signature-checked
XCTest wait hook skips only the owned app's post-event global idle wait during this swipe; pre-event
checks and other apps retain their normal behavior. Missing hook support rejects before input. This is input-delivery completion, not a claim of global idleness. Observe again after explicit time
advancement when motion needs to finish.

`FCClockWindow` reports named visible UIScrollViews in `catalog.scrolls`, independently of XCTest
hittability. Table, collection, scroll-view and text-view roles carry a view-lifetime token, selector,
content offset, minimum/maximum offset and viewport dimensions in thousandths of a point, plus
enabled/dragging/decelerating flags. Scroll views need an accessibility identifier or label. The
source extent includes adjusted content insets. A direction is available only toward a different
bound, with a positive viewport, enabled interaction and no active drag/deceleration or overscroll. The observed target must also be unique and hittable and resolve to exactly one source scroll view.

The UI server consumes the observation and returns actual before/after geometry. Python and Rust
bind the before-state to the retained observation; raw result verification preserves the source token
and exact direction. Acknowledgement does not promise movement. Zero displacement is not finite-end
evidence; check the source bounds. Missing completion remains unresolved and never authorizes a new
swipe. Search must distinguish found, finite end, unavailable action and exhausted action budget.

An `observe` during source scroll motion returns `observation_unavailable` with reason
`scroll_in_motion` and no capture. If geometry/hierarchy changes during capture, reason
`capture_changed` retains the diagnostic files but issues no usable target references. These are
completed reads, not uncertain input and not successful actionable observations. Callers may advance
time explicitly and observe again within their declared budget. The last unavailable observation
invalidates prior input authority. Missing app identity or malformed source output remains a fault.
