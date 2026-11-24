use crate::encode::{base64, origin_of};
use crate::instrument::{self, STOP_URL, WORLD};
use crate::network::{Delivery, Due, Held, Network, Stream, StreamDue};
use crate::session::Session;
use crate::ChromiumError;
use crate::input::{InputPlan, InputStep};
use frontcap_core::action::Action as ActAction;
use cdp::protocol::{accessibility, browser, debugger, dom, emulation, fetch, input, network as cnet, page, runtime, target};
use frontcap_core::bundle::{Bundle, Determinism, Finalization, Level, Manifest};
use frontcap_core::clock::{frame_slot, next_stop, Quiescence, SettleOutcome, StopBounds, WaitOutcome};
use frontcap_core::event::*;
use frontcap_core::ids::{LaunchId, RequestId, RuleId, RunId, WatchName};
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::{Locator, Predicate, WatchRegistry};
use frontcap_core::profile::DeterminismProfile;
use frontcap_core::process::ProcessIdentity;
use frontcap_core::rules::{Action, FailReason, MockSpec, RequestView, RuleSet, SequenceCursors};
use frontcap_core::time::{VirtualDuration, VirtualTime};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The durable arm point of a next-event wait. Native surface identity is only used for equality;
/// it is not included in the behavior projection or reconstructed from screenshots.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EventArm {
    pub after_seq: u64,
    presentation: Option<presentation::PresentedFrame>,
    /// For a response wait: the first response recorded while the world was stopped (an agent's
    /// answer, or a mock answered during input) and not yet delivered to the page by any progress.
    /// The wait's progress delivers it, so it is a trigger although its record precedes `after_seq`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    delivers_from: Option<u64>,
}

struct EventStop {
    event: ProgressEvent,
    after_seq: u64,
    triggers: Vec<ProgressTrigger>,
}

pub struct LaunchOptions {
    pub repo_root: PathBuf,
    pub runs_root: PathBuf,
    pub run: RunId,
    pub url: String,
    pub profile: DeterminismProfile,
    pub rules: RuleSet,
    pub passthrough_origins: Option<Vec<String>>,
    pub watchdog: Duration,
    /// `Some(dir)` starts the browser detached, with its profile in `dir`, so it outlives this process.
    pub browser_profile: Option<PathBuf>,
    /// The generation's launch id; generated when `None`.
    pub launch_id: Option<LaunchId>,
    /// Called with a detached browser's identity as soon as it runs, so the caller can record it before
    /// anything else can fail.
    pub on_browser: Option<BrowserStarted>,
    /// The proxy every connection goes through; `None` connects directly (adapter tests only).
    pub proxy: Option<ProxyRoute>,
    /// A cell the browser starts in (Linux), created by the caller, which has bound the proxy's listener
    /// inside it; `browser_profile` is then not used.
    pub cell: Option<browser_pin::cell::Cell>,
    /// The platform's connections to stream servers, for passed-through streams; `None` fails them.
    pub upstream: Option<frontcap_proxy::upstream::Upstream>,
}

pub type BrowserStarted = Box<dyn FnOnce(&ProcessIdentity) + Send>;

/// Kills a detached browser by identity unless disarmed: a launch that fails after starting it.
struct KillUnlessDisarmed(Option<ProcessIdentity>);

impl Drop for KillUnlessDisarmed {
    fn drop(&mut self) {
        if let Some(identity) = self.0.take() {
            let _ = identity.kill();
        }
    }
}

/// Where a live generation's page can be reached by a new connection.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Endpoint {
    pub browser_ws_url: String,
    pub target_id: String,
    pub browser_pid: u32,
    /// The proxy the browser was started with: every connection goes through it.
    #[serde(default)]
    pub proxy: Option<ProxyRoute>,
    /// The cell the browser runs in, reached through its network namespace; `browser_pid` is its init.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cell: Option<browser_pin::cell::Cell>,
}

/// Opens the DevTools connection to a browser: on this host's loopback, or a cell's.
pub(crate) async fn connect_browser(ws_url: &str, cell: Option<&browser_pin::cell::Cell>) -> Result<cdp::Client, ChromiumError> {
    let client = match cell {
        None => cdp::Client::connect(ws_url).await?,
        Some(cell) => {
            let port = ws_url.strip_prefix("ws://127.0.0.1:").and_then(|rest| rest.split('/').next()).and_then(|p| p.parse::<u16>().ok())
                .ok_or_else(|| ChromiumError::Shape(format!("a cell's DevTools URL on its loopback: {ws_url}")))?;
            let cell = cell.clone();
            let stream = tokio::task::spawn_blocking(move || cell.connect(port)).await.map_err(|e| ChromiumError::Shape(e.to_string()))??;
            stream.set_nonblocking(true)?;
            cdp::Client::connect_over(tokio::net::TcpStream::from_std(stream)?, ws_url).await?
        }
    };
    // Interceptions the browser reports late (`FRONTCAP_INTERCEPTION_DELAY=<request URL part>@<ms>`).
    #[cfg(feature = "fault-injection")]
    if let Some((part, ms)) = std::env::var("FRONTCAP_INTERCEPTION_DELAY").ok()
        .and_then(|v| v.rsplit_once('@').and_then(|(part, ms)| Some((part.to_string(), ms.parse().ok()?))))
    {
        client.delay_events("Fetch.requestPaused", &part, Duration::from_millis(ms));
    }
    Ok(client)
}

/// The recorder's proxy as the browser is told of it (network-rules.md, Interception and transport).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProxyRoute {
    pub addr: std::net::SocketAddr,
    /// Base64 SHA-256 of the key of every certificate the proxy presents.
    pub spki: String,
}

impl ProxyRoute {
    fn args(&self) -> Vec<String> {
        vec![
            format!("--proxy-server=http://{}", self.addr),
            // Without this Chromium connects to loopback hosts directly (S6).
            "--proxy-bypass-list=<-loopback>".into(),
            format!("--ignore-certificate-errors-spki-list={}", self.spki),
            "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".into(),
        ]
    }
}

