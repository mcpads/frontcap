//! Typed messages between control servers and a generation's recorder.
pub mod client;
pub mod frame;
pub mod native;

use frontcap_core::action::{Action, ActionKind};
use frontcap_core::clock::{SettleOutcome, WaitOutcome};
use frontcap_core::event::{ClockOutcome, PointKind, SampleValue};
use frontcap_core::ids::{LaunchId, RequestId, RunId, WatchName};
use frontcap_core::predicate::{Locator, Predicate};
use frontcap_core::rules::{FailReason, MockSpec};
use frontcap_core::time::VirtualTime;
use serde::{Deserialize, Serialize};

/// A control lease: 32 lowercase hex digits from the system's random source.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct LeaseToken(String);

#[derive(Debug, thiserror::Error)]
#[error("not a lease token: {0:?}")]
pub struct InvalidLease(pub String);

impl LeaseToken {
    pub fn new(s: impl Into<String>) -> Result<LeaseToken, InvalidLease> {
        let s = s.into();
        if s.len() == 32 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            Ok(LeaseToken(s))
        } else {
            Err(InvalidLease(s))
        }
    }

    pub fn generate() -> LeaseToken {
        use std::io::Read;
        let mut bytes = [0u8; 16];
        std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).expect("system random source");
        LeaseToken(bytes.iter().map(|b| format!("{b:02x}")).collect())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for LeaseToken {
    type Error = InvalidLease;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        LeaseToken::new(s)
    }
}

