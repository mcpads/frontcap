use crate::ids::{RequestId, RuleId, WatchName};
use crate::time::VirtualTime;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BehaviorEvent {
    pub seq: u64,
    pub t_us: VirtualTime,
    #[serde(flatten)]
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum EventKind {
    Clock(ClockRecord),
    Action(ActionRecord),
    Intervention(crate::intervention::InterventionRecord),
    Initialization(crate::intervention::InitializationRecord),
    IndexedDbInitialized(crate::indexed_db::DatabaseInitializationRecord),
    CacheInitialized(crate::cache::CacheInitializationRecord),
    CookiesInitialized(Vec<crate::intervention::InitialCookie>),
    Request(RequestRecord),
    Interception(InterceptionRecord),
    Response(ResponseRecord),
    Console(ConsoleRecord),
    Navigation(NavigationRecord),
    Watch(WatchSample),
    RulesInstalled(RulesInstalledRecord),
    Taint(TaintRecord),
    Controller(ControllerRecord),
    Dialog(DialogRecord),
    Exception(ExceptionRecord),
    Focus(FocusRecord),
    Stream(StreamRecord),
    Context(ContextRecord),
    Message(MessageRecord),
}

/// A message another window posted to a document (`postMessage`), as its trusted `message` event
/// reached the window. A window's messages to itself are not recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRecord {
    /// The sender's origin.
    pub origin: String,
    /// The data, as JSON when it can be written so, cut to 120 characters.
    pub data: String,
    pub document: u64,
    pub rseq: u64,
    /// The receiving context, when not the launch page (`ContextRecord`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// A page context of the generation opened or closed. Besides the launch page (`page`), a window a
/// context opened (`window.open`, a link or form aimed at a new window) is one (`popup-1`, `popup-2`… in
/// the order they were opened), enrolled in the generation before it ran anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRecord {
    pub context: String,
    pub event: ContextEvent,
    /// The context that opened it (`opened` only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opener: Option<String>,
    /// Browser-local identity by which a relaunched recorder finds the window again (`opened` only),
    /// excluded from comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Opened without an opener (`opened` only): it opened at the first stopped boundary after the page
    /// asked for it, and its page cannot reach its opener.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_opener: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextEvent {
    Opened,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamDirection {
    /// Delivered to the page by the platform (the delivery barrier passed).
    In,
    /// Sent by the page.
    Out,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamEventKind {
    Open,
    Message,
    Error,
    Close,
}

/// One event of a stream (Server-Sent Events or WebSocket): the request ordinal of its handshake, and
/// for an `open` the page's stream id and document, by which a relaunched recorder finds it again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamRecord {
    pub stream: String,
    pub direction: StreamDirection,
    pub event: StreamEventKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_stream: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<u64>,
    /// Browser-local publication identity, retained for recovery until the trace is synced.
    /// It is not a delivery acknowledgement from the remote peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<StreamReceipt>,
    /// The popup whose page has the stream; absent for the launch page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamReceipt {
    pub id: String,
    pub frame: String,
    pub document: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogKind {
    Alert,
    Confirm,
    Prompt,
    Beforeunload,
}

/// A dialog the platform answered in the page's place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogRecord {
    pub kind: DialogKind,
    pub message: String,
    /// What the page received: `returned` (alert), `false` (confirm), `null` (prompt), `accepted`.
    pub answer: String,
    /// Logical identity and resolution source for a browser-owned dialog.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeDialogEvidence>,
    /// Durable position in the document observer stream, when the wrapper witnessed the call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer: Option<DialogObserver>,
    /// The context that opened it, when not the launch page (`ContextRecord`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DialogObserver {
    pub document: u64,
    pub rseq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeDialogEvidence {
    /// Generation-local ordinal (`dialog-1`, ...); the engine identity lives in control metadata.
    pub id: String,
    pub source: NativeDialogSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeDialogSource {
    Answered,
    Cancelled,
    External,
}

/// An element gaining or losing focus; `target` is a bounded description, not a reusable locator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusRecord {
    pub target: String,
    pub focused: bool,
    pub document: u64,
    pub rseq: u64,
    /// The context it happened in, when not the launch page (`ContextRecord`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// An exception no page code caught.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExceptionRecord {
    pub text: String,
    /// Browser-local identity retained for recorder reattachment, excluded from comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeException>,
    /// Browser-owned evidence, retained until the trace is synced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub receipt: Option<ExceptionReceipt>,
    /// The context that threw it, when not the launch page (`ContextRecord`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExceptionReceipt {
    pub id: String,
    pub frame: String,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NativeException {
    pub context: String,
    pub id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockOp {
    Advance,
    Frames,
    Settle,
    Until,
    Next,
    WaitActionable,
    /// A drag's explicit interval before a held move (S17).
    Drag,
    /// The latency of a request the page's main thread waited for: the page could not run, and the
    /// clock moved to the response's arrival (engine patch 53).
    SynchronousRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockPhase {
    Start,
    End,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClockOutcome {
    Reached,
    Quiescent,
    BudgetExhausted { still_active: Vec<String> },
    ConditionMet { watch: WatchName },
    ConditionTimedOut { watch: WatchName },
    Actionable,
    NotActionable { reason: String },
    Event { event: ProgressEvent, triggers: Vec<ProgressTrigger> },
    EventTimedOut { event: ProgressEvent },
    /// The controller cancelled the wait; it stopped at the frame tick after the request was seen.
    Cancelled,
}

/// Serviceable boundaries available to an explicitly armed progress request.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressEvent {
    Response,
    Presentation,
    Navigation,
    Exception,
    Interception,
    Focus,
    Dialog,
    // A window a context opened, enrolled and stopped before its first document is answered.
    Popup,
    // A stream event delivered to the page (its opening, a message, an error or a close), stopped as a
    // response is: after the instant's deliveries and their handlers.
    Stream,
}

fn page_context() -> String {
    "page".into()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProgressTrigger {
    /// A response recorded after the arm watermark. Body-consumption coverage is that of its source.
    Response { seq: u64, t_us: VirtualTime, ordinal: String },
    /// A new activated main-document surface, with the paint attribution supplied by the engine, and the
    /// context whose surface it is (the launch page, `page`, in traces from before it was named).
    Presentation { t_us: VirtualTime, painted_at_us: VirtualTime, document: u64, #[serde(default = "page_context")] context: String },
    Navigation { seq: u64, t_us: VirtualTime, url: String, document: u64 },
    Exception { seq: u64, t_us: VirtualTime, text: String },
    Focus { seq: u64, t_us: VirtualTime, target: String, focused: bool, document: u64 },
    Dialog { seq: u64, t_us: VirtualTime, dialog_id: String, dialog_kind: DialogKind, message: String },
    Interception { seq: u64, t_us: VirtualTime, ordinal: String, method: String, url: String },
    Popup { seq: u64, t_us: VirtualTime, context: String, opener: String },
    Stream {
        seq: u64,
        t_us: VirtualTime,
        stream: String,
        event: StreamEventKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<String>,
    },
}

impl ProgressEvent {
    pub fn trace_trigger(self, seq: u64, t_us: VirtualTime, kind: &EventKind) -> Option<ProgressTrigger> {
        match (self, kind) {
            (Self::Interception, EventKind::Interception(r)) => Some(ProgressTrigger::Interception {
                seq, t_us, ordinal: r.ordinal.clone(), method: r.method.clone(), url: r.url.clone(),
            }),
            // A preflight the platform answered is never handed to page script.
            (Self::Response, EventKind::Response(r)) if r.source != ResponseSource::Preflight => Some(ProgressTrigger::Response { seq, t_us, ordinal: r.id.clone() }),
            (Self::Navigation, EventKind::Navigation(n)) => Some(ProgressTrigger::Navigation { seq, t_us, url: n.url.clone(), document: n.document }),
            (Self::Dialog, EventKind::Dialog(d)) => d.native.as_ref().map(|native| ProgressTrigger::Dialog {
                seq, t_us, dialog_id: native.id.clone(), dialog_kind: d.kind, message: d.message.clone(),
            }),
            (Self::Focus, EventKind::Focus(f)) => Some(ProgressTrigger::Focus {
                seq, t_us, target: f.target.clone(), focused: f.focused, document: f.document,
            }),
            (Self::Exception, EventKind::Exception(e)) => Some(ProgressTrigger::Exception { seq, t_us, text: e.text.clone() }),
            (Self::Popup, EventKind::Context(c)) if c.event == ContextEvent::Opened => Some(ProgressTrigger::Popup {
                seq, t_us, context: c.context.clone(), opener: c.opener.clone().unwrap_or_default(),
            }),
            (Self::Stream, EventKind::Stream(s)) if s.direction == StreamDirection::In => Some(ProgressTrigger::Stream {
                seq, t_us, stream: s.stream.clone(), event: s.event, context: s.context.clone(),
            }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClockRecord {
    pub op: ClockOp,
    pub phase: ClockPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_us: Option<VirtualTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ClockOutcome>,
    /// Wall-clock duration of the operation; excluded from every comparison projection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionRecord {
    /// Events observed while the input's substeps were marked. Absent when the document was lost
    /// or its observer predates this evidence; an empty list means no matching event was observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub received_events: Option<Vec<ReceivedInputEvent>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<crate::pointer::PointerHold>,
    /// The keys held after a `key_down` or `key_up`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keys: Option<Vec<String>>,
    pub request_id: RequestId,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// The action's arguments other than its target (text typed, key pressed, URL, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_to: Option<String>,
    /// The popup the action addressed; absent for the launch page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceivedInputEvent {
    pub step: usize,
    pub event: String,
    pub t_us: VirtualTime,
    pub trusted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterceptionRecord {
    pub ordinal: String,
    pub method: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestRecord {
    /// The immutable rule installation selected when this request was intercepted.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rules_revision: u64,
    /// Ordinal id assigned by the recorder (`req-1`, `req-2`, …), never an engine id.
    pub id: String,
    pub method: String,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<RuleId>,
    /// The cassette line a `replay` rule chose for the request, recorded when it chose it: the
    /// consumption a relaunched recorder re-derives, answered or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<u64>,
    /// The context that issued it, when not the launch page (`ContextRecord`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// What the page sent, when the request has a body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<RequestBody>,
}

/// A request's body as the engine showed it: the first 4 KiB of its text and its whole size, or that it
/// has one the engine did not show (a large or file upload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestBody {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The whole body's size in UTF-8 bytes; larger than `text` when it was cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unavailable: bool,
}

impl RequestBody {
    pub const LIMIT: usize = 4096;

    /// From an intercepted request's `postData` and `hasPostData`; `None` without a body.
    pub fn of(post_data: Option<&str>, has_post_data: Option<bool>) -> Option<RequestBody> {
        match post_data {
            Some(b) => {
                let mut end = b.len().min(Self::LIMIT);
                while !b.is_char_boundary(end) {
                    end -= 1;
                }
                Some(RequestBody { text: Some(b[..end].to_string()), bytes: Some(b.len() as u64), unavailable: false })
            }
            None if has_post_data == Some(true) => Some(RequestBody { text: None, bytes: None, unavailable: true }),
            None => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseSource {
    Mock,
    /// From a cassette (`replay`).
    Replay,
    Passthrough,
    Fail,
    Agent,
    /// A CORS preflight the platform answered for the request it precedes, as the server its rule
    /// stands in for would.
    Preflight,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseRecord {
    pub id: String,
    pub status: u16,
    pub source: ResponseSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<RuleId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleRecord {
    pub level: String,
    pub text: String,
    /// The context that logged it, when not the launch page (`ContextRecord`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// For a message of the browser's own (`network`, `security`, `deprecation`, `intervention`, …): its
    /// source. Absent for the page's console calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigationRecord {
    /// Retained observer receipt for a committed same-document navigation.
    #[serde(default)]
    pub rseq: u64,
    pub url: String,
    /// Ordinal of the document instance within the generation (within its context, for a popup's).
    pub document: u64,
    /// The context that navigated, when not the launch page (`ContextRecord`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SampleValue {
    True,
    False,
    Unknown,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointKind {
    Initial,
    Mutation,
    Frame,
    Frozen,
    AnimationEnd,
    Final,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchSample {
    pub name: WatchName,
    pub value: SampleValue,
    pub point: PointKind,
    /// The observer's report sequence number; it identifies a sample across a recorder relaunch. Zero
    /// (not serialized) for samples that did not come from the observer.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub rseq: u64,
    /// The document instance whose observer took the sample; `rseq` counts within it. Zero (not
    /// serialized) for samples that did not come from an observer.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub document: u64,
    /// The popup whose observer took it (`popup-1`); absent for the launch page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RulesInstalledRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<RequestId>,
    /// Installation order within the generation, starting at zero.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub revision: u64,
    pub sha256: String,
    pub count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaintSource {
    PassthroughContent,
    ArbitraryEvaluate,
    RecorderFault,
    NewWindow,
    UnseededRandomness,
    /// A request issued by the page whose interception did not arrive in time; its record is late.
    UninterceptedRequest,
    /// The engine held a fetch after input until its next frame, which a paused clock never brings
    /// (S15); the platform let virtual time reach that frame. Earlier observations can move the instant.
    FrameStarvation,
    /// An input action failed part way and the platform released what it held (a pressed button);
    /// the page saw a release it was not asked for.
    InputAborted,
    /// A response that did not come through the recorder's proxy: the transport boundary failed.
    TransportBypass,
    /// A `replay` rule had no recorded response for a request, which it failed.
    CassetteMiss,
    /// A stream (Server-Sent Events, WebSocket) matched a rule that cannot answer it (a `mock` or
    /// `sequence`, a `replay` without stream sessions, a pass-through without a connection to servers); it
    /// failed.
    UnsupportedStream,
    /// A platform event delivered after its instant: the engine had let virtual time pass it before the
    /// platform could act: on the baseline engine, which lacks engine patch 1, a stream's handshake during
    /// a document load or under a held request; on either engine, a response delivered after its due
    /// instant.
    LateDelivery,
    /// A frame stopped waiting for a dedicated worker that kept working at its instant: what that worker
    /// posted at the instant came after the frame, as the worker's speed decided. Only earlier engines
    /// cut that wait off (after about 2 s of wall time); the current engine patch 12 waits for the worker.
    WorkersCutOff,
    /// The generation runs the Chrome for Testing baseline, not frontcap's patched engine: the
    /// guarantees the engine patches give do not hold (the detail names them).
    EngineBaseline,
    /// Recordings from before popups held their dialogs: a dialog a `hold` rule matched in a popup was
    /// answered as if no rule had matched. No longer produced.
    DialogHoldUnsupported,
    /// The page's renderer crashed: what it did after the last recorded event was not observed, and the
    /// generation could not continue.
    RendererCrashed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaintRecord {
    pub source: TaintSource,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControllerChange {
    Attached,
    Detached,
    LeaseAcquired,
    LeaseReleased,
    LeaseExpired,
    /// The controller let go for a process image of the browser and took it again.
    Imaged,
    /// The controller took a browser restored from a process image (this trace is its source's so far).
    Restored,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerRecord {
    pub change: ControllerChange,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AcquisitionEvent {
    pub aseq: u64,
    pub after_seq: u64,
    pub t_us: VirtualTime,
    #[serde(flatten)]
    pub kind: AcquisitionKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum AcquisitionKind {
    Screenshot(ScreenshotRecord),
    Observation(ObservationRecord),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotRecord {
    pub path: String,
    pub sha256: String,
    pub source_frame: u64,
    pub frame_t_us: VirtualTime,
    pub document: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationRecord {
    pub call: String,
    pub summary: String,
}

#[cfg(test)]
#[path = "event_tests.rs"]
mod tests;