pub struct ReconnectOptions {
    pub repo_root: PathBuf,
    pub runs_root: PathBuf,
    pub run: RunId,
    pub launch_id: LaunchId,
    pub profile: DeterminismProfile,
    pub endpoint: Endpoint,
    pub watches: WatchRegistry,
    pub watchdog: Duration,
    /// How far virtual time was allowed to run when the previous connection died (the goal of the
    /// clock request in flight); `None` when no request was granting time, so the trace's last instant.
    pub granted_until: Option<VirtualTime>,
    /// The dialog rules in force when the previous connection died.
    pub dialog_rules: frontcap_core::dialog::DialogRules,
    pub upstream: Option<frontcap_proxy::upstream::Upstream>,
}

pub struct Reconnected {
    /// Ordinals of requests that had no response when the previous connection died.
    pub affected: Vec<String>,
    pub replayed_samples: usize,
    /// Set when virtual time ran past what was granted while no connection owned it (S13: an `advance`
    /// budget is dropped with its session): (granted, reached).
    pub ran_past: Option<(VirtualTime, VirtualTime)>,
}

/// Engine patch 6: a frame tick rendered nothing, and the page could not ask for a frame before `wake`.
#[derive(Clone, Copy)]
struct FramesIdle {
    /// `Generation::recorded` before the stop that rendered nothing: anything recorded since (that stop's
    /// own deliveries included) may have asked for a frame.
    recorded: u64,
    documents: u64,
    wake: Option<VirtualTime>,
}

enum BrowserHandle {
    Owned(browser_pin::Launched),
    Detached(ProcessIdentity),
}

impl BrowserHandle {
    fn pid(&self) -> u32 {
        match self {
            BrowserHandle::Owned(l) => l.child.id(),
            BrowserHandle::Detached(id) => id.pid,
        }
    }
}

/// An answer to a held request.
enum Answer<'a> {
    Fulfill(&'a MockSpec, ResponseSource),
    Fail(FailReason),
}

/// The engine refused the command, so it had no effect; any other failure leaves its effect unknown.
fn is_rejection(e: &ChromiumError) -> bool {
    matches!(e, ChromiumError::Cdp(cdp::CdpError::Protocol { .. }))
}

/// The engine no longer knows the interception a `Fetch` command answered: the request is gone.
fn is_gone(e: &ChromiumError) -> bool {
    matches!(e, ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. }) if message.contains("Invalid InterceptionId"))
}

/// A rendered frame from the screencast, as delivered; the state it shows follows from when it arrived.
#[derive(Clone)]
struct RawFrame {
    number: u64,
    /// The PNG, base64 as delivered; decoded only when stored.
    data: String,
    /// Wall time it arrived at, seconds since the Unix epoch.
    arrived: f64,
}

/// A document replacement the platform performs while frozen.
enum Navigation {
    To(String),
    Entry(i64),
    Reload,
}

/// Where a running settle stands: the instant it ends by and the start of its current quiet window.
#[derive(Debug, Clone, Copy)]
pub struct SettleProgress {
    pub goal: VirtualTime,
    pub quiet_since: VirtualTime,
}

/// The bundle member holding the interception binding journal.
const INTERCEPTIONS: &str = "interceptions.jsonl";
/// Wall time without any event after which a `pauseIfNetworkFetchesPending` budget is checked for a fetch
/// the engine holds until a frame (S15).
const STALL_WAIT: Duration = Duration::from_millis(250);
/// How long without a frame counts as the screencast having rendered everything committed.
const FRAME_QUIET: Duration = Duration::from_millis(60);
/// The longest a capture waits for the screencast to fall quiet.
const FRAME_QUIET_MAX: Duration = Duration::from_millis(1000);
/// How long a screenshot waits for the frame of a state that changed after the last frame.
const FRAME_BEHIND_WAIT: Duration = Duration::from_millis(300);
/// How long an operation's end waits for interceptions of requests already seen issued.
const INTERCEPTION_WAIT: Duration = Duration::from_secs(2);
/// Longest `advance` segment while a request is held: requests issued during a segment are handled at its
/// end, a fixed virtual instant, so their handling time does not depend on wall-clock timing.
const HOLD_QUANTUM: VirtualDuration = VirtualDuration(16_000);
/// The response header that names a waited-for response's arrival instant, in microseconds since the
/// Unix epoch; the engine applies and removes it on a request that blocks the main thread (patch 53).
const ARRIVAL_HEADER: &str = "x-frontcap-arrival-us";

struct Subs {
    binding: cdp::Subscription<runtime::BindingCalledEvent>,
    context: cdp::Subscription<runtime::ExecutionContextCreatedEvent>,
    context_destroyed: cdp::Subscription<runtime::ExecutionContextDestroyedEvent>,
    contexts_cleared: cdp::Subscription<runtime::ExecutionContextsClearedEvent>,
    console: cdp::Subscription<runtime::ConsoleApiCalledEvent>,
    /// The browser's own console messages (`Log`), every context's.
    log: cdp::Subscription<cdp::protocol::log::EntryAddedEvent>,
    navigated: cdp::Subscription<page::FrameNavigatedEvent>,
    /// Every context's interceptions, with the session each came on.
    paused: contexts::Routed<fetch::RequestPausedEvent>,
    /// Native pauses seen by pre-renderer recovery that normal enrollment must reacquire.
    recovery_inventory: Vec<(String, String)>,
    expired: cdp::Subscription<emulation::VirtualTimeBudgetExpiredEvent>,
    sent: contexts::Routed<cnet::RequestWillBeSentEvent>,
    /// Child frames' navigations: requested, and ended without an interception (`Interceptions`).
    requested: cdp::Subscription<page::FrameRequestedNavigationEvent>,
    frame_navigated: cdp::Subscription<page::FrameNavigatedEvent>,
    detached: cdp::Subscription<page::FrameDetachedEvent>,
    response: cdp::Subscription<cnet::ResponseReceivedEvent>,
    finished: cdp::Subscription<cnet::LoadingFinishedEvent>,
    failed: cdp::Subscription<cnet::LoadingFailedEvent>,
    exceptions: cdp::Subscription<runtime::ExceptionThrownEvent>,
    attached: cdp::Subscription<target::AttachedToTargetEvent>,
    within: cdp::Subscription<page::NavigatedWithinDocumentEvent>,
    frames: cdp::Subscription<page::ScreencastFrameEvent>,
    chooser: cdp::Subscription<page::FileChooserOpenedEvent>,
    /// The sessions of the dedicated workers the page started (`join_workers_to_virtual_time`).
    workers: Workers,
    /// The held request the page waits for, and the agent's answers to it (engine patch 53).
    holds: BlockingHolds,
    answers: tokio::sync::mpsc::UnboundedReceiver<blocking_holds::Answering>,
}

