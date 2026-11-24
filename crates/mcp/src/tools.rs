//! The tools this server offers: typed arguments (their JSON schemas are what agents see) and the table
//! of descriptions. Durations are integer milliseconds; reported instants are microseconds (`t_us`).
use crate::server::{CallToolResult, ToolDescription};
use frontcap_core::ids::{LaunchId, RequestId, WatchName};
use frontcap_core::predicate::{Locator, Predicate};
use frontcap_core::rules::{FailReason, MockSpec, RuleSet};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoArgs {}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Determinism {
    #[default]
    Time,
    Frame,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Host {
    #[default]
    Local,
    Container,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewportArgs {
    pub width: u32,
    pub height: u32,
    /// Device pixel ratio × 1000 (default 1000).
    pub device_scale_milli: Option<u32>,
}

/// Overrides of the determinism profile; anything left out keeps the default.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileArgs {
    /// Exact storage bucket policies restored before cache and IndexedDB data.
    #[serde(default)]
    pub initial_buckets: Vec<frontcap_core::storage::InitialBucket>,
    /// IndexedDB schema and JSON records installed and verified before scripts.
    #[serde(default)]
    pub initial_indexed_db: Vec<frontcap_core::indexed_db::InitialDatabase>,
    /// Cache contents installed before application scripts.
    #[serde(default)]
    pub initial_caches: Vec<frontcap_core::cache::InitialCache>,
    /// Cookies installed before the first navigation request; optional controlled expiry.
    #[serde(default)]
    pub initial_cookies: Vec<frontcap_core::intervention::InitialCookie>,
    /// Seed local/session storage in the first top document before application scripts run.
    pub initial_storage: Option<frontcap_core::intervention::InitialStorage>,
    /// Lossless native storage before scripts; cannot be combined with initial_storage.
    pub initial_dom_storage: Option<frontcap_core::dom_storage::DomStorageState>,
    /// Windows created before scripts, named popup-1, popup-2, ... in this order.
    #[serde(default)]
    pub initial_contexts: Vec<frontcap_core::profile::InitialContext>,
    /// Name of the launch window before its first application script.
    pub initial_window_name: Option<String>,
    /// Virtual date at time 0, in milliseconds since the Unix epoch (default 2000-01-01T00:00:00Z).
    pub initial_date_ms: Option<u64>,
    pub time_zone: Option<String>,
    pub locale: Option<String>,
    pub random_seed: Option<u64>,
    pub viewport: Option<ViewportArgs>,
    pub touch: Option<bool>,
    pub popups: Option<frontcap_core::profile::PopupPolicy>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchArgs {
    /// Retrying a launch with the same request id returns the same generation.
    pub request_id: Option<RequestId>,
    pub url: String,
    #[serde(default)]
    pub determinism: Determinism,
    #[serde(default)]
    pub host: Host,
    /// Viewport, locale, touch, popups and state before scripts (initial_*, whose schemas are in state
    /// describe of recipe); unset fields keep their defaults.
    #[serde(default)]
    pub profile: ProfileArgs,
    /// Origins whose requests pass through to the network; default: the URL's origin.
    pub passthrough_origins: Option<Vec<String>>,
    /// Network rules, first match wins; requests no rule matches are blocked.
    #[serde(default)]
    pub rules: RuleSet,
    /// Settle (default: idle 250 ms, at most 10 s; held loads keep it busy) or none (before scripts run).
    #[serde(default)]
    pub then: LaunchThen,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum LaunchThen {
    None,
    Settle { idle_window_ms: u64, max_ms: u64 },
}

impl Default for LaunchThen {
    fn default() -> Self {
        LaunchThen::Settle { idle_window_ms: 250, max_ms: 10_000 }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GenerationArg {
    pub launch_id: LaunchId,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StopArgs {
    pub launch_id: LaunchId,
    /// Needed only while another process holds a live lease.
    pub lease: Option<String>,
    pub request_id: Option<RequestId>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitOp {
    /// Stop at the next requested event boundary (`response`, `stream`: after the instant's deliveries and
    /// the page's handlers, before its next render; `stream` is a stream's opening, message or close).
    Next { event: frontcap_core::event::ProgressEvent, max_ms: u64 },
    /// Advance virtual time by exactly `by_ms`.
    Advance { by_ms: u64 },
    /// Progress until idle_window_ms of quiet, bounded by max_ms.
    Settle { idle_window_ms: u64, max_ms: u64 },
    /// Progress until predicate or watch is true, bounded by max_ms. Specify exactly one; `context` names
    /// the popup a predicate is judged in (a watch keeps its own).
    Until {
        #[serde(default)]
        predicate: Option<Predicate>,
        #[serde(default)]
        watch: Option<WatchName>,
        max_ms: u64,
        #[serde(default)]
        context: Option<String>,
    },
    /// Read the clock.
    Now {},
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitArgs {
    pub launch_id: LaunchId,
    pub lease: Option<String>,
    pub request_id: Option<RequestId>,
    /// E.g. {"type": "settle", "idle_window_ms": 250, "max_ms": 2000}, {"type": "advance", "by_ms": 1000} or
    /// {"type": "until", "predicate": {"is": "visible", "target": {"by": "text", "text": "Saved"}}, "max_ms": 5000}.
    pub op: WaitOp,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ActAction {
    Click { target: Locator },
    /// Click a located CSS point; checks boundary/target, never scrolls.
    ClickAt { target: Locator, point: [i32; 2], boundary: frontcap_core::observation::ObservationBoundary },
    Dblclick { target: Locator },
    Hover { target: Locator },
    /// Unpressed move; checks as click_at.
    HoverAt { target: Locator, point: [i32; 2], boundary: frontcap_core::observation::ObservationBoundary },
    /// Key events per character; text insertion if no key produces it.
    Type { target: Locator, text: String },
    /// Key/chord (Control+Shift+K); target defaults to focused element.
    Press {
        #[serde(default)]
        target: Option<Locator>,
        key: String,
    },
    /// Replace editable content.
    Fill { target: Locator, value: String },
    /// Select options by value/label.
    Select { target: Locator, values: Vec<String> },
    /// A wheel over the target's nearest scroller (the document without one); the page's handlers decide, and it scrolls at the next frame.
    Scroll {
        #[serde(default)]
        target: Option<Locator>,
        #[serde(default)]
        delta_x: i32,
        #[serde(default)]
        delta_y: i32,
    },
    /// Press, move to `to` (the drop target) or by `offset` {dx, dy}, release; pointer and HTML drags.
    /// Frozen unless `interval_ms` grants time before each move (`steps`: 5).
    Drag {
        target: Locator,
        #[serde(default)]
        to: Option<Locator>,
        #[serde(default)]
        offset: Option<frontcap_core::action::Offset>,
        #[serde(default)]
        steps: Option<u32>,
        #[serde(default)]
        interval_ms: u64,
    },
    /// Set files on the control opening the file chooser or its `<input type=file>`; paths are relative to
    /// the server's directory.
    Upload { target: Locator, files: Vec<String> },
    Navigate { url: String },
    Back {},
    Reload {},
    Close {},
    /// Advance until the target could be clicked, checked at every frame tick.
    WaitActionable { target: Locator, max_ms: u64 },
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Then {
    None,
    Settle {
        idle_window_ms: u64,
        max_ms: u64,
    },
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActArgs {
    pub launch_id: LaunchId,
    pub lease: Option<String>,
    pub request_id: Option<RequestId>,
    /// E.g. {"type": "click", "target": {"by": "css", "selector": "#buy"}} or {"type": "fill", "target":
    /// {"by": "role", "role": "textbox", "name": "Email"}, "value": "dana@example.com"}. A target is any
    /// locator: css, text, role, test_id, ref (from observe), frame or document.
    pub action: ActAction,
    /// Default: settle {idle_window_ms: 250, max_ms: 2000} (none for wait_actionable); a held request keeps
    /// it busy until max_ms.
    #[serde(default)]
    pub then: Option<Then>,
    /// A popup's id (session contexts); default: page.
    pub context: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaptureOp {
    /// Offline visual-change sheet; labels link sources. max_frames: 1–24, default 12. Tiles are at most
    /// 320×200: give `rect` [x, y, width, height] of the frame to compare and show only that region, or read
    /// one frame whole with recorded_frame.
    Keyframes {
        since_us: Option<u64>, until_us: Option<u64>, max_frames: Option<u32>,
        rect: Option<[u32; 4]>,
        /// 1–6 seqs from debug events; excludes range/max_frames. Same-time order is unknown.
        event_seqs: Option<Vec<u64>>,
    },
    /// Offline frame index; inclusive time range.
    RecordedFrames { since_us: Option<u64>, until_us: Option<u64> },
    /// Offline PNG by file from recorded_frames or record_stop.
    RecordedFrame { file: String },
    /// Last presented frame; inline includes its PNG.
    Screenshot {
        /// Element ref to crop; omitted means viewport. Never scrolls.
        target: Option<String>,
        #[serde(default)]
        inline: bool,
    },
    /// Keep every rendered frame from now on.
    RecordStart {},
    /// Stop keeping frames; returns them.
    RecordStop {},
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptureArgs {
    pub launch_id: LaunchId,
    pub lease: Option<String>,
    pub request_id: Option<RequestId>,
    /// E.g. {"type": "screenshot", "inline": true}; record_start and record_stop keep every rendered frame.
    pub op: CaptureOp,
    pub context: Option<String>,
}

/// The `network` drawer: the effective rules (read, and edit for future requests) and requests held for
/// the agent.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum NetworkOp {
    /// Insert a rule before `before` (one of yours), or after your rules; the launch's defaults stay
    /// last. Others keep their positions.
    AddRule { expected_revision: u64, boundary: frontcap_core::observation::ObservationBoundary, rule: Box<frontcap_core::rules::Rule>, before: Option<frontcap_core::ids::RuleId> },
    /// Remove a caller rule (not a launch default). Others keep their positions.
    RemoveRule { expected_revision: u64, boundary: frontcap_core::observation::ObservationBoundary, id: frontcap_core::ids::RuleId },
    /// Replace the caller's rules for future requests; an unchanged rule keeps its position, held requests
    /// their decisions.
    SetRules { expected_revision: u64, boundary: frontcap_core::observation::ObservationBoundary, rules: frontcap_core::rules::RuleSet },
    /// The effective rules in order (yours, then the launch's defaults, marked `default`), their
    /// revision and the boundary; no lease. Bodies read as `<field>_bytes` and `<field>_sha256` unless
    /// `bodies`.
    ListRules {
        #[serde(default)]
        bodies: bool,
    },
    /// Requests held for you: `{ordinal, method, url, rule, rules_revision}` and the first 4 KiB of a text
    /// body; a held stream also has `stream`: `connecting` or `open`.
    ListHeld {},
    /// Answer a held request now, e.g. {\"ordinal\": \"req-2\", \"response\": {\"status\": 200, \"body\": \"[]\"}}
    /// (`latency_ms` 0: advance first for a later answer). The page receives it at the next progress. A
    /// held stream takes a status only: 200 opens it.
    Respond { ordinal: String, response: MockSpec },
    /// Fail a held request.
    Fail { ordinal: String, reason: FailReason },
    /// Deliver a message on a held, open stream now (an EventSource's `data`, `event`, `id`; a
    /// WebSocket's text); the page's handlers run at once.
    Send {
        ordinal: String,
        data: String,
        #[serde(default)]
        event: Option<String>,
        #[serde(default)]
        id: Option<String>,
    },
    /// Close a held, open stream as its server would: an EventSource reconnects after 3 s (a new
    /// request); a WebSocket closes with `code` (default 1000).
    Close {
        ordinal: String,
        #[serde(default)]
        code: Option<u16>,
    },
}

fn changes_by_default() -> bool {
    true
}

/// The `watch` drawer: named predicates sampled at every mutation, frame tick and pause for the
/// generation's life. A target is any locator; a role one is resolved again at each sample.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum WatchOp {
    /// `context`: the popup (`popup-1`) whose page the predicate is judged in; its references name it too.
    Add {
        name: WatchName,
        predicate: Predicate,
        #[serde(default)]
        context: Option<String>,
    },
    Remove { name: WatchName },
    List {},
    /// The first sample and each change of value, with their virtual times (`omitted` counts the rest);
    /// `changes_only: false` returns every sample.
    History {
        name: WatchName,
        #[serde(default = "changes_by_default")]
        changes_only: bool,
    },
}

/// The `session` drawer: generations, leases, saved results, dialogs and contexts.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionOp {
    /// The workspace's generations, 20 per page by launch_id (pass next_after as after). status tells
    /// live ownership.
    List { after: Option<LaunchId> },
    /// A mutation's saved result or pending state; no lease, no re-execution.
    Result { operation_id: RequestId },
    /// Take the lease of a generation whose controller went away; returns what completed meanwhile.
    Reattach {},
    /// Release the lease; the generation keeps running.
    Release {},
    /// How the page's dialogs are answered from now on, first match wins, e.g. [{\"kind\": \"confirm\",
    /// \"message\": \"Delete*\", \"answer\": \"accept\"}]. Unmatched: confirm false, prompt null, beforeunload
    /// accepted. `hold` suspends the operation for answer_dialog.
    DialogRules { rules: frontcap_core::dialog::DialogRules },
    /// Answer a suspended operation's dialog (prompt_text for an accepted prompt). The parent may not be
    /// done: read session result for it.
    AnswerDialog { operation_id: RequestId, dialog_id: String, accept: bool, prompt_text: Option<String> },
    /// The launch page (`page`) and windows it opened (`popup-1`…), with opener, URL and whether open.
    Contexts {},
    /// Ask a running request (status names it) to stop: a wait at its next frame tick, a probe also
    /// between steps. The cancelled call answers where it stopped. A probe's child needs no launch_id.
    Cancel { operation_id: RequestId },
}

/// The `state` drawer: direct state reads and changes (not user input); saved starting points and branches.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateOp {
    /// The target document's origin and partition key, for storage reads and writes.
    StorageScope { target: Locator, #[serde(default)] context: Option<String> },
    /// Document state, storage, cookies, IndexedDB or app state providers; storage fields take their
    /// `scope` from `storage_scope`.
    Read { target: Locator, field: frontcap_core::intervention::StateField, #[serde(default)] context: Option<String> },
    /// Ordered document, storage or app state changes at a checked boundary; no input events, no rollback.
    /// `progress_ms` (at most 10000) lets an app provider's write that needs later instants have up to
    /// that much virtual time, within the provider's own `max_ms`.
    Apply {
        boundary: frontcap_core::observation::ObservationBoundary,
        changes: Vec<frontcap_core::intervention::StateChange>,
        #[serde(default)]
        context: Option<String>,
        #[serde(default)]
        progress_ms: Option<u64>,
        /// Requires a transactional provider; currently refused before any mutation.
        #[serde(default)]
        atomic: bool,
    },
    /// Save a starting point (no launch_id or request_id). Kind recipe takes a `recipe`, or `from` a live
    /// generation: its children get the storage, cookies, windows and app providers, then load the page
    /// fresh, so what the page derives on load (read marks, restored notices) is computed again. Kind
    /// replay takes `from` a live or stopped generation: its children repeat the recorded commands to the
    /// same moment. Kind checkpoint takes `from` a live generation: its children continue that exact
    /// execution, where bootstrap's basis_kinds lists it. With `from`, `verify` names predicates each
    /// child must meet once ready, read in a live source too (`verify_at_source`). The source goes on
    /// unchanged. Same content, same saved_id.
    Save { kind: frontcap_core::basis::BasisKind, recipe: Option<Box<RecipeArgs>>, from: Option<LaunchId>, #[serde(default)] verify: Vec<frontcap_core::basis::Verification> },
    /// A saved starting point's verified structure, without private values (no launch_id).
    DescribeSaved { saved_id: frontcap_core::ids::SavedId },
    /// Delete a saved starting point with its kept files or image (no launch_id); its children keep
    /// theirs. Refused while one of its branches is constructed.
    DeleteSaved { saved_id: frontcap_core::ids::SavedId },
    /// A new child generation from a saved starting point by its own method (no launch_id); `changes`
    /// follow a recipe's (no references). Returns the ready child or construction_failed; a checkpoint
    /// child holds its source's held input (`held`). Retry with the same request_id for the same child.
    Branch {
        saved_id: frontcap_core::ids::SavedId,
        /// The basis's own kind, as describe_saved names it: you choose the method, it is never substituted.
        kind: frontcap_core::basis::BasisKind,
        #[serde(default)]
        changes: Vec<frontcap_core::intervention::StateChange>,
        /// At most 10000 ms per app-provider write in this branch's changes (recipe branches only).
        #[serde(default)]
        progress_ms: Option<u64>,
    },
    /// Branch as above, then run `steps` in the child as one request (1 to 32, each within its own
    /// bound): each step's result as its own call returns it, up to the first that does not complete. The
    /// child stays live at `end` for inspection: stop it when done. Retry with the same request_id for the same answer.
    Probe {
        saved_id: frontcap_core::ids::SavedId,
        /// The basis's own kind, as for branch.
        kind: frontcap_core::basis::BasisKind,
        #[serde(default)]
        changes: Vec<frontcap_core::intervention::StateChange>,
        /// Progress for the branch changes before steps, as in branch; each apply step has its own grant.
        #[serde(default)]
        progress_ms: Option<u64>,
        steps: Vec<ProbeStepArgs>,
    },
}

/// One probe step, as its own call takes it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProbeStepArgs {
    /// state apply at the boundary the probe has reached.
    Apply {
        changes: Vec<frontcap_core::intervention::StateChange>,
        #[serde(default)] context: Option<String>,
        /// At most 10000 ms per app-provider write, within the provider's own bound.
        #[serde(default)] progress_ms: Option<u64>,
        /// Requires a transactional provider; currently refused before the child is constructed.
        #[serde(default)] atomic: bool,
    },
    Act {
        action: ActAction,
        #[serde(default)]
        then: Option<Then>,
        #[serde(default)]
        context: Option<String>,
    },
    /// Any wait op but now.
    Wait { op: WaitOp },
    /// Wait until after_ms after an earlier step completed (zero-based index). Use then:none on an
    /// action to anchor before its settling. A target already reached advances zero time.
    WaitFrom { step_index: usize, after_ms: u64 },
    /// An observe op, without scene images.
    Read { op: ObserveOp, #[serde(default)] context: Option<String> },
    /// A state field, as state read reads it (an app provider's value, storage, cookies).
    State { target: Locator, field: frontcap_core::intervention::StateField, #[serde(default)] context: Option<String> },
    /// `{operation: add, name, predicate}` or `{operation: remove, name}`; read its history after the probe.
    Watch { op: WatchOp },
    Input {
        #[schemars(with = "serde_json::Value")]
        op: frontcap_recorder_protocol::ProbeInput,
    },
}

/// A reconstruction recipe: launch inputs, then a barrier, typed changes and named startup predicates.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecipeArgs {
    pub url: String,
    #[serde(default)]
    pub profile: ProfileArgs,
    #[serde(default)]
    pub rules: frontcap_core::rules::RuleSet,
    pub passthrough_origins: Option<Vec<String>>,
    /// none, settle (default: idle 250 ms, at most 10 s) or until a predicate holds (no references).
    #[serde(default)]
    pub barrier: frontcap_core::basis::Barrier,
    /// Changes after the barrier, as for state apply (no references).
    #[serde(default)]
    pub changes: Vec<frontcap_core::intervention::StateChange>,
    /// At most 10000 ms per app-provider write in changes, within the provider's own bound.
    #[serde(default)]
    pub progress_ms: Option<u64>,
    /// Named predicates each child must meet, kept as its watches (no references).
    #[serde(default)]
    pub verify: Vec<frontcap_core::basis::Verification>,
    /// Each child's dialog rules from its start.
    #[serde(default)]
    pub dialog_rules: frontcap_core::dialog::DialogRules,
}

/// The `input` drawer: held pointer, keys and composition. Nothing settles or grants time.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputOp {
    Status {},
    /// A wheel delta in CSS pixels at a checked point; scrolling may await a frame. `context`, here and on
    /// begin, key_down and compose, names the popup input starts in; later input follows what it holds.
    Wheel { target: Locator, point: [i32; 2], delta: [i32; 2], boundary: frontcap_core::observation::ObservationBoundary, #[serde(default)] context: Option<String> },
    /// Press at a checked point (`pointer: touch` touches); the call's request_id names the handle. The
    /// mouse does not move there first: act hover_at moves it.
    Begin {
        target: Locator,
        point: [i32; 2],
        #[serde(default)]
        button: frontcap_core::pointer::PointerButton,
        #[serde(default)]
        pointer: frontcap_core::pointer::PointerKind,
        boundary: frontcap_core::observation::ObservationBoundary,
        #[serde(default)]
        context: Option<String>,
    },
    Move { handle: frontcap_core::pointer::PointerHandle, point: [i32; 2], boundary: frontcap_core::observation::ObservationBoundary },
    /// Press another mouse button where the pointer is.
    Press { handle: frontcap_core::pointer::PointerHandle, button: frontcap_core::pointer::PointerButton, boundary: frontcap_core::observation::ObservationBoundary },
    /// Release one mouse button; the handle stays while others are held.
    Release { handle: frontcap_core::pointer::PointerHandle, button: frontcap_core::pointer::PointerButton },
    /// Add a touch contact over any document; touch events stay in the first contact's.
    TouchAdd { handle: frontcap_core::pointer::PointerHandle, target: Locator, point: [i32; 2], boundary: frontcap_core::observation::ObservationBoundary },
    TouchMove { handle: frontcap_core::pointer::PointerHandle, contact: u32, point: [i32; 2], boundary: frontcap_core::observation::ObservationBoundary },
    TouchEnd { handle: frontcap_core::pointer::PointerHandle, contact: u32 },
    /// Release every button or lift every contact; may click or drop.
    End { handle: frontcap_core::pointer::PointerHandle },
    /// Cancel the pointer (`pointercancel`, `touchcancel`) without a release or click.
    Cancel { handle: frontcap_core::pointer::PointerHandle },
    /// Hold a key (`Shift`, `a`) or US-layout code (`KeyA`); modifiers apply to later input.
    KeyDown { key: String, #[serde(default)] context: Option<String> },
    KeyUp { key: String },
    /// One repeated keydown of a held key.
    KeyRepeat { key: String },
    /// Set the focused element's uncommitted composition (`서`, then `서울`); empty text cancels it.
    Compose { text: String, #[serde(default)] context: Option<String> },
    /// Commit the composition, optionally with the `key` the input method consumes (`Enter`).
    Commit {
        #[serde(default)]
        key: Option<String>,
    },
}

/// The `debug` drawer: script evaluation, histories, result pages, bundle and replay.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum DebugOp {
    /// Offline behavior events and source-prefix hash; pages never repair partial records.
    Events { after_seq: Option<u64>, limit: Option<usize> },
    /// Offline MP4 from recorded frames at their virtual intervals. Requires ffmpeg. The final
    /// image holds for tail_ms (1–10000, default 100); this is video padding, not browser progress.
    Video { since_us: Option<u64>, until_us: Option<u64>, tail_ms: Option<u64> },
    /// Published derived artifacts, verifying their image/video and provenance hashes without browser contact.
    Artifacts {},
    /// A page of a retained oversized result: `pointer` selects its JSON, `offset` a byte position.
    /// Concatenate pages before parsing.
    Result {
        result_id: String,
        #[serde(default)]
        pointer: String,
        #[serde(default)]
        offset: usize,
    },
    /// Evaluate an expression in the page (promises are awaited); taints the run.
    Evaluate {
        expression: String,
        /// Frame element whose document's main world runs the expression; omitted for the page.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        frame: Option<Locator>,
    },
    /// Console messages and uncaught exceptions.
    Console {
        #[serde(default)]
        filter: ConsoleFilterArgs,
    },
    /// Requests with their issue and answer times and the first 4 KiB of the body the page sent.
    Requests {
        #[serde(default)]
        filter: NetworkFilterArgs,
    },
    /// The evidence bundle: members, integrity, finalization.
    Bundle {},
    /// Seal a failed or stopped generation's evidence.
    Finalize {},
    /// Run a stopped generation's commands again in a new one (`url` elsewhere) and compare: `equal`, the
    /// first differing answer and trace line.
    Replay {
        #[serde(default)]
        url: Option<String>,
    },
}

/// Arguments every drawer operation shares; the rest belongs to the operation.
#[derive(Debug, Default)]
pub struct Common {
    pub launch_id: Option<LaunchId>,
    pub lease: Option<String>,
    pub request_id: Option<RequestId>,
}

pub enum Drawer<Op> {
    /// `describe`, with `of` naming one operation.
    Describe(Option<String>),
    Run(Common, Op),
}

/// Decodes a drawer call: `operation: describe`, or the shared arguments and one operation.
fn parse<T: serde::de::DeserializeOwned>(v: Option<serde_json::Value>) -> Result<Option<T>, serde_json::Error> {
    v.map(serde_json::from_value).transpose()
}

pub fn drawer<Op: serde::de::DeserializeOwned>(tool: &str, text: &str) -> Result<Drawer<Op>, CallToolResult> {
    let bad = |message: String| {
        ToolError::new("invalid_arguments", format!("{tool}: {message}"), format!("call {tool} with operation describe for its operations")).result()
    };
    let mut v: serde_json::Value = serde_json::from_str(text).map_err(|e| bad(e.to_string()))?;
    let obj = v.as_object_mut().ok_or_else(|| bad("arguments must be an object".into()))?;
    // `describe` shows each operation as an object with an `operation` constant; it is sent flat.
    if obj.get("operation").is_some_and(|o| o.is_object()) {
        return Err(ToolError::new("invalid_arguments", format!("{tool}: `operation` is a string naming the operation, with its fields beside it"),
            format!("send {{\"operation\": \"<name>\", ...its fields}} to {tool}, not {{\"operation\": {{...}}}}")).result());
    }
    if obj.get("operation").and_then(|o| o.as_str()) == Some("describe") {
        return Ok(Drawer::Describe(obj.get("of").and_then(|o| o.as_str()).map(str::to_owned)));
    }
    let take = |obj: &mut serde_json::Map<String, serde_json::Value>, k: &str| obj.remove(k).filter(|v| !v.is_null());
    let common = Common {
        launch_id: parse(take(obj, "launch_id")).map_err(|e| bad(format!("launch_id: {e}")))?,
        lease: parse(take(obj, "lease")).map_err(|e| bad(format!("lease: {e}")))?,
        request_id: parse(take(obj, "request_id")).map_err(|e| bad(format!("request_id: {e}")))?,
    };
    let operation = obj.get("operation").and_then(|o| o.as_str()).map(str::to_owned);
    let fields = obj.clone();
    let op = serde_path_to_error::deserialize(v).map_err(|e| {
        let recovery = match &operation {
            Some(op) => format!("call {tool} with operation describe and of \"{op}\" for its fields"),
            None => format!("call {tool} with operation describe for its operations"),
        };
        // An operation is read as a whole, which loses the path: the field holding the value the error
        // names is that field.
        let message = argument_message(&e);
        let message = match (e.path().to_string() == ".", field_showing(&fields, &e.inner().to_string())) {
            (true, Some(field)) => format!("{field}: {message}"),
            _ => message,
        };
        ToolError::new("invalid_arguments", format!("{tool}: {message}"), recovery).result()
    })?;
    Ok(Drawer::Run(common, op))
}

/// A drawer's `describe`. Without `of`, what each operation does and the fields it takes (`?` marks an
/// optional one); with `of`, that operation's full schema and the notes that belong to it.
pub struct Describe {
    operations: serde_json::Value,
    common: serde_json::Value,
    index_notes: serde_json::Value,
    notes: fn(&str) -> serde_json::Value,
    /// Folds part of an operation's schema to a pointer at a section.
    fold: fn(&str, &mut serde_json::Value),
    /// A schema `of` names besides the operations (`recipe`).
    section: fn(&str) -> Option<serde_json::Value>,
}

impl Describe {
    pub fn of<Op: JsonSchema>() -> Describe {
        Describe {
            operations: serde_json::to_value(schemars::schema_for!(Op)).expect("a schema"),
            common: serde_json::json!({"launch_id": "the generation (every operation)", "lease": "optional, as for other tools", "request_id": "optional, at most 100 characters, not starting with mcp-"}),
            index_notes: serde_json::json!({}),
            notes: |_| serde_json::Value::Null,
            fold: |_, _| {},
            section: |_| None,
        }
    }

    fn name(variant: &serde_json::Value) -> Option<&str> {
        let operation = &variant["properties"]["operation"];
        operation["const"].as_str().or_else(|| operation["enum"][0].as_str())
    }

    pub fn answer(&self, of: Option<&str>) -> Result<serde_json::Value, ToolError> {
        let variants = self.operations["oneOf"].as_array().expect("drawer operations are a tagged enum");
        let Some(of) = of else {
            let mut index = serde_json::Map::new();
            for variant in variants {
                let required: Vec<&str> = variant["required"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).collect();
                let fields: Vec<String> = variant["properties"].as_object().into_iter().flatten()
                    .filter(|(name, _)| *name != "operation")
                    .map(|(name, _)| if required.contains(&name.as_str()) { name.clone() } else { format!("{name}?") })
                    .collect();
                let mut entry = serde_json::json!({"fields": fields});
                if let Some(does) = variant.get("description") {
                    entry["does"] = does.clone();
                }
                index.insert(Self::name(variant).expect("each variant names its operation").into(), entry);
            }
            let mut described = serde_json::json!({"common": self.common, "operations": index,
                "next": "describe with of: <operation> for that operation's full schema"});
            for (key, value) in self.index_notes.as_object().into_iter().flatten() {
                described[key] = value.clone();
            }
            return Ok(described);
        };
        let (of, definition) = match of.split_once('#') {
            Some((of, definition)) => (of, Some(definition)),
            None => (of, None),
        };
        let described = self.schema_of(of)?;
        Ok(match definition {
            Some(name) => within_budget(definition_of(described, of, name)?, of),
            None => within_budget(described, of),
        })
    }

    /// An operation's full schema and its notes, or a section's.
    fn schema_of(&self, of: &str) -> Result<serde_json::Value, ToolError> {
        let variants = self.operations["oneOf"].as_array().expect("drawer operations are a tagged enum");
        let Some(variant) = variants.iter().find(|v| Self::name(v) == Some(of)) else {
            return (self.section)(of).map(|schema| serde_json::json!({"section": of, "schema": schema})).ok_or_else(|| ToolError::new(
                "invalid_arguments", format!("no operation {of}"), "call describe without of to list the operations"));
        };
        let mut schema = variant.clone();
        (self.fold)(of, &mut schema);
        schema["$defs"] = self.operations["$defs"].clone();
        if let Some(root) = schema.as_object_mut() {
            prune_definitions(root);
            root.values_mut().for_each(compact);
        }
        let mut described = serde_json::json!({"operation": of, "schema": schema});
        for (key, value) in (self.notes)(of).as_object().into_iter().flatten() {
            described[key] = value.clone();
        }
        Ok(described)
    }
}

/// What one `describe` response carries at most. A larger schema names its definitions, and each is
/// described alone (`of: <operation>#<definition>`).
pub const DESCRIBE_BYTES: usize = 8 * 1024;

/// `name` of `described`'s definitions, with the definitions it uses.
fn definition_of(described: serde_json::Value, of: &str, name: &str) -> Result<serde_json::Value, ToolError> {
    let defs = &described["schema"]["$defs"];
    let Some(definition) = defs.get(name) else {
        return Err(ToolError::new("invalid_arguments", format!("{of} names no definition {name}"),
            "describe with of: <operation> to see the definitions it names"));
    };
    let mut schema = definition.clone();
    schema["$defs"] = defs.clone();
    if let Some(root) = schema.as_object_mut() {
        prune_definitions(root);
        if root.get("$defs").and_then(|d| d.as_object()).is_some_and(|d| d.is_empty()) {
            root.remove("$defs");
        }
    }
    Ok(serde_json::json!({"definition": format!("{of}#{name}"), "schema": schema}))
}

/// `described` as it is when it fits [`DESCRIBE_BYTES`]; otherwise with each definition named, its
/// description kept and the `of` that describes it.
fn within_budget(mut described: serde_json::Value, of: &str) -> serde_json::Value {
    if serde_json::to_string(&described).expect("serializes").len() <= DESCRIBE_BYTES {
        return described;
    }
    if let Some(defs) = described["schema"]["$defs"].as_object_mut() {
        for (name, definition) in defs.iter_mut() {
            let mut named = serde_json::json!({"describe": {"of": format!("{of}#{name}")}});
            if let Some(text) = definition.get("description").and_then(|d| d.as_str()) {
                named["description"] = text.into();
            }
            *definition = named;
        }
    }
    described
}

pub fn describe_debug() -> Describe {
    Describe::of::<DebugOp>()
}

pub fn describe_input() -> Describe {
    let mut d = Describe::of::<InputOp>();
    d.common["handle"] = serde_json::json!("verbatim as begin returned it, or as a checkpoint branch's held names it in the child");
    d
}

pub fn describe_session() -> Describe {
    let mut d = Describe::of::<SessionOp>();
    d.common["launch_id"] = serde_json::json!("required except list; list rejects launch_id, lease and request_id");
    d
}

pub fn describe_network() -> Describe {
    let mut d = Describe::of::<NetworkOp>();
    d.index_notes = serde_json::json!({"rules": "launch takes the rules set_rules does"});
    d
}

pub fn describe_watch() -> Describe {
    let mut d = Describe::of::<WatchOp>();
    d.index_notes = serde_json::json!({"targets": "any locator; a role one is matched again at every sample"});
    d
}

pub fn describe_state() -> Describe {
    let mut d = Describe::of::<StateOp>();
    d.common["launch_id"] = serde_json::json!("required except save, describe_saved, delete_saved, branch and probe, which reject it");
    d.common["request_id"] = serde_json::json!("optional, at most 100 characters, not starting with mcp-; save rejects it (the same content returns the same saved_id)");
    d.index_notes = serde_json::json!({
        "recipe": "describe with of: recipe for an authored recipe, every initial_* schema included",
        "signed_in": "stand in for signing in with what the app checks: apply its session cookie (field cookie, http_only allowed) or stored token (field storage), or seed them as a recipe's initial_cookies or initial_storage; network rules answer its auth requests. Report it as direct intervention, not user input"
    });
    d.fold = |of, schema| if of == "save" {
        schema["properties"]["recipe"] = serde_json::json!({"type": ["object", "null"],
            "description": "An authored recipe {url, profile, rules, barrier, changes, verify, ...}; describe with of: recipe."});
    };
    d.section = |name| (name == "recipe").then(|| {
        let mut schema = serde_json::to_value(schemars::schema_for!(RecipeArgs)).expect("a schema");
        if let Some(root) = schema.as_object_mut() {
            root.remove("$schema");
            root.remove("title");
            root.values_mut().for_each(compact);
        }
        schema
    });
    d.notes = |of| match of {
        "apply" | "read" => serde_json::json!({
            "atomic_changes": {
                "supported": false, "default": "ordered",
                "on_failure": "ordered changes keep their applied prefix; atomic: true is refused before any effect"
            },
            "indexed_db_record": {
                "value": schemars::schema_for!(frontcap_core::intervention::IndexedDbRecord),
                "scope": "an existing database and store, with an explicit primary key; an inline key must match; JSON up to 64 KiB",
                "completion": "the native transaction commits, then reads back, without page work; an unfinished conflicting transaction refuses",
                "recovery": "repeat an explicit-key write whose dispatch is uncertain; the app does not reread"
            },
            "cache_entry": {
                "value": schemars::schema_for!(frontcap_core::cache::CacheRecord),
                "matching": "GET with native request-header and Vary matching, in the target's storage scope and bucket",
                "writes": "present replaces the URL regardless of Vary; omit metadata for a synthetic response, or keep a fetched one's full native metadata (up to 4 MiB); absent uses Vary matching and creates no cache; named buckets must exist",
                "recovery": "repeat a write whose completion was lost; entries are written one by one; reads include native metadata"
            }
        }),
        _ => serde_json::Value::Null,
    };
    // A probe step takes what its own tool takes: name that tool's schema instead of repeating it.
    let schema = &mut d.operations;
    for variant in schema["$defs"]["ProbeStepArgs"]["oneOf"].as_array_mut().expect("probe step variants") {
        let fields: &[(&str, &str)] = match variant["properties"]["step"]["const"].as_str() {
            Some("act") => &[("action", "the act tool's action"), ("then", "the act tool's then")],
            Some("wait") => &[("op", "the wait tool's op, but now")],
            Some("read") => &[("op", "the observe tool's op, scene without image")],
            Some("watch") => &[("op", "the watch drawer's {operation: add, name, predicate} or {operation: remove, name}")],
            Some("input") => &[("op", "the input drawer's {operation, ...} but status, without handle or boundary: the child's held pointer at the boundary reached")],
            Some("state") => &[("target", "state read's target"), ("field", "state read's field")],
            _ => &[],
        };
        for (field, description) in fields {
            variant["properties"][*field] = serde_json::json!({ "description": description });
        }
    }
    d
}

/// Launch advertises rule and initialization inputs; the owning drawers provide their full schemas.
fn collapse_launch_inputs(tool: &mut ToolDescription) {
    let Some(root) = tool.input_schema.as_object_mut() else { return };
    if let Some(p) = root.get_mut("properties").and_then(|p| p.as_object_mut()) {
        p.insert(
            "rules".into(),
            serde_json::json!({"type": "array", "items": {"type": "object"}, "default": [],
                "description": "Rules {id, match: {url, method?, headers?, body?}, action: {type, ...}}; network describe of set_rules has their schema."}),
        );
    }
    if let Some(profile) = root.get_mut("$defs").and_then(|defs| defs.get_mut("ProfileArgs"))
        .and_then(|p| p.get_mut("properties")).and_then(|p| p.as_object_mut()) {
        profile.insert("initial_indexed_db".into(), serde_json::json!({"type":"array","items":{"type":"object"},"default":[],"description":"IndexedDB schema and JSON records before scripts, read back natively."}));
        profile.insert("initial_buckets".into(), serde_json::json!({"type":"array","items":{"type":"object"},"default":[],"description":"Exact storage bucket policy before cache and IndexedDB content."}));
        profile.insert("initial_caches".into(), serde_json::json!({"type":"array","items":{"type":"object"},"default":[],"description":"CacheStorage entries before scripts."}));
        profile.insert("initial_cookies".into(), serde_json::json!({"type":"array","items":{"type":"object"},"default":[],
            "description":"Cookies before the first request, optional expires_at_ms."}));
        profile.insert("initial_storage".into(), serde_json::json!({"type":["object","null"],
            "description":"Local/session storage before the first script."}));
        profile.insert("initial_dom_storage".into(), serde_json::json!({"type":["object","null"],
            "description":"Lossless native local/session storage before scripts; exclusive with initial_storage. Session owners must match page or initial_contexts."}));
        profile.insert("initial_contexts".into(), serde_json::json!({"type":"array","items":{"type":"object"},"default":[],
            "description":"Windows {opener,url,name,no_opener?,closed?}, created before scripts as popup-1, popup-2, ...; opener names page or an earlier window; a closed one stays at about:blank."}));
        profile.insert("popups".into(), serde_json::json!({"enum":["controlled","block",null],
            "description":"Windows the page opens: contexts (default; session contexts) or refused."}));
    }
    prune_definitions(root);
}

/// Keep only definitions reachable from the advertised arguments, including transitive references.
fn prune_definitions(root: &mut serde_json::Map<String, serde_json::Value>) {
    let mut used = std::collections::BTreeSet::new();
    let defs = root.get("$defs").cloned().unwrap_or_default();
    let mut todo = vec![serde_json::Value::Object(root.iter().filter(|(k, _)| *k != "$defs").map(|(k, v)| (k.clone(), v.clone())).collect())];
    while let Some(v) = todo.pop() {
        match v {
            serde_json::Value::Object(m) => {
                if let Some(r) = m.get("$ref").and_then(|r| r.as_str()).and_then(|r| r.strip_prefix("#/$defs/"))
                    && used.insert(r.to_string())
                    && let Some(d) = defs.get(r)
                {
                    todo.push(d.clone());
                }
                todo.extend(m.into_iter().map(|(_, v)| v));
            }
            serde_json::Value::Array(a) => todo.extend(a),
            _ => {}
        }
    }
    if let Some(d) = root.get_mut("$defs").and_then(|d| d.as_object_mut()) {
        d.retain(|k, _| used.contains(k));
    }
}

/// The operation names a drawer decodes, read from its schema so the listing cannot drift from it.
pub fn operations_of<Op: JsonSchema>() -> Vec<String> {
    let schema = serde_json::to_value(schemars::schema_for!(Op)).expect("a schema");
    schema["oneOf"].as_array().expect("drawer operations are a tagged enum").iter()
        .map(|variant| {
            let operation = &variant["properties"]["operation"];
            operation["const"].as_str().or_else(|| operation["enum"][0].as_str()).expect("each variant names its operation").to_string()
        })
        .collect()
}

fn drawer_tool<Op: JsonSchema>(name: &str, role: &str) -> ToolDescription {
    let mut ops = operations_of::<Op>();
    ops.push("describe".into());
    ToolDescription {
        name: name.into(),
        description: format!("Drawer: {role}."),
        input_schema: schemars::json_schema!({
            "type": "object",
            "properties": {
                "operation": {"type": "string", "enum": ops},
                "launch_id": {"type": "string"}
            },
            "required": ["operation"],
            "additionalProperties": true
        }),
    }
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConsoleFilterArgs {
    pub since_us: Option<u64>,
    pub until_us: Option<u64>,
    /// `log`, `info`, `warning`, `error`, `debug`, or `exception` for uncaught exceptions.
    pub level: Option<String>,
    pub text_contains: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NetworkFilterArgs {
    pub since_us: Option<u64>,
    pub until_us: Option<u64>,
    pub url_contains: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObserveOp {
    /// Map pixels of a scene image or crop to the page; pending or stale images are refused.
    LocateImage { image_id: String, point: [f64; 2] },
    /// Crop a retained image; rect is [x, y, width, height].
    Crop { image_id: String, rect: [u32; 4] },
    /// Hit-test viewport CSS pixels at a scene's boundary; a stale boundary is refused.
    Locate { boundary: frontcap_core::observation::ObservationBoundary, point: [f64; 2] },
    /// One node's requested properties, border geometry and click readiness (also: interaction, scroll,
    /// selection).
    Inspect { target: Locator, #[serde(default)] props: Vec<String> },
    /// Structure and optional last-presented image at one checked boundary, with coverage gaps.
    Scene { scope: Option<Locator>, depth: Option<u32>, max_nodes: Option<u32>, #[serde(default)] image: bool, #[serde(default)] unfold: bool },
    /// Interactive elements, frames and text, with references valid while the document is current.
    /// `scope`: a subtree; `max_nodes`: default 400; `geometry`: border quads. Six or more siblings of
    /// one shape show the first three and a node `folded: {count, scope}`; `unfold: true` lists them all.
    Snapshot { scope: Option<Locator>, depth: Option<u32>, max_nodes: Option<u32>, #[serde(default)] geometry: bool, #[serde(default)] unfold: bool },
    /// What changed since a snapshot of this document (one of the last 16), within that snapshot's scope
    /// and depth; runs fold as in snapshot.
    Diff { since: u64, max_nodes: Option<u32>, #[serde(default)] unfold: bool },
    /// One node's `text` (its textContent: hidden parts too), `value`, `visible`, `bbox`, `interaction`,
    /// `scroll`, `selection`, `attr:<name>` or `style:<property>`.
    Query { target: Locator, props: Vec<String> },
    /// How many nodes `target` matches (0 for none), as {by: text, text, contains: true} counts rows that say it.
    Count { target: Locator },
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ObserveArgs {
    pub launch_id: LaunchId,
    /// E.g. {"type": "snapshot"} for elements and their references, {"type": "scene", "image": true} to see
    /// the page, or {"type": "query", "target": {...}, "props": ["text"]}.
    pub op: ObserveOp,
    pub context: Option<String>,
}

/// `{code, message, recovery}`: every error names exactly one next action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub recovery: String,
}

impl ToolError {
    pub fn new(code: impl Into<String>, message: impl Into<String>, recovery: impl Into<String>) -> ToolError {
        ToolError { code: code.into(), message: message.into(), recovery: recovery.into() }
    }

    pub fn result(&self) -> CallToolResult {
        CallToolResult::error(self)
    }
}

/// Reads a tool's arguments; an error names the field it is in.
pub fn read_arguments<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, serde_path_to_error::Error<serde_json::Error>> {
    serde_path_to_error::deserialize(&mut serde_json::Deserializer::from_str(text))
}

/// The one top-level field whose value a deserializer error shows (`invalid type: string "storage"`).
fn field_showing(fields: &serde_json::Map<String, serde_json::Value>, message: &str) -> Option<String> {
    let shown = |v: &serde_json::Value| match v {
        serde_json::Value::String(s) => Some(format!("string {}", serde_json::to_string(s).ok()?)),
        serde_json::Value::Number(n) if n.is_f64() => Some(format!("floating point `{n}`")),
        serde_json::Value::Number(n) => Some(format!("integer `{n}`")),
        serde_json::Value::Bool(b) => Some(format!("boolean `{b}`")),
        _ => None,
    };
    let mut matching = fields.iter().filter(|(k, v)| *k != "operation" && shown(v).is_some_and(|s| message.contains(&s)));
    match (matching.next(), matching.next()) {
        (Some((k, _)), None) => Some(k.clone()),
        _ => None,
    }
}

/// An argument error in words: which field, and for a tagged object that it names its `type`.
fn argument_message(e: &serde_path_to_error::Error<serde_json::Error>) -> String {
    let path = e.path().to_string();
    let message = e.inner().to_string();
    let message = match message.split_once(", expected internally tagged enum ") {
        Some((found, _)) => format!("{found}, expected an object with its \"type\""),
        None => message,
    };
    if path == "." { message } else { format!("{path}: {message}") }
}

pub fn invalid_arguments(tool: &str, e: &serde_path_to_error::Error<serde_json::Error>) -> CallToolResult {
    ToolError::new("invalid_arguments", format!("{tool}: {}", argument_message(e)), format!("send {tool} with arguments that match its input schema")).result()
}

fn tool<T: JsonSchema>(name: &str, description: &str) -> ToolDescription {
    let mut input_schema = schemars::schema_for!(T);
    if let Some(map) = input_schema.as_object_mut() {
        // The MCP envelope identifies this as an input schema; generated Rust type titles add no arguments.
        map.remove("$schema");
        map.remove("title");
        if let Some(defs) = map.get_mut("$defs").and_then(|d| d.as_object_mut()) {
            if let Some(boundary) = defs.get_mut("ObservationBoundary") {
                *boundary = serde_json::json!({"type": "object", "description":
                    "The complete boundary an observation returned (scene, inspect, locate, state read), unchanged; stale ones reject."});
            }
            if name == "wait" && let Some(predicate) = defs.get_mut("Predicate") {
                *predicate = serde_json::json!({"type": "object", "description":
                    "Condition {is, target, ...}: attached, visible, opacity_at_least {milli}, text_equals {text}, value_equals {value} (a form control's value), count_at_least {n}. target is any locator, role included."});
            }
        }
        prune_definitions(map);
        // Generator metadata (dialect and Rust type name) says nothing about the arguments.
        map.remove("$schema");
        map.remove("title");
        map.values_mut().for_each(compact);
    }
    ToolDescription { name: name.into(), description: description.into(), input_schema }
}

/// Drops integer `format` annotations (`uint64`, …) and an unsigned type's `minimum: 0`: they describe
/// the Rust type, not the argument, and the server refuses an out-of-range number when it decodes the call.
fn compact(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            // A constant already constrains its JSON type.
            if map.contains_key("const") {
                map.remove("type");
            }
            let format = map.get("format").and_then(|f| f.as_str()).map(str::to_string);
            if let Some(f) = format.filter(|f| f.starts_with("uint") || f.starts_with("int")) {
                map.remove("format");
                // An unsigned type's floor says nothing an agent needs.
                if f.starts_with("uint") && map.get("minimum").and_then(|m| m.as_u64()) == Some(0) {
                    map.remove("minimum");
                }
            }
            map.values_mut().for_each(compact);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(compact),
        _ => {}
    }
}

fn launch_tool(description: &str) -> ToolDescription {
    let mut t = tool::<LaunchArgs>("launch", description);
    collapse_launch_inputs(&mut t);
    t
}

fn native_tool() -> ToolDescription {
    let schema = schemars::schema_for!(crate::native::Op);
    let operations: Vec<_> = schema.get("oneOf").and_then(|v| v.as_array())
        .expect("tagged native operation schema").iter()
        .map(|v| v["properties"]["type"]["const"].as_str().expect("native operation tag"))
        .collect();
    ToolDescription {
        name: "native".into(),
        description: "Investigate configured Android/iOS apps through native, Flutter, host or WebView runtimes. First call {op:{type:describe}} for arguments, runtime profiles, clock-control capabilities and recovery guidance. Do not assume app time is paused before selecting supported clock control.".into(),
        input_schema: schemars::json_schema!({
            "type": "object",
            "properties": {"op": {
                "type": "object",
                "properties": {"type": {"type": "string", "enum": operations}},
                "required": ["type"],
                "additionalProperties": true
            }},
            "required": ["op"],
            "additionalProperties": false
        }),
    }
}

/// The front panel: the tools of the usual loop. `tools/list` shows each with its arguments and the names
/// of their choices; `{describe: true}` returns the full schema.
const FRONT_PANEL: [&str; 7] = ["launch", "status", "act", "wait", "observe", "capture", "stop"];

/// The listed tools: the front panel folded to its buttons, the drawers and `native` as they are.
pub fn descriptions() -> Vec<ToolDescription> {
    full_descriptions().into_iter().map(|d| if FRONT_PANEL.contains(&d.name.as_str()) { buttons(d) } else { d }).collect()
}

/// A front-panel tool's full argument schema, which `{describe: true}` returns.
pub fn describe_front_panel(name: &str) -> Option<serde_json::Value> {
    full_descriptions().into_iter().find(|d| d.name == name && FRONT_PANEL.contains(&name))
        .map(|d| serde_json::json!({"tool": d.name, "input_schema": d.input_schema}))
}

/// Folds a tool to its buttons: each argument keeps its description, a tagged choice lists only its tag's
/// values and any other nested object is shown as an object. Nothing is lost: the full schema is one
/// `describe` away, and every call is still decoded against it.
fn buttons(full: ToolDescription) -> ToolDescription {
    let schema = serde_json::to_value(&full.input_schema).expect("a schema");
    let defs = schema.get("$defs").cloned().unwrap_or_default();
    let mut properties = serde_json::Map::new();
    for (name, argument) in schema["properties"].as_object().into_iter().flatten() {
        properties.insert(name.clone(), button(argument, &defs));
    }
    properties.insert("describe".into(), serde_json::json!({"const": true, "description": "Send alone for this tool's full schema."}));
    let mut folded = serde_json::json!({"type": "object", "properties": properties});
    if let Some(required) = schema.get("required") {
        folded["anyOf"] = serde_json::json!([{"required": required}, {"required": ["describe"]}]);
    }
    let input_schema = serde_json::from_value(folded).expect("a folded schema");
    ToolDescription { name: full.name, description: full.description, input_schema }
}

fn button(argument: &serde_json::Value, defs: &serde_json::Value) -> serde_json::Value {
    let resolved = resolve(argument, defs);
    let description = argument.get("description").or_else(|| resolved.get("description")).cloned();
    let mut shown = if let Some(variants) = resolved.get("oneOf").and_then(|v| v.as_array()) {
        match choice_tag(variants) {
            Some((tag, names)) => serde_json::json!({"type": "object", "properties": {tag.clone(): {"enum": names}}, "required": [tag]}),
            None => {
                let names: Vec<_> = variants.iter().filter_map(|v| v.get("const").cloned()).collect();
                if names.len() == variants.len() { serde_json::json!({"enum": names}) } else { serde_json::json!({}) }
            }
        }
    } else if let Some(values) = resolved.get("enum") {
        serde_json::json!({"enum": values})
    } else {
        match resolved.get("type") {
            Some(t) if t == "object" => serde_json::json!({"type": "object"}),
            Some(t) if t == "array" => serde_json::json!({"type": "array"}),
            Some(t) => serde_json::json!({"type": t}),
            None => serde_json::json!({}),
        }
    };
    if let Some(description) = description {
        shown["description"] = description;
    }
    shown
}

/// Follows `$ref`s, and an optional value's `anyOf: [T, null]` to `T`.
fn resolve(schema: &serde_json::Value, defs: &serde_json::Value) -> serde_json::Value {
    let mut current = schema.clone();
    loop {
        if let Some(name) = current.get("$ref").and_then(|r| r.as_str()).and_then(|r| r.strip_prefix("#/$defs/")) {
            current = defs[name].clone();
            continue;
        }
        if let Some(options) = current.get("anyOf").and_then(|v| v.as_array()) {
            let present: Vec<_> = options.iter().filter(|o| o.get("type") != Some(&serde_json::json!("null"))).collect();
            if present.len() == 1 {
                current = present[0].clone();
                continue;
            }
        }
        return current;
    }
}

/// The property every variant of a tagged choice fixes (`type`, `by`, ...), with each variant's value.
fn choice_tag(variants: &[serde_json::Value]) -> Option<(String, Vec<serde_json::Value>)> {
    let value = |variant: &serde_json::Value, tag: &str| {
        let fixed = &variant["properties"][tag];
        fixed.get("const").cloned().or_else(|| fixed["enum"].as_array().filter(|e| e.len() == 1).map(|e| e[0].clone()))
    };
    let first = variants.first()?["properties"].as_object()?;
    first.keys().find_map(|tag| {
        let names: Option<Vec<_>> = variants.iter().map(|v| value(v, tag)).collect();
        names.map(|names| (tag.clone(), names))
    })
}

fn full_descriptions() -> Vec<ToolDescription> {
    vec![
        native_tool(),
        tool::<NoArgs>("bootstrap", "Start here: host capabilities, live generations and the next action."),
        launch_tool("Open a paused generation and return its id. Unmatched external requests are blocked; the platform answers CORS."),
        tool::<GenerationArg>("status", "Clock, lease ownership (self/other/none), held requests, watches, last request and faults; stopped sessions use evidence."),
        tool::<ActArgs>("act", "User input, then settle unless then is none. Reports network, navigation, console, DOM, dialog and exception effects."),
        tool::<WaitArgs>("wait", "Progress, then pause; or read the clock. Its end is not an observation boundary: take one from observe."),
        tool::<ObserveArgs>("observe", "Read the frozen page without changing it. Returned references are locators in this generation ({\"by\": \"ref\", \"id\": \"e3-1a2b3c\"})."),
        tool::<CaptureArgs>("capture", "Capture/record presentations or read evidence after stop. Reads never move time; exact/current report freshness."),
        tool::<StopArgs>("stop", "Stop; report evidence finalization."),
        drawer_tool::<NetworkOp>("network", "inspect or edit future network rules, or answer held requests and streams"),
        drawer_tool::<WatchOp>("watch", "named predicates kept and sampled for the generation's life, with their history"),
        drawer_tool::<SessionOp>("session", "list generations, contexts, leases, results and dialogs; cancel a wait"),
        drawer_tool::<StateOp>("state", "read or change page state at a checked boundary; save starting points and branch them"),
        drawer_tool::<InputOp>("input", "held pointer, keys and composition; wheel. Time stays stopped"),
        drawer_tool::<DebugOp>("debug", "evaluate (taints), history, result pages, bundle, replay; offline video/artifacts"),
    ]
}

#[cfg(test)]
#[path = "tools_tests.rs"]
mod tests;