impl From<LeaseToken> for String {
    fn from(t: LeaseToken) -> String {
        t.0
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case", deny_unknown_fields)]
pub enum Call {
    /// Native operations use the same lease and durable caller-ID namespace.
    Native { lease: LeaseToken, request_id: RequestId, op: native::Op, budget_ms: u64 },
    Hello,
    Status,
    /// Read status and compare a supplied lease with the current owner without acquiring control.
    InspectControl { lease: Option<LeaseToken> },
    Result { request_id: RequestId },
    AnswerDialog { lease: LeaseToken, request_id: RequestId, response: DialogResponse },
    Reattach { request_id: RequestId },
    Release { lease: LeaseToken, request_id: RequestId },
    Mutate { lease: Option<LeaseToken>, request_id: RequestId, op: Op },
    /// Asks the running request or probe `request_id` to stop at its next serviceable boundary: a wait at
    /// the frame tick after it sees this, a probe between its steps too. The request answers for itself.
    /// With `at_us`, arms a request that has not begun to have its waits cancelled at that instant, as a
    /// replay of a cancelled request does. It needs the live lease, or none when no controller holds one:
    /// a lost controller's running request is cancelled by whoever finds it.
    Cancel {
        lease: Option<LeaseToken>,
        request_id: RequestId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_us: Option<frontcap_core::time::VirtualTime>,
    },
    /// What the trace records at sequence numbers from `after_seq` on, and below `before_seq` if given.
    Effects { after_seq: u64, before_seq: Option<u64> },
    WatchHistory { name: WatchName },
    /// Observation of the frozen page; `busy` while a request runs.
    Observe {
        op: ObserveOp,
        /// The context to observe: the launch page by default, or a popup (`popup-1`). A reference names its
        /// own context.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
    },
    /// Issues node references from their resolution records, as a replayed run's snapshots did; not
    /// journaled (binding again binds the same nodes). `stale_ref` names the first that is not there.
    BindRefs { lease: LeaseToken, refs: std::collections::BTreeMap<String, frontcap_core::inputs::Resolution> },
    /// Forgets the names `BindRefs` bound from a recording once a replayed prefix is over: the generation
    /// then names its nodes itself and refuses the recording's references. Not journaled (forgetting again
    /// changes nothing).
    RetireBoundRefs { lease: LeaseToken },
    /// A finalized copy of the evidence so far under `dest/<run>`, taken at a frozen boundary with no
    /// request running: the prefix a replay basis repeats. Needs no lease; changes nothing in the target.
    SavePrefix { dest: std::path::PathBuf },
    /// A process image of the browser at a frozen boundary with no request running, named `name`
    /// (letters, digits, `-`, `_`) in the workspace's images: CRIU's images, the cell's directory, the run's
    /// evidence and the recorder's control state so far. Needs no lease; the generation goes on, its
    /// boundary, requests and evidence unchanged.
    Image { name: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObserveOp {
    NetworkRules,
    /// The live state a recipe saved from this generation reconstructs, with its launch inputs.
    RecipeCapture,
    InputState,
    StorageScope { target: Locator },
    StateRead { target: Locator, field: frontcap_core::intervention::StateField },
    /// A predicate's value now, as a watch would sample it; nothing is kept.
    Predicate { predicate: Predicate },
    LocateImage { image_id: String, point: [f64; 2] },
    Crop { image_id: String, rect: [u32; 4] },
    Locate { boundary: frontcap_core::observation::ObservationBoundary, point: [f64; 2] },
    Inspect { target: Locator, props: Vec<String> },
    Scene { scope: Option<Locator>, depth: Option<u32>, max_nodes: Option<u32>, image: bool, #[serde(default)] unfold: bool },
    Snapshot { scope: Option<Locator>, depth: Option<u32>, max_nodes: Option<u32>, #[serde(default)] geometry: bool, #[serde(default)] unfold: bool },
    Diff { since: u64, max_nodes: Option<u32>, #[serde(default)] unfold: bool },
    Query { target: Locator, props: Vec<String> },
    /// How many nodes `target` matches.
    Count { target: Locator },
    Console { filter: ConsoleFilter },
    Network { filter: NetworkFilter },
    /// The last rendered frame (S16).
    Screenshot {},
    /// Crop current presented pixels to a checked element without rendering or scrolling.
    ScreenshotTarget { target: Locator },
    /// The generation's contexts: the launch page and the windows its pages opened.
    Contexts {},
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleFilter {
    pub since_us: Option<u64>,
    pub until_us: Option<u64>,
    /// `log`, `info`, `warning`, `error`, …; `exception` selects uncaught exceptions.
    pub level: Option<String>,
    pub text_contains: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkFilter {
    pub since_us: Option<u64>,
    pub until_us: Option<u64>,
    pub url_contains: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleEntry {
    pub t_us: VirtualTime,
    pub level: String,
    pub text: String,
    /// The source of a message of the browser's own (`network`, `security`, …); absent for the page's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The originating popup; absent for the main page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// In effects, how many times the same message came (from `t_us` on); absent for once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkEntry {
    /// The request's ordinal (`req-<n>`), as held requests and response stops name it.
    pub ordinal: String,
    /// The instant the request was issued.
    pub t_us: VirtualTime,
    /// The instant its response or failure was delivered; absent while it is pending or held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answered_us: Option<VirtualTime>,
    /// `METHOD url → outcome(rule=id) status`, as in `act` effects.
    pub request: String,
    /// The first 4 KiB of the text body the page sent, when it sent one the engine showed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The whole body's size in UTF-8 bytes; larger than `body` when it was cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes: Option<u64>,
    /// The request has a body the engine did not show (a large or file upload).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub body_unavailable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "observation", rename_all = "snake_case")]
pub enum Observation {
    NetworkRules(frontcap_core::rules::RuleSnapshot),
    RecipeCapture(Box<frontcap_core::basis::RecipeCapture>),
    /// Boxed like `Outcome::Pointer`: the held-pointer state dominates the observation size.
    Input(Box<frontcap_core::pointer::PointerState>),
    StorageScope(frontcap_core::intervention::ScopedStorage),
    State(Box<frontcap_core::intervention::StateValue>),
    Sample { value: SampleValue },
    Image(frontcap_core::observation::ImageAnchor),
    Located(Box<frontcap_core::observation::Located>),
    Inspection(frontcap_core::observation::Inspection),
    Scene(Box<frontcap_core::observation::Scene>),
    Snapshot(frontcap_core::observation::Snapshot),
    Diff(frontcap_core::observation::Diff),
    Query(frontcap_core::observation::QueryResult),
    Count { count: u32 },
    Console { entries: Vec<ConsoleEntry> },
    Network { entries: Vec<NetworkEntry> },
    Screenshot(frontcap_core::observation::Captured),
    Contexts { contexts: Vec<frontcap_core::observation::ContextEntry> },
}

/// One server event on a held stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StreamCommand {
    /// An EventSource's message (`data`, with its `event` name and `id`), or a WebSocket's text.
    Message {
        data: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        event: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },
    /// The server's close: an EventSource reconnects after its retry delay; a WebSocket closes with
    /// `code` (1000 when none).
    Close {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code: Option<u16>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    EditRules { expected_revision: u64, boundary: frontcap_core::observation::ObservationBoundary, edit: frontcap_core::rules::RuleEdit },
    SetRules { expected_revision: u64, boundary: frontcap_core::observation::ObservationBoundary, rules: frontcap_core::rules::RuleSet },
    /// `context` names a popup (default: the launch page); `boundary` must be that context's.
    StateApply {
        boundary: frontcap_core::observation::ObservationBoundary,
        changes: Vec<frontcap_core::intervention::StateChange>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        /// Virtual time an app provider's write may be given to settle, within its own `max_ms`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress_us: Option<u64>,
    },
    Advance { by_us: u64 },
    Settle { idle_us: u64, max_us: u64 },
    Until { watch: WatchName, max_us: u64 },
    /// `context`: the popup the predicate is judged in (its references name it too); the launch page
    /// without one.
    UntilPredicate {
        predicate: Predicate,
        max_us: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
    },
    Next { event: frontcap_core::event::ProgressEvent, max_us: u64 },
    /// `context` as for `UntilPredicate`.
    WatchAdd {
        name: WatchName,
        predicate: Predicate,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
    },
    WatchRemove { name: WatchName },
    Respond { ordinal: String, response: MockSpec },
    /// Puts dialog rules in force (replacing the earlier ones); dialogs without a matching rule are dismissed.
    DialogRules { rules: frontcap_core::dialog::DialogRules },
    FailHeld { ordinal: String, reason: FailReason },
    /// What the agent sends, at the current instant, on a stream a `hold` rule keeps and it opened.
    StreamSend { ordinal: String, command: StreamCommand },
    Stop,
    /// A debug escape hatch, always recorded as taint.
    Evaluate { expression: String },
    /// The same escape hatch in the main world of the document shown by one frame element.
    EvaluateInFrame { expression: String, frame: Locator },
    /// Keep every rendered frame in the bundle from now on.
    RecordStart,
    /// Stop keeping frames; answers the frames kept.
    RecordStop,
    /// An action and, optionally, a settle after it: one request, so a retry with other arguments
    /// conflicts and an interrupted one resumes where it stopped.
    Act {
        action: Action,
        then: Option<SettleSpec>,
        /// The context the input goes to: the launch page by default, or a popup. A reference names its own.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
    },
    /// Steps run in order as one request (a composed probe): each state-changing step under its own
    /// request id derived from this one (`probe-<n>`), reads answered at the boundary reached. It stops at
    /// the first step that does not complete. Nothing else changes the generation meanwhile.
    Probe { steps: Vec<ProbeStep> },
}

/// The most steps one probe runs.
pub const MAX_PROBE_STEPS: usize = 32;

/// One step of a probe: what a separate call would send.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProbeStep {
    /// Advance to an offset after a completed earlier step (zero-based); never move time backward.
    WaitFrom { step_index: usize, after_us: u64 },
    /// A state-changing request, as `Call::Mutate` sends it (neither a probe nor `stop`).
    Mutate { op: Op },
    /// Typed state changes at the boundary the probe has reached.
    Apply {
        changes: Vec<frontcap_core::intervention::StateChange>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        progress_us: Option<u64>,
    },
    /// A read, as `Call::Observe` sends it.
    Observe {
        op: ObserveOp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
    },
    /// Input on the generation's held pointer at the boundary the probe has reached: the recorder names
    /// both when the step runs, which the caller cannot before the child exists.
    Input { op: ProbeInput },
}

/// Input a probe step sends, as `Action`'s without the handle and boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProbeInput {
    Begin {
        target: Locator,
        point: [i32; 2],
        #[serde(default)]
        button: frontcap_core::pointer::PointerButton,
        #[serde(default)]
        pointer: frontcap_core::pointer::PointerKind,
    },
    Move { point: [i32; 2] },
    Press { button: frontcap_core::pointer::PointerButton },
    Release { button: frontcap_core::pointer::PointerButton },
    TouchAdd { target: Locator, point: [i32; 2] },
    TouchMove { contact: u32, point: [i32; 2] },
    TouchEnd { contact: u32 },
    End {},
    Cancel {},
    Wheel { target: Locator, point: [i32; 2], delta: [i32; 2] },
    KeyDown { key: String },
    KeyUp { key: String },
    KeyRepeat { key: String },
    Compose { text: String },
    Commit {
        #[serde(default)]
        key: Option<String>,
    },
}

impl ProbeInput {
    /// The action on the held pointer's `handle` at `boundary`; `None` when it needs a pointer and none
    /// is held.
    pub fn action(self, handle: Option<frontcap_core::pointer::PointerHandle>, boundary: frontcap_core::observation::ObservationBoundary) -> Option<Action> {
        Some(match self {
            Self::Begin { target, point, button, pointer } => Action::PointerBegin { target, point, button, pointer, boundary },
            Self::Wheel { target, point, delta } => Action::Wheel { target, point, delta, boundary },
            Self::Move { point } => Action::PointerMove { handle: handle?, point, boundary },
            Self::Press { button } => Action::PointerPress { handle: handle?, button, boundary },
            Self::Release { button } => Action::PointerRelease { handle: handle?, button },
            Self::TouchAdd { target, point } => Action::TouchAdd { handle: handle?, target, point, boundary },
            Self::TouchMove { contact, point } => Action::TouchMove { handle: handle?, contact, point, boundary },
            Self::TouchEnd { contact } => Action::TouchEnd { handle: handle?, contact },
            Self::End {} => Action::PointerEnd { handle: handle? },
            Self::Cancel {} => Action::PointerCancel { handle: handle? },
            Self::KeyDown { key } => Action::KeyDown { key },
            Self::KeyUp { key } => Action::KeyUp { key },
            Self::KeyRepeat { key } => Action::KeyRepeat { key },
            Self::Compose { text } => Action::Compose { text },
            Self::Commit { key } => Action::Commit { key },
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettleSpec {
    pub idle_us: u64,
    pub max_us: u64,
}

/// Recovery classes of `runtime-lifecycle.md#mutation-requests`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpClass {
    Intervention,
    RecorderState,
    Clock,
    Input,
    Lifecycle,
    /// Script whose effects leave no evidence that survives a fault (the engine-patch class).
    Script,
    /// An action that replaces or closes a document, which holds its evidence (the engine-patch class).
    Navigation,
    /// A composed probe, whose steps are requests of their own.
    Probe,
}

impl Op {
    pub fn class(&self) -> OpClass {
        match self {
            Op::Advance { .. } | Op::Settle { .. } | Op::Until { .. } | Op::UntilPredicate { .. } | Op::Next { .. } => OpClass::Clock,
            Op::Act { action, .. } => match action.kind() {
                ActionKind::Input => OpClass::Input,
                ActionKind::Navigation | ActionKind::Close => OpClass::Navigation,
                ActionKind::Wait => OpClass::Clock,
            },
            Op::SetRules { .. } | Op::EditRules { .. } | Op::WatchAdd { .. } | Op::WatchRemove { .. } | Op::Respond { .. } | Op::FailHeld { .. } | Op::StreamSend { .. } | Op::DialogRules { .. } | Op::RecordStart | Op::RecordStop => {
                OpClass::RecorderState
            }
            Op::Stop => OpClass::Lifecycle,
            Op::Evaluate { .. } | Op::EvaluateInFrame { .. } => OpClass::Script,
            Op::Probe { .. } => OpClass::Probe,
            Op::StateApply { .. } => OpClass::Intervention,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    RulesInstalled(frontcap_core::rules::RuleReceipt),
    /// An armed event wait, including the actual terminal trace boundary.
    Progress { result: ClockOutcome, start: Boundary, end: Boundary },
    /// A held input operation and the trace positions around it, so a repeat reports the same effects.
    /// Boxed: the held-pointer state is several times larger than every other outcome.
    Pointer { state: Box<frontcap_core::pointer::PointerState>, start: Boundary, end: Boundary },
    StateApplied(frontcap_core::intervention::AppliedState),
    /// A clock request: where it started and ended, and the trace positions around it so a repeat reports
    /// the same effects (absent in requests journaled before they were kept).
    Clock {
        result: ClockOutcome,
        start_us: VirtualTime,
        t_us: VirtualTime,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<Boundary>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end: Option<Boundary>,
    },
    /// A settle: where it started, and where it left the trace.
    Settled {
        result: SettleOutcome,
        start_us: VirtualTime,
        end: Boundary,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        start: Option<Boundary>,
    },
    /// An act, with the trace positions around it so a repeat reports the same effects: where the action
    /// itself ended (`acted`), what `wait_actionable` found, and the settle's result if one was asked for.
    Acted { start: Boundary, acted: Boundary, waited: Option<WaitOutcome>, settle: Option<SettleOutcome>, end: Boundary },
    Sample { value: SampleValue },
    Ack,
    Evaluated { value: serde_json::Value },
    Stopped { finalized: bool },
    Recorded(frontcap_core::observation::Recording),
    /// A probe's step answers, in order, as separate calls would have answered them; `completed` steps
    /// completed (the answer after them, if any, is the step that stopped the probe). `end` is the
    /// boundary it left the generation at, none while a stopping step still holds it (suspended).
    /// `cancelled`: the controller cancelled it, and no step ran after that was seen. `interrupted`: a
    /// recorder fault interrupted it; its answers are the steps' saved or recovered ones, and no step ran
    /// after the fault.
    Probed {
        results: Vec<Answer>,
        completed: usize,
        end: Option<frontcap_core::observation::ObservationBoundary>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        cancelled: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        interrupted: bool,
    },
    /// The request never reached dispatch; it may be sent again under a new request id.
    NotApplied,
}

/// A position in a generation's history: virtual time, the trace's next sequence number, and the DOM
/// mutation count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Boundary {
    pub t_us: VirtualTime,
    pub seq: u64,
    pub dom_mutations: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fault {
    pub at_us: VirtualTime,
    /// Ordinals of network requests and ids of control requests the fault affected.
    pub affected: Vec<String>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusReport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspension: Option<SuspendedOperation>,
    #[serde(default)]
    pub input_cleanup_pending: bool,
    #[serde(default)]
    pub pointer: Option<frontcap_core::pointer::HeldPointer>,
    /// Keys held by `key_down`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<String>,
    /// Whether a composition may be in progress (cleanup cancels it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub composing: bool,
    pub launch_id: LaunchId,
    pub run_id: RunId,
    pub t_us: VirtualTime,
    /// The page's `Date.now()` at `t_us`: the profile's initial date plus the virtual time.
    #[serde(default)]
    pub date_ms: u64,
    pub lease_held: bool,
    pub running: Option<RequestId>,
    /// While a request runs, the virtual instant it has reached (`t_us` is the last committed one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_t_us: Option<VirtualTime>,
    pub held: Vec<String>,
    /// The held requests with what they ask for.
    #[serde(default)]
    pub held_requests: Vec<HeldRequest>,
    pub watches: Vec<WatchName>,
    /// Each watch's predicate, which `watch list` shows.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub watch_predicates: std::collections::BTreeMap<WatchName, frontcap_core::predicate::Predicate>,
    pub last_completed_request_id: Option<RequestId>,
    pub faults: Vec<Fault>,
    pub stopped: bool,
    /// The trace's next sequence number: everything recorded so far is below it.
    pub observation_seq: u64,
    /// DOM mutation records the page's observer has reported so far.
    pub dom_mutations: u64,
    /// The evidence bundle's directory.
    pub bundle: std::path::PathBuf,
}

/// A response is a separate mutation bound to exactly one running operation and native dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogResponse {
    pub parent_request_id: RequestId,
    pub dialog_id: String,
    pub accept: bool,
    pub prompt_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SuspendedOperation {
    pub request_id: RequestId,
    /// A dialog the operation's page work waits for an answer to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialog: Option<frontcap_core::dialog::DialogDescriptor>,
    /// A held request the page's main thread waits for (engine patch 53), answered with `Respond` or
    /// `FailHeld` naming its ordinal. Boxed: it is rare and keeps answers small.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<Box<BlockingRequest>>,
}

impl SuspendedOperation {
    /// Where the page's clock stands while the operation waits.
    pub fn t_us(&self) -> frontcap_core::time::VirtualTime {
        self.dialog.as_ref().map(|d| d.t_us).or(self.request.as_ref().map(|r| r.t_us)).unwrap_or_default()
    }
}

/// A held request the page's main thread waits for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockingRequest {
    pub ordinal: String,
    pub method: String,
    pub url: String,
    pub rule: frontcap_core::ids::RuleId,
    pub rules_revision: u64,
    pub t_us: frontcap_core::time::VirtualTime,
}

/// A request a `hold` rule keeps for the agent to answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldRequest {
    pub ordinal: String,
    pub method: String,
    pub url: String,
    pub rule: frontcap_core::ids::RuleId,
    #[serde(default)]
    pub rules_revision: u64,
    /// The first 4 KiB of the request body as text, when it has one the engine showed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The whole body's size in UTF-8 bytes; larger than `body` when it was cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_bytes: Option<usize>,
    /// The request has a body the engine did not show (a large or file upload).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub body_unavailable: bool,
    /// A held stream's state: `connecting` until its handshake is answered, then `open`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
}

impl HeldRequest {
    pub const BODY_LIMIT: usize = 4096;

    /// The same request without its body, for summaries that list requests only.
    pub fn without_body(mut self) -> HeldRequest {
        self.body = None;
        self.body_bytes = None;
        self.body_unavailable = false;
        self
    }
}

/// What a step did, read from the trace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Effects {
    /// Observed events receiving an input's marked substeps; absent for older observer recordings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input: Vec<InputEffect>,
    /// `METHOD url → outcome(rule=id) status`, in issue order.
    pub requests: Vec<String>,
    pub navigations: Vec<String>,
    pub console: ConsoleCounts,
    /// Errors, assertions and warnings in trace order, with their original text and provenance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub console_messages: Vec<ConsoleEntry>,
    /// Requests no caller rule matched (answered by `default-block`), as `METHOD url`.
    pub unmatched_requests: Vec<String>,
    /// What to do about them, naming their origins.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unmatched_next: Option<String>,
    /// Dialogs the platform answered, as `kind "message" → answer`.
    pub dialogs: Vec<String>,
    /// Uncaught exceptions.
    pub exceptions: Vec<String>,
    /// Contexts opened and closed (`popup-1 opened by page`, `popup-1 closed`) and windows refused
    /// (`refused: …`). A popup's own entries above start with its context (`popup-1: GET …`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contexts: Vec<String>,
    /// Focus transitions in order, as `focus <element>` and `blur <element>` (a bounded description of
    /// the field itself, inside shadow roots and frames too; inspect a scene for references).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub focus: Vec<String>,
    /// Messages windows posted to one another, as `from <origin> <data>` (a popup's own start with its
    /// context, as above); a window's messages to itself are not listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<String>,
    /// Entries past each list's first [`Effects::ENTRIES`], by list, and where to read them all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub omitted: Option<EffectsOmitted>,
}

impl Effects {
    /// What each list holds at most: effects summarize a step, and the histories hold the rest.
    pub const ENTRIES: usize = 20;
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectsOmitted {
    pub entries: std::collections::BTreeMap<String, u32>,
    pub next: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputEffect {
    pub request_id: RequestId,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    pub events: Vec<frontcap_core::event::ReceivedInputEvent>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsoleCounts {
    pub error: u32,
    pub warn: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySample {
    pub seq: u64,
    pub t_us: VirtualTime,
    pub value: SampleValue,
    pub point: PointKind,
}

/// Durable state of a mutation. Pending evidence does not by itself claim that execution is live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestResult {
    Unknown,
    Pending { dispatched: bool },
    Completed { answer: Box<Answer> },
    NotApplied,
}

/// Ownership relative to the supplied token, established by the recorder at the read boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlOwnership {
    #[serde(rename = "self")]
    ThisController,
    Other,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Native { body: native::Reply },
    Suspended { operation: SuspendedOperation },
    DialogAnswered { receipt: Box<frontcap_core::dialog::DialogReceipt> },
    Hello { launch_id: LaunchId },
    Status { status: Box<StatusReport> },
    Control { status: Box<StatusReport>, ownership: ControlOwnership },
    Result { request_id: RequestId, result: RequestResult },
    Attached { lease: LeaseToken, status: Box<StatusReport>, completed_while_detached: Vec<RequestId> },
    Released,
    Bound,
    Done { request_id: RequestId, outcome: Outcome },
    Effects { effects: Box<Effects> },
    Observed { result: Observation },
    History { samples: Vec<HistorySample> },
    /// A saved prefix: its run, the SHA-256 of its `manifest.json` and its command count.
    Prefix { run: RunId, manifest_sha256: String, commands: usize },
    /// A process image taken at `boundary`.
    Imaged { boundary: frontcap_core::observation::ObservationBoundary },
    /// The request was asked to stop; its own answer says where it did.
    Cancelling { request_id: RequestId },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    LeaseRequired,
    LeaseHeld,
    Busy,
    RequestIdConflict,
    NotActionable,
    NotFound,
    AmbiguousLocator,
    AtomicityViolation,
    PageUnresponsive,
    UnsupportedCapability,
    RecorderFault,
    NoSuchHold,
    NoSuchContext,
    ContextClosed,
    StaleRef,
    StaleCursor,
    ScriptFailed,
    NavigationCancelled,
    InvalidArguments,
    Stopped,
    NoFrame,
    StaleFrame,
    ObservationChanged,
    PageWorkPending,
    Internal,
}

impl ErrorCode {
    pub const ALL: &'static [ErrorCode] = &[
        ErrorCode::LeaseRequired,
        ErrorCode::LeaseHeld,
        ErrorCode::Busy,
        ErrorCode::RequestIdConflict,
        ErrorCode::NotActionable,
        ErrorCode::NotFound,
        ErrorCode::AmbiguousLocator,
        ErrorCode::AtomicityViolation,
        ErrorCode::PageUnresponsive,
        ErrorCode::UnsupportedCapability,
        ErrorCode::RecorderFault,
        ErrorCode::NoSuchHold,
        ErrorCode::NoSuchContext,
        ErrorCode::ContextClosed,
        ErrorCode::StaleRef,
        ErrorCode::StaleCursor,
        ErrorCode::ScriptFailed,
        ErrorCode::NavigationCancelled,
        ErrorCode::InvalidArguments,
        ErrorCode::Stopped,
        ErrorCode::NoFrame,
        ErrorCode::StaleFrame,
        ErrorCode::ObservationChanged,
        ErrorCode::PageWorkPending,
        ErrorCode::Internal,
    ];

    /// The one next action for each code.
    pub fn recovery(self) -> &'static str {
        match self {
            ErrorCode::LeaseRequired => "reattach to obtain a lease, then send the call with it",
            ErrorCode::LeaseHeld => "release the existing lease from its controller, or wait for it to expire",
            ErrorCode::Busy => "wait for the running request to complete (status), then send the call again",
            ErrorCode::RequestIdConflict => "send the call with a new request id",
            ErrorCode::NotActionable => "observe the target and act on an actionable one, under a new request_id if you named one (a refused request keeps its answer)",
            ErrorCode::NotFound => "observe the page and use a locator that matches one node",
            ErrorCode::AmbiguousLocator => "narrow the locator to one node",
            ErrorCode::AtomicityViolation => "report the defect with the trace; the input reached another node",
            ErrorCode::PageUnresponsive => "stop the generation; the page no longer answers",
            ErrorCode::UnsupportedCapability => "use an advertised capability",
            ErrorCode::RecorderFault => "read status for the affected requests, then continue",
            ErrorCode::NoSuchHold => "list held requests and answer one of them",
            ErrorCode::NoSuchContext => "list the generation's contexts (session contexts) and name one of them",
            ErrorCode::ContextClosed => "address an open context; the closed one keeps its evidence in the trace",
            ErrorCode::StaleRef => "take a fresh snapshot and use its references",
            ErrorCode::StaleCursor => "take a fresh snapshot and diff against it",
            ErrorCode::NavigationCancelled => "stay on the current document; navigate again only with a new request_id",
            ErrorCode::ScriptFailed => "fix the expression and evaluate it with a new request_id",
            ErrorCode::InvalidArguments => "send the call with arguments that match its schema",
            ErrorCode::Stopped => "launch a new generation",
            ErrorCode::NoFrame => "advance virtual time so a frame is rendered, then capture",
            ErrorCode::StaleFrame => "advance by one frame interval so the new document is rendered, then capture",
            ErrorCode::ObservationChanged => "request a fresh scene at the current stopped boundary",
            ErrorCode::PageWorkPending => "advance virtual time so the page finishes that work, then read again",
            ErrorCode::Internal => "stop the generation and report the defect with the trace",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorReply {
    pub code: ErrorCode,
    pub message: String,
    pub recovery: String,
}

impl ErrorReply {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ErrorReply {
        ErrorReply { code, message: message.into(), recovery: code.recovery().to_string() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "answer", content = "body", rename_all = "snake_case")]
pub enum Answer {
    Ok(Reply),
    Err(ErrorReply),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientFrame {
    pub id: u64,
    pub call: Call,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerFrame {
    pub id: u64,
    pub answer: Answer,
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