type Workers = std::sync::Arc<std::sync::Mutex<WorkerSessions>>;

/// The dedicated workers' sessions, which one issued each of their requests in flight (by network id), and
/// which worker started each nested one.
#[derive(Default)]
struct WorkerSessions {
    /// Retained constructor evidence is separate from live session routing.
    lineages: worker_lineage::WorkerLineages,
    sessions: BTreeMap<String, WorkerSession>,
    failure: Option<WorkerEnrollmentFailure>,
    changed: std::sync::Arc<tokio::sync::Notify>,
    requests: BTreeMap<String, String>,
    parents: BTreeMap<String, String>,
}

struct WorkerSession {
    // Dropping this sender retires the session's event bookkeeping task.
    _stop: tokio::sync::oneshot::Sender<()>,
    target: String,
    enrollment: WorkerEnrollment,
    synchronous_wait: Option<(String, cdp::Arrival)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerEnrollment { Joining, Ready, Failed }

#[derive(Clone)]
struct WorkerEnrollmentFailure {
    target: String,
    method: &'static str,
    detail: String,
}

impl WorkerSessions {
    /// Forgets a detached worker and the workers it started (their detach may come after its own, on a
    /// session already gone); returns their sessions.
    fn forget(&mut self, session: &str) -> Vec<String> {
        let mut gone = vec![session.to_string()];
        let mut i = 0;
        while i < gone.len() {
            let parent = gone[i].clone();
            gone.extend(self.parents.iter().filter(|(_, p)| **p == parent).map(|(c, _)| c.clone()));
            i += 1;
        }
        for s in &gone {
            self.sessions.remove(s);
            self.parents.remove(s);
            self.requests.retain(|_, by| by != s);
        }
        gone
    }
}

/// Where a worker session's events go: the page's own subscriptions, in the connection's order.
#[derive(Clone)]
struct NetworkFeeds {
    worker_changes: std::sync::Arc<tokio::sync::Notify>,
    attached: cdp::Feed<target::AttachedToTargetEvent>,
    sent: cdp::Feed<cnet::RequestWillBeSentEvent>,
    response: cdp::Feed<cnet::ResponseReceivedEvent>,
    finished: cdp::Feed<cnet::LoadingFinishedEvent>,
    failed: cdp::Feed<cnet::LoadingFailedEvent>,
}

enum RunEnd {
    Reached,
    Stopped,
    /// A cancellation's instant came before the goal or with it.
    Cancelled,
}

/// What `handle_request` does next with its queue.
enum Next {
    Handle(usize),
    /// The earliest queued request was issued at this place; one issued before it is still to come.
    WaitFor(cdp::Arrival),
    /// Only requests whose `requestWillBeSent` has not arrived; the first of them.
    Unannounced(usize),
}

/// An interception's place in the page's issue order.
enum Place {
    Now,
    At(cdp::Arrival),
    Unannounced,
}

enum Wait {
    Expired,
    Request(Box<fetch::RequestPausedEvent>),
    /// The agent answered the held request the page waits for.
    Answer(blocking_holds::Answering),
    Watchdog,
    /// Nothing happened for `STALL_WAIT` of wall time.
    Quiet,
}

mod actions;
mod adoption;
mod blocking_holds;
pub use blocking_holds::{BlockingAnswer, BlockingHold, BlockingHolds};
mod capture;
mod contexts;
pub use contexts::PAGE;
mod events;
mod exploration;
mod interception;
mod intervention;
mod cookies;
mod buckets;
mod cache;
mod indexed_db;
mod image;
mod lifecycle;
mod worker_lineage;
mod request_collection;
mod observation;
mod native_dialogs;
pub use native_dialogs::{DialogRecovery, DialogSession};
mod pointer;
mod presentation;
mod progress;
mod shared_time;
mod streams;
mod rule_updates;
pub use intervention::{StatePlan, validate_initial_storage};
pub use cookies::validate_initial_cookies;
pub use cache::validate_initial_caches;
pub use indexed_db::validate_initial_databases;

/// A profile's locale is a language tag (letters, digits and `-`): it becomes the browser's language
/// argument.
pub fn validate_locale(locale: &str) -> Result<(), ChromiumError> {
    if locale.is_empty() || locale.len() > 35 || !locale.starts_with(|c: char| c.is_ascii_alphabetic())
        || !locale.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(ChromiumError::InvalidArgument(format!("locale {locale:?} is not a language tag such as en-US")));
    }
    Ok(())
}

enum PendingEvent {
    Recorded(Box<EventKind>),
    /// Ordering evidence only: cannot be committed until a native receipt supplies the answer.
    NativeDialogOpening(DialogRecord),
}

impl PendingEvent {
    fn recorded(&self) -> Option<&EventKind> {
        match self { Self::Recorded(event) => Some(event), Self::NativeDialogOpening(_) => None }
    }
}

pub struct Generation {
    event_stop: Option<EventStop>,
    held_pointer: Option<frontcap_core::pointer::PointerHold>,
    /// Keys `key_down` holds, in the order they were pressed.
    held_keys: Vec<String>,
    /// The popup held keys were pressed in; none for the launch page.
    keys_context: Option<String>,
    /// Whether the last composition operation left one in progress. The page can end it without one
    /// (a focus change commits it), so this is what cleanup checks, not the composition itself.
    composing: bool,
    /// The popup a composition is in; none for the launch page.
    compose_context: Option<String>,
    browser: BrowserHandle,
    browser_ws_url: String,
    /// The cell the browser runs in, if any.
    cell: Option<browser_pin::cell::Cell>,
    target_id: String,
    page: Session,
    profile: DeterminismProfile,
    subs: Subs,
    /// The first observer report after a request's issue announcement, deferred until that request
    /// has been recorded. Both subscriptions carry the connection's shared arrival sequence.
    deferred_binding: Option<(cdp::Arrival, runtime::BindingCalledEvent)>,
    world: Option<runtime::ExecutionContextId>,
    world_unique: Option<String>,
    /// The top document's main world, where the stream wrapper lives.
    main_context: Option<runtime::ExecutionContextId>,
    main_context_unique: Option<String>,
    /// Current main-world context per frame, addressed by browser-unique identity.
    stream_contexts: BTreeMap<String, String>,
    /// Observer context epochs bind buffered stream reports to their original document owner.
    stream_observers: BTreeMap<i64, Vec<(cdp::Arrival, String)>>,
    main_frame: page::FrameId,
    now: VirtualTime,
    log: BehaviorLog,
    /// The trace as written so far, for reads between appends.
    trace_tail: std::sync::Arc<std::sync::Mutex<frontcap_core::log::BehaviorTail>>,
    /// What the controller looked at; never read back into behavior.
    acquisition: frontcap_core::log::AcquisitionLog,
    /// Events of the current step, appended in virtual-time order by `commit`.
    pending: Vec<(VirtualTime, PendingEvent)>,
    /// An input action being dispatched: its first instant and how many buffered events precede it. Its
    /// effects stay buffered until its action is recorded ahead of them (`input_finish`).
    acting: Option<(VirtualTime, usize)>,
    bundle: Bundle,
    /// Native pauses bound to their requests (`interceptions.jsonl`), synced before each release or answer.
    bindings: crate::bindings::BindingJournal,
    /// Engine receipts (patch 24) of answers whose response records are not written yet: (ordinal,
    /// native pause id). Once written, the receipt is no longer needed and moves to `receipts_ready`.
    receipts_awaiting: Vec<(String, String)>,
    /// Native pause ids whose receipts the engine can forget, at the next drain.
    receipts_ready: Vec<String>,
    /// Set while a reconnect adopts retained requests: a recorded request continued then is recorded from
    /// its paused response, since the new session's Network domain never saw it issued and keeps no body.
    adopting: bool,
    run: RunId,
    launch_id: LaunchId,
    engine: String,
    engine_revision: String,
    /// The generation runs frontcap's patched engine (its commands are available).
    patched: bool,
    shared_time: Option<shared_time::SharedTime>,
    /// The last frame tick a frame was rendered at.
    last_frame: u64,
    /// Controller-local evidence connecting recorded changes to a native presentation.
    /// The native engine owns frame identity and paint attribution across controller loss.
    presented: Option<presentation::Presentation>,
    /// Engine patch 6: surface copies taken so far (screenshots and recorded frames), numbering them.
    surface_copies: u64,
    /// Engine patch 6: while recording, presented renders whose copy failed (a recording never stops the run).
    recording_missed: Vec<VirtualTime>,
    /// Every event recorded so far (`record`): what may have changed the page.
    recorded: u64,
    /// Potential state changes, independent of observation/acquisition sequence numbers.
    state_revision: u64,
    /// After a frame tick that rendered nothing: no tick needs rendering before this instant (the page's
    /// next task; `None` inside: none pending), as long as nothing else is recorded meanwhile and the
    /// document is the same.
    frames_idle: Option<FramesIdle>,
    /// An accepted frame retains its native command and remaining windows across a progress stop.
    pending_render: Option<capture::PendingRender>,
    pending_frame: Option<capture::PendingFrame>,
    frame_stop: Option<RunEnd>,
    frame_watch: Option<WatchName>,
    /// Native frame requests made by this adapter instance (verification only).
    #[cfg(feature = "test-hooks")]
    frame_requests: u64,
    #[cfg(feature = "test-hooks")]
    after_round_selection: Option<String>,
    #[cfg(feature = "test-hooks")]
    interrupt_synchronous_binding: Option<String>,
    net: Network,
    watches: WatchRegistry,
    /// New-document scripts that register each watch in every future document.
    watch_scripts: BTreeMap<WatchName, page::ScriptIdentifier>,
    stop_script: Option<page::ScriptIdentifier>,
    /// The popup the condition stop is armed in; none for the launch page.
    stop_context: Option<String>,
    last_samples: BTreeMap<WatchName, SampleValue>,
    quiet: Option<Quiescence>,
    progressing: bool,
    /// Set from another task (`cancel_handle`) to cancel the running wait.
    cancel_requested: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The virtual microseconds a running wait has reached, read from another task (`progress_handle`).
    progress: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Where a cancelled wait stops: the frame tick after the run saw the request, or the instant a
    /// replay armed.
    cancel_at: Option<VirtualTime>,
    /// Whether the running progress is a wait the controller may cancel (an act's own moves are not).
    cancellable: bool,
    /// Where the last wait stopped by cancellation, until taken.
    cancelled_at: Option<VirtualTime>,
    /// A request the page's main thread waits for is being answered (engine patch 53).
    blocking: bool,
    /// Decisions allocated before a synchronous request; delivery waits for a page reply.
    deferred_requests: Vec<(fetch::RequestPausedEvent, crate::bindings::Binding)>,
    /// Completion bookkeeping whose response-body read must wait for the main thread.
    deferred_network_completions: Vec<(String, bool)>,
    collecting_requests: bool,
    /// The collected round of same-instant requests being allocated (`next_in_order`).
    request_round: Option<request_collection::RequestRound>,
    /// The clock moved to a waited-for response's arrival; the grant record follows once the page runs.
    arrived: bool,
    stop_hit: Option<WatchName>,
    documents: u64,
    /// Network id and issue time of the latest main-frame document request, which stamps its navigation.
    document_request: Option<(String, VirtualTime)>,
    /// `navigate` events reported by the observer, as (URL without fragment, virtual time).
    navigates: Vec<(String, VirtualTime)>,
    taints: Vec<TaintRecord>,
    watchdog: Duration,
    /// Set once the watchdog fired: the page is stopped in the debugger and no longer controllable.
    dead: bool,
    /// Set once the renderer crashed (`watch_crashes`): its state is gone and nothing is answered.
    crashed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Stream receipts already queued or restored from the trace, and those safe to forget.
    seen_stream_receipts: std::collections::BTreeSet<String>,
    stream_receipts_ready: std::collections::BTreeSet<String>,
    /// Highest observer report sequence number recorded for the current document's observer.
    max_rseq: u64,
    /// Highest of those already written to the trace: the observer may drop only these.
    written_rseq: u64,
    /// The document instance the current observer world belongs to; samples carry it.
    report_epoch: u64,
    /// Restored for the first observer world a reconnected generation finds (the current document's).
    resume_cursor: Option<(u64, BTreeMap<WatchName, SampleValue>)>,
    /// Navigations within the current document seen so far; one ends a same-document history traversal.
    within_document: u64,
    /// The last frame the screencast delivered, and the frames kept while recording.
    frame: Option<RawFrame>,
    frames_seen: u64,
    /// State-changing events recorded so far (actions, navigations, responses, dialogs, scripts).
    changes: u64,
    /// When each state began, for labelling frames by when they arrived.
    timeline: crate::frames::Timeline,
    /// While recording: frames whose state is not settled yet, and the frames written so far.
    recording: Option<(Vec<RawFrame>, Vec<frontcap_core::observation::RecordedFrame>)>,
    /// Set by the host that records re-execution inputs (`set_missing_inputs`).
    missing_inputs: Option<Vec<String>>,
    /// Answers to held moves sent without waiting (S17), checked before the drag goes on.
    drag_answers: Vec<cdp::Answer>,
    /// Interceptions waiting for a request issued before them (`handle_request`), with when each arrived.
    queued: Vec<(Instant, fetch::RequestPausedEvent)>,
    proxy: Option<ProxyRoute>,
    upstream: Option<frontcap_proxy::upstream::Upstream>,
    /// Passed-through streams, by handshake ordinal: what their servers sent waits for a slot.
    pass: BTreeMap<String, PassStream>,
    /// The last slot (frame-interval multiple) delivered at.
    last_slot: u64,
    /// The caller's rules as installed (a replayed cassette names the bundle's copy).
    caller_rules: RuleSet,

    /// Set when a platform-answered fetch is held by the engine until a frame (S15): virtual time is
    /// granted in frame intervals under `advance` until it finishes.
    starving: bool,
    /// Set while the platform's own navigation runs; its document request is issued now, while frozen.
    own_navigation: bool,
    /// The agent's request whose navigation runs, named in its top-frame document request's binding.
    navigation_request: Option<String>,
    /// The loader of the launch page's last commit taken: a recovery that takes a commit from the frame
    /// tree must not count its later `frameNavigated` again.
    committed_loader: Option<String>,
    /// DOM mutation records of earlier documents, and the current document's observer total.
    dom_base: u64,
    dom_doc: u64,
    /// Requests whose click is already recorded: finishing one again never records it twice.
    recorded_operations: std::collections::BTreeSet<RequestId>,
    interception_stops: BTreeMap<String, (fetch::RequestPausedEvent, crate::bindings::Binding)>,
    released_interceptions: std::collections::BTreeSet<String>,
    /// Dialogs answered through CDP, recorded at the next drain.
    native_dialogs: NativeDialogs,
    native_dialog_driver: Option<native_dialogs::Driver>,
    dialog_bindings: std::sync::Arc<std::sync::Mutex<native_dialogs::DialogBindings>>,
    recorded_native_dialogs: std::collections::BTreeSet<String>,
    /// Where the active context's mouse was last sent, in viewport CSS pixels; `None` before any mouse input.
    mouse_at: Option<(f64, f64)>,
    /// Every page the browser attaches, in arrival order: the windows opened without an opener that a
    /// stopped boundary admits (`admit_held_windows`).
    window_attaches: Option<cdp::Subscription<target::AttachedToTargetEvent>>,
    /// The window being admitted, whose document is not there to read yet.
    admitting: Option<String>,
    /// The popups (by target) whose document request the generation decided since their last drain: the
    /// next drain reads their navigation once it has committed or ended (`await_popup_navigation`).
    popup_navigations: std::collections::BTreeSet<String>,
    exception_contexts: BTreeMap<i64, Vec<(cdp::Arrival, String)>>,
    recorded_exceptions: std::collections::BTreeSet<NativeException>,
    seen_exception_receipts: std::collections::BTreeSet<String>,
    exception_receipts_ready: std::collections::BTreeSet<String>,
    native_dialog_receipts_ready: std::collections::BTreeSet<String>,
    /// The agent's dialog rules, shared with the task answering native dialogs.
    dialog_rules: std::sync::Arc<std::sync::Mutex<frontcap_core::dialog::DialogRules>>,
    /// dialogs.js as new documents get it, with the rules in force.
    dialogs_script: page::ScriptIdentifier,
    /// Windows the page tried to open that the browser did not create, by URL, for the trace.
    closed_windows: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Virtual time of the latest `navigate` report: a `beforeunload` dialog opens in that same task.
    last_navigate: Option<VirtualTime>,
    obs: ObservationState,
    /// (document, backend node id) → reference.
    ref_of: BTreeMap<(u64, i64), String>,
    /// The windows the generation's pages opened (`popups: controlled`).
    contexts: contexts::Contexts,
    /// The popups' own state while the launch page is active, by context.
    parked: BTreeMap<String, contexts::PageState>,
    /// The popup the fields hold now and the launch page's state meanwhile (`enter_context`).
    entered: Option<(String, contexts::PageState)>,
}

fn ms_epoch_to_virtual(ms: f64, origin_ms: u64) -> VirtualTime {
    VirtualTime::from_engine_ms(ms - origin_ms as f64)
}

fn error_reason(r: FailReason) -> cnet::ErrorReason {
    match r {
        FailReason::ConnectionRefused => cnet::ErrorReason::ConnectionRefused,
        FailReason::TimedOut => cnet::ErrorReason::TimedOut,
        FailReason::Aborted => cnet::ErrorReason::Aborted,
        FailReason::NameNotResolved => cnet::ErrorReason::NameNotResolved,
    }
}

const ACCEPTED: &str = "function(n) { return globalThis.__frontcap.accepted(this, n); }";
const DESCRIBE: &str = "function() { return globalThis.__frontcap.describe(this); }";

pub use frontcap_core::observation::{Diff, Prop, QueryResult, Replaced, Snapshot, SnapshotNode};
/// A browser cell the caller creates for `LaunchOptions::cell` (Linux).
pub use browser_pin::cell::{Cell, CellUser, Image, Restoring};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RefEntry {
    pub document: u64,
    pub backend: i64,
    /// The popup the node is in; none for the launch page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

type NativeDialogs = std::sync::Arc<std::sync::Mutex<Vec<DialogRecord>>>;

/// References and recent snapshots, kept by the recorder across a relaunch.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ObservationState {
    #[serde(default)]
    pub state_revision: u64,
    /// DOM mutation records of documents before the current one.
    #[serde(default)]
    pub dom_base: u64,
    pub refs: BTreeMap<String, RefEntry>,
    pub next_ref: u64,
    pub next_observation: u64,
    #[serde(default)]
    pub next_image: u64,
    #[serde(default)]
    pub images: std::collections::VecDeque<frontcap_core::observation::ImageAnchor>,
    pub snapshots: std::collections::VecDeque<Snapshot>,
    /// The launch page's mouse position, so a relaunched recorder sends no move to where it already is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouse: Option<[f64; 2]>,
}

const KEPT_SNAPSHOTS: usize = 16;
/// A frame whose document the observer cannot reach (on the baseline engine, any of another origin:
/// engine patch 11) shows nodes no locator, watch or receipt covers, so input that would reach one could
/// leave no receipt.
const UNOBSERVED_FRAME: &str = "the observer cannot reach that frame's document (on the baseline engine, a frame of another origin)";

/// The frames a frame scope names: the frame element, and the frame it shows.
struct Scope {
    owner: dom::BackendNodeId,
    frame: page::FrameId,
}

/// One frame's accessibility tree and the backend id of the element that shows it (none for the top frame).
struct FrameTree {
    owner: Option<i64>,
    /// The document the frame shows.
    url: String,
    nodes: Vec<accessibility::AXNode>,
}

fn ax_string(v: &Option<accessibility::AXValue>) -> String {
    v.as_ref().and_then(|v| v.value.as_ref()).map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())).unwrap_or_default()
}

fn ax_input(n: &accessibility::AXNode) -> crate::observe::AxInput {
    const STATES: &[&str] = &["focused", "disabled", "checked", "expanded", "selected", "pressed", "required", "invalid"];
    let states = n
        .properties
        .iter()
        .flatten()
        .filter_map(|p| {
            let name = serde_json::to_value(p.name).ok()?.as_str()?.to_string();
            let on = match p.value.value.as_ref()? {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::String(s) => s == "true" || s == "mixed",
                _ => false,
            };
            (STATES.contains(&name.as_str()) && on).then_some(name)
        })
        .collect();
    let value = ax_string(&n.value);
    crate::observe::AxInput {
        id: n.node_id.0.clone(),
        parent: n.parent_id.as_ref().map(|p| p.0.clone()),
        role: ax_string(&n.role),
        name: ax_string(&n.name),
        value: (!value.is_empty()).then_some(value),
        states,
        backend: n.backend_dom_node_id.as_ref().map(|b| b.0),
        ignored: n.ignored,
    }
}

impl Generation {
    /// The caller's rules as installed, for the re-execution inputs: a replayed cassette names the
    /// bundle's copy.
    pub async fn network_rules(&mut self) -> Result<frontcap_core::rules::RuleSnapshot, ChromiumError> {
        let boundary = self.observation_boundary().await?;
        Ok(frontcap_core::rules::RuleSnapshot { boundary, revision: self.net.rules_revision, rules: self.net.rules.clone() })
    }

    pub fn caller_rules(&self) -> &RuleSet {
        &self.caller_rules
    }

    pub fn frame_interval_us(&self) -> u64 {
        self.profile.frame_interval_us
    }

    /// Records a recovery's account of what an interrupted request lost.
    pub fn record_recovery_taint(&mut self, detail: String) -> Result<(), ChromiumError> {
        self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::RecorderFault, detail }));
        self.commit()
    }

    /// The browser's process id (its renderers and GPU process are its children).
    pub fn browser_pid(&self) -> u32 {
        self.browser.pid()
    }

    pub fn now(&self) -> VirtualTime {
        self.now
    }

    pub fn trace_path(&self) -> PathBuf {
        self.bundle.trace()
    }

    /// Reads the complete events the trace file holds now. Only lines appended since the last read
    /// are parsed.
    fn read_trace<R>(&self, read: impl FnOnce(&[BehaviorEvent]) -> R) -> Result<R, ChromiumError> {
        Ok(read(self.trace_tail.lock().unwrap().events()?))
    }

    /// DOM mutation records reported so far in every context the generation took in, closed popups
    /// included, so a step's difference counts changes in any window (app-state consumer run: a popup's
    /// changes were not counted).
    pub fn dom_mutations(&self) -> u64 {
        self.dom_base + self.dom_doc + self.parked.values().map(contexts::PageState::dom_mutations).sum::<u64>()
    }

    /// The trace's next sequence number.
    pub fn next_seq(&self) -> u64 {
        self.log.next_seq()
    }

    pub fn launch_id(&self) -> &LaunchId {
        &self.launch_id
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint { browser_ws_url: self.browser_ws_url.clone(), target_id: self.target_id.clone(), browser_pid: self.browser.pid(), proxy: self.proxy.clone(), cell: self.cell.clone() }
    }

    pub fn watches(&self) -> &WatchRegistry {
        &self.watches
    }

    /// Records a change of the controlling connection at the current virtual time.
    pub fn record_controller(&mut self, change: ControllerChange) -> Result<(), ChromiumError> {
        self.record(self.now, EventKind::Controller(ControllerRecord { change }));
        self.commit()
    }

    /// Observation helper: evaluates a read-only expression in the page's main world.
    pub async fn eval_main(&self, expr: &str) -> Result<serde_json::Value, ChromiumError> {
        self.alive()?;
        self.page.eval(expr, None).await
    }

    fn worker_enrollment_result(&self) -> Result<(), ChromiumError> {
        let mut books = vec![self.subs.workers.clone()];
        books.extend(self.contexts.lock().unwrap().popups.iter().map(|p| p.workers.clone()));
        for book in books {
            if let Some(failure) = &book.lock().unwrap().failure {
                return Err(ChromiumError::WorkerEnrollment {
                    target: failure.target.clone(), method: failure.method, detail: failure.detail.clone(),
                });
            }
        }
        Ok(())
    }

    fn alive(&self) -> Result<(), ChromiumError> {
        self.contexts.lock().unwrap().check_enrollment()?;
        self.worker_enrollment_result()?;
        if self.crashed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(ChromiumError::RendererCrashed);
        }
        if self.dead {
            return Err(ChromiumError::Unresponsive { goal_us: self.now.0 });
        }
        Ok(())
    }

    /// Buffers an event of the current step; `commit` appends the step's events in time order.
    fn record(&mut self, t: VirtualTime, kind: EventKind) {
        if let EventKind::Stream(stream) = &kind && let Some(receipt) = &stream.receipt {
            self.seen_stream_receipts.insert(receipt.id.clone());
        }
        self.note_record(&kind);
        self.pending.push((t, PendingEvent::Recorded(Box::new(kind))));
    }

    fn note_record(&mut self, kind: &EventKind) {
        self.recorded += 1;
        if exploration::affects_state(kind) {
            self.state_revision = self.state_revision.saturating_add(1);
        }
        if let EventKind::Taint(taint) = &kind {
            self.taints.push(taint.clone());
        }
        // Events of the platform after which a frame may no longer show the page as it is. A passed-through
        // response ends when the real network delivers it, like a timer firing while time moves: its effect
        // belongs to the state the screencast shows next and is tainted `passthrough_content`.
        if matches!(&kind, EventKind::Action(_) | EventKind::Navigation(_) | EventKind::Dialog(_) | EventKind::Focus(_))
            || matches!(&kind, EventKind::Response(r) if r.source != ResponseSource::Passthrough)
            || matches!(&kind, EventKind::Taint(t) if t.source == TaintSource::ArbitraryEvaluate)
        {
            self.changes += 1;
            self.timeline.changed(crate::frames::wall_now(), self.now, self.documents, self.changes);
        }
    }

    /// Appends the buffered events in virtual-time order, up to the issue time of the earliest request
    /// not handled yet: its record is still to come and must not land behind later events.
    fn commit(&mut self) -> Result<(), ChromiumError> {
        let mut horizon = self.net.horizon();
        if let Some(t) = self.pending.iter().filter_map(|(t, event)| matches!(event, PendingEvent::NativeDialogOpening(_)).then_some(*t)).min() {
            if t.0 == 0 { return Ok(()) }
            let before = VirtualTime(t.0 - 1);
            horizon = Some(horizon.map_or(before, |h| h.min(before)));
        }
        // While an input action is dispatched, nothing from its first instant on is written: its action
        // record goes ahead of its effects there.
        if let Some((start, _)) = self.acting {
            let before = VirtualTime(start.0.saturating_sub(1));
            horizon = Some(horizon.map_or(before, |h| h.min(before)));
            if start.0 == 0 {
                return Ok(());
            }
        }
        let (now, later) = crate::network::split_at_horizon(std::mem::take(&mut self.pending), horizon);
        self.pending = later;
        // What was written came from before the action (its effects are at or after its first instant).
        if let Some((_, before)) = self.acting.as_mut() {
            *before = before.saturating_sub(now.len());
        }
        let now: Vec<_> = now.into_iter().map(|(t, event)| match event {
            PendingEvent::Recorded(kind) => (t, *kind),
            PendingEvent::NativeDialogOpening(_) => unreachable!("commit horizon excludes unresolved dialogs"),
        }).collect();
        self.written_rseq = written_cursor(self.written_rseq, &now, self.report_epoch);
        let mut native_written = Vec::new();
        let mut exceptions_written = Vec::new();
        let mut streams_written = Vec::new();
        let mut stream_calls_written = false;
        for (t, kind) in now {
            let seq = self.log.next_seq();
            if let Some(stop) = self.event_stop.as_mut()
                && seq >= stop.after_seq
                && let Some(trigger) = stop.event.trace_trigger(seq, t, &kind)
            {
                stop.triggers.push(trigger);
            }
            let native_id = match &kind {
                EventKind::Dialog(d) => d.native.as_ref().map(|n| n.id.clone()),
                _ => None,
            };
            if let EventKind::Exception(e) = &kind
                && let Some(receipt) = &e.receipt
            {
                exceptions_written.push(receipt.id.clone());
            }
            if let EventKind::Stream(stream) = &kind
                && let Some(receipt) = &stream.receipt
            {
                streams_written.push(receipt.id.clone());
                stream_calls_written |= stream.direction == StreamDirection::Out;
            }
            #[cfg(feature = "fault-injection")]
            let split_close = matches!(&kind, EventKind::Stream(stream)
                if stream.direction == StreamDirection::Out && stream.event == StreamEventKind::Close
                    && stream.receipt.is_some());
            self.log.append(t, kind)?;
            #[cfg(feature = "fault-injection")]
            if split_close && std::env::var("FRONTCAP_STREAM_FAULT").is_ok_and(|point| point == "after_close_request") {
                self.log.sync()?;
                events::stream_fault_point("after_close_request");
            }
            if let Some(id) = native_id { native_written.push(id); }
        }
        if !streams_written.is_empty() {
            // A browser receipt may be forgotten only after the behavior record survives process loss.
            self.log.sync()?;
            if stream_calls_written { events::stream_fault_point("after_trace_sync"); }
            self.stream_receipts_ready.extend(streams_written);
        }
        if !exceptions_written.is_empty() {
            self.log.sync()?;
            events::exception_fault_point("after_trace_sync");
            self.exception_receipts_ready.extend(exceptions_written);
        }
        if !native_written.is_empty() {
            self.log.sync()?;
            self.recorded_native_dialogs.extend(native_written.iter().cloned());
            for ordinal in native_written {
                self.native_dialog_receipts_ready.insert(self.dialog_bindings.lock().unwrap().native_id(&ordinal)?);
            }
        }
        // A receipt is kept until its response record is written: a relaunched recorder reads one or the other.
        let unwritten: std::collections::BTreeSet<&str> = self.pending.iter().filter_map(|(_, k)| match k.recorded() {
            Some(EventKind::Response(r)) => Some(r.id.as_str()),
            _ => None,
        }).collect();
        let (ready, awaiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.receipts_awaiting).into_iter().partition(|(o, _)| !unwritten.contains(o.as_str()));
        self.receipts_awaiting = awaiting;
        self.receipts_ready.extend(ready.into_iter().map(|(_, id)| id));
        Ok(())
    }

    /// Records a source of randomness the profile's seed does not reach, once per source.
    fn unseeded(&mut self, t: VirtualTime, detail: &str) {
        if !self.taints.iter().any(|t| t.source == TaintSource::UnseededRandomness && t.detail == detail) {
            self.record(t, EventKind::Taint(TaintRecord { source: TaintSource::UnseededRandomness, detail: detail.to_string() }));
        }
    }
}

pub const UPLOADS: &str = "inputs/uploads";
/// The largest file an upload copies into the bundle.
const UPLOAD_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A drag's record, at release, also names the instant it began, before its press and any explicitly
/// authorized intervals between moves.
fn drag_args(plan: &InputPlan) -> Option<serde_json::Value> {
    if !matches!(plan.action, ActAction::Drag { .. }) {
        return None;
    }
    let mut args = plan.action.args().unwrap_or_else(|| serde_json::json!({}));
    args["started_us"] = serde_json::json!(plan.start_us.0);
    Some(args)
}

/// Headers as the engine reports them (several of one name joined by a line feed).
fn engine_headers(h: &cnet::Headers) -> BTreeMap<String, String> {
    h.0.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect()
}

/// A passed-through stream: the platform's connection to its server, the next event's number, the
/// instant it opened (its events are recorded from there), and — under a `record` rule — the cassette,
/// key, and session it is kept as.
struct PassStream {
    upstream: frontcap_proxy::upstream::UpstreamStream,
    next_seq: u64,
    issued: VirtualTime,
    record: Option<(String, frontcap_core::cassette::Key, u64)>,
}

/// One event of a stream's script: its instant, kind, data, Server-Sent Events name and id, close code.
type ScriptEvent = (VirtualTime, StreamEventKind, Option<String>, Option<String>, Option<String>, Option<u16>);

/// Whether `frame` is in `tree`.
fn frame_in(tree: &page::FrameTree, frame: &page::FrameId) -> bool {
    tree.frame.id == *frame || tree.child_frames.iter().flatten().any(|child| frame_in(child, frame))
}

/// A stream's handshake request (streams.js): the page's stream id, whether it is a WebSocket, the
/// stream's own URL (the marker removed; `ws(s)` for a WebSocket), and an EventSource's last event id
/// (hex of its UTF-8, in the marker) when it reconnects.
fn stream_anchor(url: &str) -> Option<(u64, bool, String, Option<String>)> {
    let (base, query) = url.split_once('?')?;
    let (query, fragment) = match query.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (query, None),
    };
    let mut marker = None;
    let rest: Vec<&str> = query
        .split('&')
        .filter(|p| match p.strip_prefix("__frontcap_stream=") {
            Some(v) => {
                marker = Some(v.to_string());
                false
            }
            None => true,
        })
        .collect();
    let marker = marker?;
    let mut parts = marker.split('.');
    let (id, kind) = (parts.next()?.to_string(), parts.next()?.to_string());
    let last_event_id = match parts.next() {
        Some(hex) if !hex.is_empty() => {
            let bytes: Option<Vec<u8>> = (0..hex.len()).step_by(2).map(|i| hex.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok())).collect();
            Some(String::from_utf8(bytes?).ok()?)
        }
        _ => None,
    };
    let websocket = kind == "ws";
    let mut url = base.to_string();
    if websocket {
        url = url.replacen("https://", "wss://", 1).replacen("http://", "ws://", 1);
    }
    if !rest.is_empty() {
        url = format!("{url}?{}", rest.join("&"));
    }
    if let Some(f) = fragment {
        url = format!("{url}#{f}");
    }
    Some((id.parse().ok()?, websocket, url, last_event_id))
}

/// A request's body as the platform keys it: `None` when the engine has one it does not show (a large or
/// file body), `Some(None)` when there is none.
fn request_body(r: &cnet::Request) -> Option<Option<Vec<u8>>> {
    if let Some(entries) = &r.post_data_entries {
        let mut bytes = Vec::new();
        for e in entries {
            bytes.extend(frontcap_core::encode::unbase64(&e.bytes.as_ref()?.0)?);
        }
        return Some(Some(bytes));
    }
    match (&r.post_data, r.has_post_data) {
        (Some(text), _) => Some(Some(text.as_bytes().to_vec())),
        (None, Some(true)) => None,
        (None, _) => Some(None),
    }
}

/// The header naming a passed-through request's ordinal for the proxy (`frontcap_proxy::ORDINAL_HEADER`).
fn frontcap_proxy_header() -> &'static str {
    "x-frontcap-request"
}

/// What the trace records of an upload: each file's name, size, and hash — never a path, which differs
/// between runs.
fn upload_args(plan: &InputPlan) -> Option<serde_json::Value> {
    plan.steps.iter().find_map(|s| match s {
        InputStep::SetFiles { files } | InputStep::ChooseFiles { files } => Some(serde_json::json!({ "files": files.iter().map(|f| serde_json::json!({"name": f.name, "size": f.size, "sha256": f.sha256})).collect::<Vec<_>>() })),
        _ => None,
    })
}

/// The observer cursor after writing `events`: the highest durable report sequence of document `epoch`.
fn written_cursor(prev: u64, events: &[(VirtualTime, EventKind)], epoch: u64) -> u64 {
    events
        .iter()
        .filter_map(|(_, k)| match k {
            EventKind::Navigation(n) if n.document == epoch => Some(n.rseq),
            EventKind::Watch(w) if w.document == epoch => Some(w.rseq),
            EventKind::Focus(f) if f.document == epoch => Some(f.rseq),
            EventKind::Message(m) if m.document == epoch => Some(m.rseq),
            EventKind::Dialog(d) => d.observer.as_ref().filter(|o| o.document == epoch).map(|o| o.rseq),
            _ => None,
        })
        .fold(prev, u64::max)
}

#[cfg(test)]
#[path = "generation_tests.rs"]
mod tests;
