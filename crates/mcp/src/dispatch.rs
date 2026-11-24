//! Tool implementations over recorder connections. The server keeps no state that affects a target:
//! idle lease connections tie leases to this process's lifetime; oversized readbacks live on disk.
use crate::config::Config;
use crate::connections::{Connections, from_reply};
use crate::server::{CallToolResult, ToolDescription, ToolFuture, Tools};
use crate::tools::*;
use frontcap_core::clock::SettleOutcome;
use frontcap_core::event::{ClockOutcome, SampleValue};
use frontcap_core::ids::{LaunchId, RequestId, WatchName};
use frontcap_core::profile::{DeterminismProfile, Viewport};
use frontcap_core::time::VirtualTime;
use frontcap_recorder::launcher::{self, LaunchRequest, Registered};
use frontcap_recorder::runtime::{self, RuntimeState};
use frontcap_recorder_protocol::{Call, Effects, HistorySample, LeaseToken, Op, Outcome, Reply, StatusReport};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;
use std::path::PathBuf;

pub struct Frontcap {
    pub config: Config,
    pub connections: Connections,
}

type ToolResult<T> = Result<T, ToolError>;

#[derive(Debug)]
pub(crate) enum Mutation {
    Completed(Outcome),
    Suspended(Box<SuspendedResult>),
}

/// A successful transport response for an unfinished operation, never a terminal mutation result.
#[derive(Debug, Serialize)]
pub(crate) struct SuspendedResult {
    state: &'static str,
    launch_id: LaunchId,
    operation_id: RequestId,
    clock: ClockState,
    /// The dialog the operation waits for an answer to, or else the held request its page waits for.
    #[serde(skip_serializing_if = "Option::is_none")]
    dialog: Option<frontcap_core::dialog::DialogDescriptor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    request: Option<Box<frontcap_recorder_protocol::BlockingRequest>>,
    next_action: &'static str,
}

// Value-returning handlers preserve their completed payload shape and return suspension before
// fetching effects or reporting success. Image-bearing capture results handle the same enum directly.
macro_rules! completed {
    ($mutation:expr) => {
        match $mutation {
            Mutation::Completed(outcome) => outcome,
            Mutation::Suspended(result) => return Ok(serde_json::to_value(result).expect("serializes")),
        }
    };
}

fn decode<T: DeserializeOwned>(tool: &str, args: &Option<Box<RawValue>>) -> Result<T, CallToolResult> {
    let text = args.as_ref().map_or("{}", |a| a.get());
    read_arguments(text).map_err(|e| invalid_arguments(tool, &e))
}

/// Room for server role prefixes in derived ids (launch settle and stop release), so
/// distinct ids never derive the same one.
const MAX_AGENT_ID: usize = 100;

fn parse_lease(text: &str) -> ToolResult<LeaseToken> {
    LeaseToken::new(text).map_err(|e| ToolError::new("invalid_arguments", e.to_string(), "send the lease exactly as launch or reattach returned it"))
}

/// The id a state-changing call runs under: the agent's, or a new one. A retry after a lost answer is
/// answered from the journal only when it repeats an id the agent chose.
pub(crate) fn request_id(id: Option<RequestId>) -> ToolResult<RequestId> {
    match id {
        Some(id) if id.as_str().len() > MAX_AGENT_ID => Err(ToolError::new(
            "invalid_arguments",
            format!("request ids are at most {MAX_AGENT_ID} characters"),
            "send a shorter request_id, or leave it out",
        )),
        Some(id) if id.as_str().starts_with(RequestId::SERVER_PREFIX) => Err(ToolError::new(
            "invalid_arguments",
            format!("request ids starting with {:?} are the server's", RequestId::SERVER_PREFIX),
            "choose another request_id, or leave it out",
        )),
        Some(id) => Ok(id),
        None => Ok(RequestId::generate()),
    }
}

/// Milliseconds from a tool argument as microseconds; a value that does not fit is an argument error.
fn micros(name: &str, ms: u64) -> ToolResult<u64> {
    ms.checked_mul(1000).ok_or_else(|| {
        ToolError::new("invalid_arguments", format!("{name} = {ms} ms is too large"), format!("send {name} below {} ms", u64::MAX / 1000))
    })
}

/// Current setters have per-item receipts, not a transactional batch boundary. Refuse the stronger
/// contract before creating a mutation request (or constructing a probe child).
pub(crate) fn validate_atomicity(atomic: bool) -> ToolResult<()> {
    if atomic {
        Err(ToolError::new("unsupported_capability",
            "atomic changes require a transactional provider; current state providers support ordered changes only",
            "inspect state describe for supported mutation semantics"))
    } else {
        Ok(())
    }
}

fn idle_window(ms: u64) -> ToolResult<u64> {
    if ms == 0 {
        return Err(ToolError::new("invalid_arguments", "idle_window_ms must be at least 1", "send settle with an idle window of 1 ms or more"));
    }
    micros("idle_window_ms", ms)
}

pub(crate) fn unexpected(reply: Reply) -> ToolError {
    ToolError::new("internal", format!("unexpected recorder reply {reply:?}"), "stop the generation and report the defect")
}

#[derive(Debug, Serialize)]
pub struct ClockState {
    pub state: &'static str,
    pub t_us: VirtualTime,
    /// The page's date at `t_us` (UTC).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
}

/// `initial_date_ms` is the profile's, which dates the blocked instant as `status` does.
pub(crate) fn suspended(id: &LaunchId, operation: frontcap_recorder_protocol::SuspendedOperation, initial_date_ms: Option<u64>) -> SuspendedResult {
    let date = initial_date_ms.map(|ms| frontcap_core::time::utc_iso(ms + operation.t_us().0 / 1000));
    SuspendedResult {
        state: "suspended", launch_id: id.clone(), operation_id: operation.request_id.clone(),
        clock: ClockState { state: "blocked", t_us: operation.t_us(), date },
        next_action: if operation.request.is_some() {
            "answer the held request with network respond or fail naming its ordinal; then session result for this operation_id"
        } else {
            "answer with session answer_dialog; use status for subsequent dialogs and session result for this operation_id"
        },
        dialog: operation.dialog, request: operation.request,
    }
}

fn clock_of(s: &StatusReport) -> ClockState {
    let date = |t: VirtualTime| Some(frontcap_core::time::utc_iso(s.date_ms.saturating_sub(s.t_us.0 / 1000) + t.0 / 1000));
    if let Some(operation) = &s.suspension {
        return ClockState { state: "blocked", t_us: operation.t_us(), date: date(operation.t_us()) };
    }
    ClockState { state: if s.running.is_some() { "advancing" } else { "frozen" }, t_us: s.t_us, date: date(s.t_us) }
}

#[derive(Serialize)]
struct BootstrapResult {
    levels: Levels,
    hosts: Hosts,
    ffmpeg: bool,
    live_runtimes: usize,
    /// The ways to save a starting point here: `checkpoint` (a process image) only where browsers run in
    /// cells.
    basis_kinds: Vec<&'static str>,
    not_yet_available: Vec<&'static str>,
    next_action: &'static str,
}

#[derive(Serialize)]
struct Levels {
    time: bool,
    frame: bool,
}

#[derive(Serialize)]
struct Hosts {
    local: bool,
    container: bool,
}

#[derive(Serialize)]
struct LaunchResult {
    launch_id: LaunchId,
    lease: LeaseToken,
    clock: ClockState,
    settle: SettleOutcome,
    /// What loading the page did up to the settle's end, as `act` reports what its action did.
    #[serde(skip_serializing_if = "Option::is_none")]
    effects: Option<EffectsResult>,
    bundle: PathBuf,
}

#[derive(Serialize)]
struct StatusResult {
    ownership: frontcap_recorder_protocol::ControlOwnership,
    clock: ClockState,
    #[serde(flatten)]
    status: serde_json::Map<String, serde_json::Value>,
}

#[derive(Serialize)]
struct ReattachResult {
    lease: LeaseToken,
    status: StatusResult,
    completed_while_detached: Vec<RequestId>,
}

#[derive(Serialize)]
struct Released {
    released: bool,
}

#[derive(Serialize)]
struct StopResult {
    finalized: bool,
    bundle: PathBuf,
}

#[derive(Serialize)]
struct ClockResult {
    outcome: ClockOutcome,
    started_us: VirtualTime,
    t_us: VirtualTime,
    /// Virtual time the wait took: `t_us - started_us`.
    elapsed_us: u64,
}

#[derive(Serialize)]
struct SettleResult {
    outcome: SettleOutcome,
    started_us: VirtualTime,
    t_us: VirtualTime,
    elapsed_us: u64,
}

#[derive(Serialize)]
struct EffectsResult {
    #[serde(flatten)]
    effects: Effects,
    dom_mutations: u64,
}

#[derive(Serialize)]
struct ActResult {
    observation_seq: u64,
    t_before_us: VirtualTime,
    t_after_us: VirtualTime,
    settle: SettleOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    waited: Option<frontcap_core::clock::WaitOutcome>,
    effects: EffectsResult,
}

/// The core action an `act` names; durations in milliseconds become microseconds.
/// A wait operation as the recorder runs it; `now` is a clock read, not an operation.
pub(crate) fn wait_op(op: WaitOp) -> ToolResult<Op> {
    Ok(match op {
        WaitOp::Now {} => return Err(ToolError::new("invalid_arguments", "now reads the clock; it runs nothing", "read the clock with wait now")),
        WaitOp::Advance { by_ms } => Op::Advance { by_us: micros("by_ms", by_ms)? },
        WaitOp::Next { event, max_ms } => Op::Next { event, max_us: micros("max_ms", max_ms)? },
        WaitOp::Settle { idle_window_ms, max_ms } => Op::Settle { idle_us: idle_window(idle_window_ms)?, max_us: micros("max_ms", max_ms)? },
        WaitOp::Until { watch: Some(_), context: Some(_), .. } => {
            return Err(ToolError::new("invalid_arguments", "a watch is judged in its own context", "send until with the watch alone, or with a predicate and its context"));
        }
        WaitOp::Until { watch: Some(watch), predicate: None, max_ms, .. } => Op::Until { watch, max_us: micros("max_ms", max_ms)? },
        WaitOp::Until { watch: None, predicate: Some(predicate), max_ms, context } => Op::UntilPredicate { predicate, max_us: micros("max_ms", max_ms)?, context },
        WaitOp::Until { .. } => {
            return Err(ToolError::new("invalid_arguments", "until takes exactly one of predicate and watch", "send until with a predicate"));
        }
    })
}

/// An act as one recorder operation, with its default settle.
pub(crate) fn act_op(action: ActAction, then: Option<Then>, context: Option<String>) -> ToolResult<Op> {
    let waits = matches!(action, ActAction::WaitActionable { .. });
    let then = match then {
        None if waits => None,
        None => Some(frontcap_recorder_protocol::SettleSpec { idle_us: 250_000, max_us: 2_000_000 }),
        Some(Then::None) => None,
        Some(Then::Settle { idle_window_ms, max_ms }) => {
            Some(frontcap_recorder_protocol::SettleSpec { idle_us: idle_window(idle_window_ms)?, max_us: micros("max_ms", max_ms)? })
        }
    };
    let action = action_of(action)?;
    if matches!(action, frontcap_core::action::Action::WaitActionable { .. }) && then.is_some() {
        return Err(ToolError::new("invalid_arguments", "wait_actionable takes no then", "send it without then, and settle with wait afterwards"));
    }
    Ok(Op::Act { action, then, context })
}

/// A watch's registration change as one recorder operation; `list` and `history` are reads.
pub(crate) fn watch_change(op: WatchOp) -> ToolResult<Op> {
    match op {
        WatchOp::Add { name, predicate, context } => {
            if name.as_str().starts_with(RequestId::SERVER_PREFIX) {
                return Err(ToolError::new("invalid_arguments", format!("watch names starting with {:?} are the server's", RequestId::SERVER_PREFIX), "choose another name"));
            }
            predicate.validate().map_err(|e| ToolError::new("invalid_arguments", e.to_string(), "fix the predicate and send it again"))?;
            Ok(Op::WatchAdd { name, predicate, context })
        }
        WatchOp::Remove { name } => Ok(Op::WatchRemove { name }),
        WatchOp::List {} | WatchOp::History { .. } => {
            Err(ToolError::new("invalid_arguments", "a watch step adds or removes a watch", "read watch list or history after the probe"))
        }
    }
}

pub(crate) fn observe_op(op: ObserveOp) -> frontcap_recorder_protocol::ObserveOp {
    use frontcap_recorder_protocol::ObserveOp as P;
    match op {
        ObserveOp::Scene { scope, depth, max_nodes, image, unfold } => P::Scene { scope, depth, max_nodes, image, unfold },
        ObserveOp::Inspect { target, props } => P::Inspect { target, props },
        ObserveOp::Locate { boundary, point } => P::Locate { boundary, point },
        ObserveOp::LocateImage { image_id, point } => P::LocateImage { image_id, point },
        ObserveOp::Crop { image_id, rect } => P::Crop { image_id, rect },
        ObserveOp::Snapshot { scope, depth, max_nodes, geometry, unfold } => P::Snapshot { scope, depth, max_nodes, geometry, unfold },
        ObserveOp::Diff { since, max_nodes, unfold } => P::Diff { since, max_nodes, unfold },
        ObserveOp::Query { target, props } => P::Query { target, props },
        ObserveOp::Count { target } => P::Count { target },
    }
}

fn action_of(a: ActAction) -> ToolResult<frontcap_core::action::Action> {
    use frontcap_core::action::Action as A;
    Ok(match a {
        ActAction::Click { target } => A::Click { target },
        ActAction::ClickAt { target, point, boundary } => A::ClickAt { target, point, boundary },
        ActAction::HoverAt { target, point, boundary } => A::HoverAt { target, point, boundary },
        ActAction::Dblclick { target } => A::Dblclick { target },
        ActAction::Hover { target } => A::Hover { target },
        ActAction::Type { target, text } => A::Type { target, text },
        ActAction::Press { target, key } => A::Press { target, key },
        ActAction::Fill { target, value } => A::Fill { target, value },
        ActAction::Select { target, values } => A::Select { target, values },
        ActAction::Scroll { target, delta_x, delta_y } => A::Scroll { target, delta_x, delta_y },
        ActAction::Drag { target, to, offset, steps, interval_ms } => A::Drag { target, to, offset, steps, interval_us: micros("interval_ms", interval_ms)? },
        // The recorder runs elsewhere: a relative path is this server's.
        ActAction::Upload { target, files } => {
            let here = std::env::current_dir().map_err(|e| ToolError::new("internal", e.to_string(), "report the defect"))?;
            A::Upload { target, files: files.into_iter().map(|f| here.join(f).display().to_string()).collect() }
        }
        ActAction::Navigate { url } => A::Navigate { url },
        ActAction::Back {} => A::Back {},
        ActAction::Reload {} => A::Reload {},
        ActAction::Close {} => A::Close {},
        ActAction::WaitActionable { target, max_ms } => A::WaitActionable { target, max_us: micros("max_ms", max_ms)? },
    })
}

#[derive(Serialize)]
struct HeldResult {
    held: Vec<frontcap_recorder_protocol::HeldRequest>,
}

#[derive(Serialize)]
struct Ok_ {
    ok: bool,
}

#[derive(Serialize)]
struct WatchValue {
    name: WatchName,
    value: SampleValue,
}

#[derive(Serialize)]
struct WatchList {
    watches: Vec<WatchEntry>,
}

/// A watch with its predicate (consumer runs: a list of names alone did not say what each watched).
#[derive(Serialize)]
struct WatchEntry {
    name: WatchName,
    predicate: Option<frontcap_core::predicate::Predicate>,
}

#[derive(Serialize)]
struct History {
    samples: Vec<HistorySample>,
    /// Samples left out by `changes_only`: each repeated its predecessor's value.
    #[serde(skip_serializing_if = "Option::is_none")]
    omitted: Option<usize>,
}

/// The first sample and every change of value.
fn changes_of(samples: Vec<HistorySample>) -> History {
    let total = samples.len();
    let mut kept: Vec<HistorySample> = Vec::new();
    for s in samples {
        if kept.last().is_none_or(|last| last.value != s.value) {
            kept.push(s);
        }
    }
    History { omitted: Some(total - kept.len()), samples: kept }
}

pub(crate) fn clock_value(outcome: Outcome) -> ToolResult<serde_json::Value> {
    let value = match outcome {
        Outcome::Progress { result, start, end } => Ok(serde_json::json!({
            "outcome": result, "started_us": start.t_us, "t_us": end.t_us,
            "elapsed_us": end.t_us.0.saturating_sub(start.t_us.0), "armed_seq": start.seq, "end": end,
        })),
        Outcome::Clock { result, start_us, t_us, .. } => {
            serde_json::to_value(ClockResult { outcome: result, started_us: start_us, t_us, elapsed_us: t_us.0.saturating_sub(start_us.0) })
        }
        Outcome::Settled { result, start_us, end, .. } => serde_json::to_value(SettleResult {
            outcome: result,
            started_us: start_us,
            t_us: end.t_us,
            elapsed_us: end.t_us.0.saturating_sub(start_us.0),
        }),
        other => return Err(ToolError::new("internal", format!("unexpected outcome {other:?}"), "stop the generation and report the defect")),
    };
    Ok(value.expect("serializes"))
}

/// Replaces each response body and stream message data in a rule snapshot with its size and digest, so
/// the rule structure reads without the payloads.
fn summarize_bodies(snapshot: &mut serde_json::Value) {
    fn summarize(object: &mut serde_json::Value, field: &str) {
        let Some(map) = object.as_object_mut() else { return };
        if let Some(serde_json::Value::String(text)) = map.remove(field) {
            map.insert(format!("{field}_bytes"), text.len().into());
            map.insert(format!("{field}_sha256"), frontcap_core::canonical::sha256_hex(&text).into());
        }
    }
    let Some(rules) = snapshot.get_mut("rules").and_then(|r| r.as_array_mut()) else { return };
    for rule in rules {
        let Some(action) = rule.get_mut("action") else { continue };
        let mut responses: Vec<&mut serde_json::Value> = match action.get("type").and_then(|t| t.as_str()) {
            Some("mock") => vec![action],
            Some("sequence") => action.get_mut("steps").and_then(|s| s.as_array_mut()).map(|s| s.iter_mut().collect()).unwrap_or_default(),
            Some("stream_mock") => {
                for message in action.get_mut("messages").and_then(|m| m.as_array_mut()).into_iter().flatten() {
                    summarize(message, "data");
                }
                Vec::new()
            }
            _ => Vec::new(),
        };
        for response in responses.iter_mut() {
            summarize(response, "body");
            summarize(response, "body_base64");
        }
    }
}

/// A status as `status` lists it: held requests without their bodies, which `network list_held` shows.
fn listed(mut s: StatusReport) -> serde_json::Map<String, serde_json::Value> {
    s.held_requests = s.held_requests.into_iter().map(frontcap_recorder_protocol::HeldRequest::without_body).collect();
    // Watches by name; `watch list` shows their predicates.
    s.watch_predicates.clear();
    let serde_json::Value::Object(mut listed) = serde_json::to_value(s).expect("serializes") else { unreachable!("a status is an object") };
    // Held requests' ordinals are in `held_requests`; `held` beside `pointer` read as held input.
    listed.remove("held");
    // The page's date is the clock's (`clock.date`).
    listed.remove("date_ms");
    listed
}

pub(crate) fn host_class() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The determinism profile `p` asks for, with defaults for what it leaves out; the host class stays
/// `unspecified` for the caller to set.
pub(crate) fn effective_profile(p: ProfileArgs) -> DeterminismProfile {
    let defaults = DeterminismProfile::default();
    DeterminismProfile {
        initial_buckets: p.initial_buckets,
        initial_caches: p.initial_caches,
        initial_indexed_db: p.initial_indexed_db,
        initial_storage: p.initial_storage,
        initial_dom_storage: p.initial_dom_storage,
        initial_contexts: p.initial_contexts,
        initial_window_name: p.initial_window_name,
        initial_cookies: p.initial_cookies,
        initial_date_ms: p.initial_date_ms.unwrap_or(defaults.initial_date_ms),
        time_zone: p.time_zone.unwrap_or(defaults.time_zone),
        locale: p.locale.unwrap_or(defaults.locale),
        random_seed: p.random_seed.unwrap_or(defaults.random_seed),
        viewport: p.viewport.map_or(defaults.viewport.clone(), |v| Viewport {
            width: v.width,
            height: v.height,
            device_scale_milli: v.device_scale_milli.unwrap_or(1000),
        }),
        touch: p.touch.unwrap_or(defaults.touch),
        popups: p.popups.unwrap_or(frontcap_core::profile::PopupPolicy::Controlled),
        ..defaults
    }
}

fn ok<T: Serialize>(r: ToolResult<T>) -> CallToolResult {
    match r {
        Ok(v) => CallToolResult::structured(&v),
        Err(e) => e.result(),
    }
}

impl Tools for Frontcap {
    fn list(&self) -> Vec<ToolDescription> {
        descriptions()
    }

    fn call(&self, name: &str, arguments: Option<Box<RawValue>>) -> ToolFuture<'_> {
        let name = name.to_string();
        Box::pin(async move {
            macro_rules! run {
                ($ty:ty, $f:ident) => {
                    match decode::<$ty>(&name, &arguments) {
                        Ok(a) => ok(self.$f(a).await),
                        Err(e) => e,
                    }
                };
            }
            macro_rules! drawer {
                ($ty:ty, $f:ident) => {
                    drawer!($ty, $f, crate::tools::Describe::of::<$ty>())
                };
                ($ty:ty, $f:ident, $described:expr) => {
                    match drawer::<$ty>(&name, arguments.as_ref().map_or("{}", |a| a.get())) {
                        Ok(Drawer::Describe(of)) => ok($described.answer(of.as_deref())),
                        Ok(Drawer::Run(common, op)) => ok(match common.launch_id.clone() {
                            Some(id) => self.$f(id, common, op).await,
                            None => Err(ToolError::new("invalid_arguments", format!("{name}: launch_id is required"), format!("call {name} with the launch_id"))),
                        }),
                        Err(e) => e,
                    }
                };
            }
            let describe_only = arguments.as_ref()
                .and_then(|a| serde_json::from_str::<serde_json::Value>(a.get()).ok())
                .is_some_and(|a| a == serde_json::json!({"describe": true}));
            if describe_only && let Some(described) = crate::tools::describe_front_panel(&name) {
                return CallToolResult::structured(&described);
            }
            match name.as_str() {
                "native" => match decode::<crate::native::Args>(&name, &arguments) {
                    Ok(a) => self.native_result(a).await,
                    Err(e) => e,
                },
                "bootstrap" => run!(NoArgs, bootstrap),
                "launch" => run!(LaunchArgs, launch),
                "status" => run!(GenerationArg, status),
                "act" => run!(ActArgs, act),
                "wait" => run!(WaitArgs, wait),
                "observe" => match decode::<ObserveArgs>(&name, &arguments) {
                    Ok(a) => self.observe_result(a).await,
                    Err(e) => e,
                },
                "stop" => run!(StopArgs, stop),
                "capture" => match decode::<CaptureArgs>(&name, &arguments) {
                    Ok(a) => self.capture(a).await,
                    Err(e) => e,
                },
                "network" => drawer!(NetworkOp, network, describe_network()),
                "watch" => drawer!(WatchOp, watch, describe_watch()),
                "session" => match drawer::<SessionOp>(&name, arguments.as_ref().map_or("{}", |a| a.get())) {
                    Ok(Drawer::Describe(of)) => ok(describe_session().answer(of.as_deref())),
                    Ok(Drawer::Run(common, SessionOp::List { after })) => {
                        if common.launch_id.is_some() || common.lease.is_some() || common.request_id.is_some() {
                            ToolError::new("invalid_arguments", "session list takes only after", "call session describe").result()
                        } else {
                            ok(crate::discovery::list(&self.config.workspace, after))
                        }
                    }
                    Ok(Drawer::Run(common, SessionOp::Cancel { operation_id })) if common.launch_id.is_none() => ok(self.cancel_probe(common, operation_id).await),
                    Ok(Drawer::Run(common, op)) => ok(match common.launch_id.clone() {
                        Some(id) => self.session(id, common, op).await,
                        None => Err(ToolError::new("invalid_arguments", "session: launch_id is required", "use session list to discover generations")),
                    }),
                    Err(e) => e,
                },
                "debug" => match drawer::<DebugOp>(&name, arguments.as_ref().map_or("{}", |a| a.get())) {
                    Ok(Drawer::Describe(of)) => ok(describe_debug().answer(of.as_deref())),
                    Ok(Drawer::Run(common, op)) => match common.launch_id.clone() {
                        Some(id) => match self.debug(id.clone(), common, op).await {
                            Ok(value) => CallToolResult::structured(&value),
                            Err(error) => crate::result_pages::error(&self.config.workspace.root, &id, error),
                        },
                        None => ToolError::new("invalid_arguments", "debug: launch_id is required", "call debug with the launch_id").result(),
                    },
                    Err(error) => error,
                },
                "state" => match drawer::<StateOp>(&name, arguments.as_ref().map_or("{}", |a| a.get())) {
                    Ok(Drawer::Describe(of)) => ok(describe_state().answer(of.as_deref())),
                    Ok(Drawer::Run(common, op @ (StateOp::Save { .. } | StateOp::DescribeSaved { .. } | StateOp::DeleteSaved { .. } | StateOp::Branch { .. } | StateOp::Probe { .. }))) => {
                        if common.launch_id.is_some() || common.lease.is_some() {
                            ToolError::new("invalid_arguments", "state save, describe_saved, delete_saved, branch and probe take no launch_id or lease", "call state describe").result()
                        } else {
                            ok(self.saved(common, op).await)
                        }
                    }
                    Ok(Drawer::Run(common, op)) => ok(match common.launch_id.clone() {
                        Some(id) => self.state(id, common, op).await,
                        None => Err(ToolError::new("invalid_arguments", "state: launch_id is required", "call state with the launch_id")),
                    }),
                    Err(e) => e,
                },
                "input" => drawer!(InputOp, input, describe_input()),
                other => ToolError::new("not_found", format!("no tool {other}"), "call tools/list").result(),
            }
        })
    }
}

/// Replays the finalized bundle at `bundle` in a new generation of the configured workspace.
pub async fn replay(config: &Config, bundle: &std::path::Path, url: Option<String>) -> ToolResult<frontcap_recorder::replay::ReplayReport> {
    use frontcap_recorder::replay::{ReplayError, ReplayOptions};
    let options = ReplayOptions { url, watchdog_ms: 10_000, repo_root: config.frontcap_home.clone(), recorder_bin: config.recorder_bin.clone(), env: vec![] };
    frontcap_recorder::replay::replay(&config.workspace, bundle, options).await.map_err(|e| match e {
        ReplayError::NotFound(_) => ToolError::new("invalid_arguments", e.to_string(), "name a bundle directory (.frontcap/runs/<run>)"),
        ReplayError::NotFinalized(_) => ToolError::new("not_finalized", e.to_string(), "stop the generation, then replay it"),
        ReplayError::Corrupt(_) => ToolError::new("bundle_corrupt", e.to_string(), "restore the bundle from where it was kept, or record the scenario again"),
        ReplayError::NotReplayable { .. } => ToolError::new("not_replayable", e.to_string(), "record the scenario again in a run without a recorder fault"),
        ReplayError::Launch(reply) => from_reply(reply),
        other => ToolError::new("internal", other.to_string(), "check the bundle's inputs/ and report the defect"),
    })
}

impl Frontcap {
    /// The lease a state-changing call presents: the agent's, or the one this server holds.
    fn lease(&self, id: &LaunchId, text: Option<&str>) -> ToolResult<LeaseToken> {
        match text {
            Some(t) => parse_lease(t),
            None => self.connections.lease_of(id).ok_or_else(|| {
                ToolError::new("lease_required", format!("this server holds no lease for {id}"), "take the lease with session reattach, then send the call again")
            }),
        }
    }

    /// The launch profile's initial date, from the generation's launch spec.
    pub(crate) fn initial_date_ms(&self, id: &LaunchId) -> Option<u64> {
        runtime::load_spec(&self.config.workspace.runtime_dir(id)).ok().flatten().map(|spec| spec.profile.initial_date_ms)
    }

    pub(crate) async fn call(&self, id: &LaunchId, call: Call) -> ToolResult<Reply> {
        self.connections.call(&self.config.workspace, id, call).await
    }

    async fn status_report(&self, id: &LaunchId) -> ToolResult<StatusReport> {
        match self.call(id, Call::Status).await? {
            Reply::Status { status } => Ok(*status),
            other => Err(unexpected(other)),
        }
    }

    pub(crate) async fn mutate(&self, id: &LaunchId, lease: LeaseToken, request_id: RequestId, op: Op) -> ToolResult<Mutation> {
        let reply = self.call(id, Call::Mutate { lease: Some(lease.clone()), request_id, op }).await.inspect_err(|e| {
            // The recorder refused the lease: whatever this server held for the generation is stale.
            if e.code == "lease_required" && self.connections.lease_of(id).as_ref() == Some(&lease) {
                self.connections.forget(id);
            }
        })?;
        match reply {
            Reply::Done { outcome: Outcome::NotApplied, request_id } => Err(ToolError::new(
                "not_applied",
                format!("request {request_id} was interrupted before it reached the page"),
                "send the call again with a new request_id",
            )),
            Reply::Done { outcome, .. } => Ok(Mutation::Completed(outcome)),
            Reply::Suspended { operation } => Ok(Mutation::Suspended(Box::new(suspended(id, operation, self.initial_date_ms(id))))),
            other => Err(unexpected(other)),
        }
    }

    async fn bootstrap(&self, _: NoArgs) -> ToolResult<BootstrapResult> {
        let mut live = 0;
        if let Ok(dirs) = std::fs::read_dir(self.config.workspace.root.join("runtimes")) {
            for d in dirs.flatten() {
                if let Ok(Some(r)) = runtime::load_runtime(&d.path())
                    && r.state == RuntimeState::Ready
                    && r.recorder.is_alive()
                {
                    live += 1;
                }
            }
        }
        let ffmpeg = std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        Ok(BootstrapResult {
            levels: Levels { time: true, frame: false },
            hosts: Hosts { local: true, container: false },
            ffmpeg,
            live_runtimes: live,
            // The recorders this server starts inherit FRONTCAP_CELLS.
            basis_kinds: if std::env::var_os("FRONTCAP_CELLS").is_some() { vec!["recipe", "replay", "checkpoint"] } else { vec!["recipe", "replay"] },
            not_yet_available: vec!["frame level", "container host"],
            next_action: "session list to discover existing generations, or launch; with a saved_id, state describe_saved, then branch or probe it",
        })
    }

    async fn launch(&self, mut a: LaunchArgs) -> ToolResult<serde_json::Value> {
        if a.determinism == Determinism::Frame || a.host == Host::Container {
            return Err(ToolError::new(
                "unsupported_capability",
                "level frame and the container host are not available yet",
                "launch with determinism time on the local host",
            ));
        }
        let profile = DeterminismProfile { host_class: host_class(), ..effective_profile(a.profile) };
        a.rules.validate().map_err(|e| ToolError::new("invalid_arguments", e.to_string(), "fix the rules and launch again"))?;
        frontcap_core::rules::validate_passthrough_origins(a.passthrough_origins.as_deref().unwrap_or_default())
            .map_err(|e| ToolError::new("invalid_arguments", e.to_string(), "use scheme://host[:port] in passthrough_origins and launch again"))?;
        server_cassettes(a.rules.rules.iter_mut())?;
        let then = serde_json::to_value(&a.then).expect("serializes");
        let settle = match a.then {
            LaunchThen::None => None,
            LaunchThen::Settle { idle_window_ms, max_ms } => Some(Op::Settle { idle_us: idle_window(idle_window_ms)?, max_us: micros("max_ms", max_ms)? }),
        };
        let req = LaunchRequest {
            request_id: request_id(a.request_id)?,
            url: a.url,
            profile,
            rules: a.rules,
            passthrough_origins: a.passthrough_origins,
            watchdog_ms: 10_000,
            repo_root: self.config.frontcap_home.clone(),
            origin: None,
            restore: None,
        };
        // A retry from this same process finds the lease it already holds.
        let args = frontcap_core::journal::args_sha256(&serde_json::to_value(&req).expect("launch requests serialize"));
        let ws = &self.config.workspace;
        // `then` is registered under its own derived id, so a retry that changes it conflicts too.
        let then_args = frontcap_core::journal::args_sha256(&then);
        launcher::register(ws, &req.request_id.derived("then"), &then_args).map_err(|e| from_reply(e.0))?;
        let registered = launcher::register(ws, &req.request_id, &args).map_err(|e| from_reply(e.0))?;
        let (Registered::New(id) | Registered::Existing(id)) = registered;
        let settle_id = req.request_id.derived("settle");
        let lease = match self.connections.lease_of(&id) {
            Some(lease) => lease,
            None => {
                let attached = launcher::launch(ws, req, &self.config.recorder_bin, &[]).await.map_err(|e| from_reply(e.0))?;
                let lease = attached.lease.clone();
                self.connections.hold(attached.launch_id, attached.lease, attached.client);
                lease
            }
        };
        // Journaled under an id derived from the launch's, so a retried launch is answered, not re-run.
        let (settle, end) = match settle {
            None => (SettleOutcome::NotRequested, None),
            Some(op) => match self.mutate(&id, lease.clone(), settle_id, op).await {
                Ok(Mutation::Completed(Outcome::Settled { result, end, .. })) => (result, Some(end)),
                Ok(Mutation::Suspended(result)) => return Ok(serde_json::to_value(result).expect("serializes")),
                Ok(other) => return Err(ToolError::new("internal", format!("unexpected outcome {other:?}"), "stop the generation and report the defect")),
                // The generation exists and this server holds its lease: the error names it.
                Err(e) => {
                    return Err(ToolError::new(e.code, format!("{} (generation {id} was launched and keeps running)", e.message), format!("{} — the generation is {id}", e.recovery)));
                }
            },
        };
        // The settle's end bounds them, so a retried launch reports what the first one did.
        let effects = match end {
            Some(end) => match self.call(&id, Call::Effects { after_seq: 0, before_seq: Some(end.seq) }).await? {
                Reply::Effects { effects } => Some(EffectsResult { effects: *effects, dom_mutations: end.dom_mutations }),
                other => return Err(unexpected(other)),
            },
            None => None,
        };
        let s = self.status_report(&id).await?;
        Ok(serde_json::to_value(LaunchResult { launch_id: id, lease, clock: clock_of(&s), settle, effects, bundle: s.bundle }).expect("serializes"))
    }

    async fn status(&self, a: GenerationArg) -> ToolResult<serde_json::Value> {
        match self.live_status(&a.launch_id).await {
            Ok(status) => Ok(serde_json::to_value(status).expect("serializes")),
            Err(e) if e.code == "stopped" => self.stopped_status(&a.launch_id),
            Err(e) => Err(e),
        }
    }

    async fn live_status(&self, id: &LaunchId) -> ToolResult<StatusResult> {
        match self.call(id, Call::InspectControl { lease: self.connections.lease_of(id) }).await? {
            Reply::Control { status, ownership } => Ok(StatusResult { clock: clock_of(&status), status: listed(*status), ownership }),
            other => Err(unexpected(other)),
        }
    }

    /// Asks `operation_id`, running in `id`, to stop; it answers its own call where it did.
    async fn cancel(&self, id: &LaunchId, c: Common, operation_id: RequestId) -> ToolResult<serde_json::Value> {
        if c.request_id.is_some() {
            return Err(ToolError::new("invalid_arguments", "session cancel takes no request_id", "name the request to cancel as operation_id"));
        }
        // A generation no controller holds (its controller was lost) takes a cancel without a lease.
        let lease = match c.lease.as_deref() {
            Some(_) => Some(self.lease(id, c.lease.as_deref())?),
            None => self.connections.lease_of(id),
        };
        match self.call(id, Call::Cancel { lease, request_id: operation_id, at_us: None }).await? {
            Reply::Cancelling { request_id } => Ok(serde_json::json!({
                "launch_id": id, "operation_id": request_id, "cancelling": true,
                "next_action": "the cancelled call answers where it stopped; session result reads that answer",
            })),
            other => Err(unexpected(other)),
        }
    }

    /// A probe named by its request id: its child is the one constructed under the probe's derived id.
    async fn cancel_probe(&self, c: Common, operation_id: RequestId) -> ToolResult<serde_json::Value> {
        let construction = operation_id.derived("probe");
        let Some(child) = launcher::registered_launch(&self.config.workspace, &construction) else {
            return Err(ToolError::new("not_found", format!("no probe {operation_id}"), "give session cancel the launch_id of the generation running the request"));
        };
        self.cancel(&child, c, operation_id).await
    }

    async fn session(&self, id: LaunchId, c: Common, op: SessionOp) -> ToolResult<serde_json::Value> {
        let value = match op {
            SessionOp::List { .. } => unreachable!("workspace discovery is dispatched without a generation"),
            SessionOp::Result { operation_id } => match self.call(&id, Call::Result { request_id: operation_id }).await? {
                Reply::Result { request_id, result } => return crate::result_pages::bound(
                    &self.config.workspace.root, &id, serde_json::json!({ "operation_id": request_id, "result": result }),
                ),
                other => return Err(unexpected(other)),
            },
            SessionOp::Reattach {} => serde_json::to_value(self.reattach(id, c.request_id).await?),
            SessionOp::Contexts {} => serde_json::to_value(self.observation(&id, frontcap_recorder_protocol::ObserveOp::Contexts {}).await?),
            SessionOp::Cancel { operation_id } => return self.cancel(&id, c, operation_id).await,
            SessionOp::Release {} => serde_json::to_value(self.release(id, c).await?),
            SessionOp::DialogRules { rules } => {
                completed!(self.mutate(&id, self.lease(&id, c.lease.as_deref())?, request_id(c.request_id)?, Op::DialogRules { rules }).await?);
                serde_json::to_value(Ok_ { ok: true })
            }
            SessionOp::AnswerDialog { operation_id, dialog_id, accept, prompt_text } => {
                let request = request_id(c.request_id)?;
                let lease = self.lease(&id, c.lease.as_deref())?;
                let response = frontcap_recorder_protocol::DialogResponse {
                    parent_request_id: operation_id.clone(), dialog_id, accept, prompt_text,
                };
                let reply = self.call(&id, Call::AnswerDialog {
                    lease: lease.clone(), request_id: request.clone(), response,
                }).await.map_err(|mut e| {
                    if e.code == "lease_required" && self.connections.lease_of(&id).as_ref() == Some(&lease) { self.connections.forget(&id); }
                    if e.code == "no_such_hold" {
                        e.recovery = "read status and answer its current suspension with a new request_id".into();
                    }
                    e
                })?;
                match reply {
                    Reply::DialogAnswered { receipt } => {
                        let (parent, next_action) = self.parent_after_answer(&id, &operation_id, &receipt.dialog.id).await;
                        Ok(serde_json::json!({
                            "operation_id": request, "parent_operation_id": operation_id, "receipt": receipt,
                            "parent": parent, "next_action": next_action,
                        }))
                    }
                    other => return Err(unexpected(other)),
                }
            }
        };
        Ok(value.expect("serializes"))
    }

    /// What became of a suspended operation after the agent answered its dialog or held request
    /// (`answered`): completed, with its answer; suspended again on another; or still running after a
    /// short wall-clock wait.
    async fn parent_after_answer(&self, id: &LaunchId, parent: &RequestId, answered: &str) -> (serde_json::Value, &'static str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(Reply::Result { result: frontcap_recorder_protocol::RequestResult::Completed { answer }, .. }) =
                self.call(id, Call::Result { request_id: parent.clone() }).await
            {
                // An act's effects, as the act itself reports them: what it did, its dialogs included.
                let acted = match answer.as_ref() {
                    frontcap_recorder_protocol::Answer::Ok(Reply::Done { outcome: Outcome::Acted { start, end, .. }, .. }) => Some((*start, *end)),
                    _ => None,
                };
                let effects = match acted {
                    Some((start, end)) => match self.call(id, Call::Effects { after_seq: start.seq, before_seq: Some(end.seq) }).await {
                        Ok(Reply::Effects { effects }) => serde_json::to_value(EffectsResult { effects: *effects, dom_mutations: end.dom_mutations.saturating_sub(start.dom_mutations) }).ok(),
                        _ => None,
                    },
                    None => None,
                };
                let mut parent = serde_json::json!({ "state": "completed", "answer": answer });
                if let Some(effects) = effects { parent["effects"] = effects; }
                return (parent, "the parent completed: its answer is parent.answer, an act's effects parent.effects");
            }
            if let Ok(status) = self.status_report(id).await
                && let Some(next) = status.suspension.filter(|s| &s.request_id == parent)
                && next.dialog.as_ref().map(|d| d.id.as_str()).or(next.request.as_ref().map(|r| r.ordinal.as_str())) != Some(answered)
            {
                return (serde_json::json!({ "state": "suspended", "suspension": next }), "answer the parent's next suspension");
            }
            if tokio::time::Instant::now() >= deadline {
                return (serde_json::json!({ "state": "running" }), "read session result for parent_operation_id once it completes");
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    async fn reattach(&self, launch_id: LaunchId, request: Option<RequestId>) -> ToolResult<ReattachResult> {
        let ws = &self.config.workspace;
        // Recorder death invalidates the old connection's lease, even while this MCP process lives.
        if let Some(r) = runtime::load_runtime(&ws.runtime_dir(&launch_id)).ok().flatten()
            && !r.recorder.is_alive()
            && r.state != RuntimeState::Stopped
        {
            self.connections.forget(&launch_id);
            launcher::recover(ws, &launch_id, &self.config.recorder_bin, &[]).await.map_err(|e| from_reply(e.0))?;
        }
        // This process already is the controller: the lease it holds is the caller's.
        if let Some(lease) = self.connections.lease_of(&launch_id) {
            let status = self.live_status(&launch_id).await?;
            if status.ownership == frontcap_recorder_protocol::ControlOwnership::ThisController {
                return Ok(ReattachResult { lease, status, completed_while_detached: vec![] });
            }
            self.connections.forget(&launch_id);
        }
        let attached = launcher::reattach(ws, &launch_id, request_id(request)?).await.map_err(|e| from_reply(e.0))?;
        let result = ReattachResult {
            lease: attached.lease.clone(),
            status: StatusResult { clock: clock_of(&attached.status), status: listed(attached.status.clone()), ownership: frontcap_recorder_protocol::ControlOwnership::ThisController },
            completed_while_detached: attached.completed_while_detached.clone(),
        };
        self.connections.hold(launch_id, attached.lease, attached.client);
        Ok(result)
    }

    async fn release(&self, launch_id: LaunchId, c: Common) -> ToolResult<Released> {
        let l = self.lease(&launch_id, c.lease.as_deref())?;
        match self.call(&launch_id, Call::Release { lease: l, request_id: request_id(c.request_id)? }).await? {
            Reply::Released => {
                self.connections.forget(&launch_id);
                Ok(Released { released: true })
            }
            other => Err(unexpected(other)),
        }
    }

    async fn stop(&self, a: StopArgs) -> ToolResult<StopResult> {
        let ws = &self.config.workspace;
        // Failed executions must never be recovered just to terminate their remaining process.
        // The recorder ownership lock, not an advisory liveness check, excludes a live controller.
        if let Some(r) = runtime::load_runtime(&ws.runtime_dir(&a.launch_id)).ok().flatten()
            && matches!(r.state, RuntimeState::Failed { .. })
        {
            let bundle = launcher::terminate_failed(ws, &a.launch_id, &request_id(a.request_id)?).await.map_err(|e| from_reply(e.0))?;
            self.connections.forget(&a.launch_id);
            return Ok(StopResult { finalized: false, bundle });
        }
        // A stopped generation has no recorder; a repeated stop is answered from its bundle.
        if let Some(r) = runtime::load_runtime(&ws.runtime_dir(&a.launch_id)).ok().flatten()
            && r.state == RuntimeState::Stopped
        {
            let bundle = self.bundle_dir(&a.launch_id)?;
            let finalized = self.inspect(&bundle)?.finalized;
            self.connections.forget(&a.launch_id);
            return Ok(StopResult { finalized, bundle });
        }
        if let Some(r) = runtime::load_runtime(&ws.runtime_dir(&a.launch_id)).ok().flatten()
            && !r.recorder.is_alive()
            && r.state != RuntimeState::Stopped
        {
            self.connections.forget(&a.launch_id);
            launcher::recover(ws, &a.launch_id, &self.config.recorder_bin, &[]).await.map_err(|e| from_reply(e.0))?;
        }
        let status = self.status_report(&a.launch_id).await?;
        let bundle = status.bundle;
        // Without a lease of its own or the agent's, stop presents none: allowed once the lease expired.
        let mut lease = match a.lease.as_deref() {
            Some(l) => Some(parse_lease(l)?),
            None => self.connections.lease_of(&a.launch_id),
        };
        let request = request_id(a.request_id)?;
        if status.suspension.is_some() {
            let held = lease.take().ok_or_else(|| ToolError::new("lease_required", "stopping a suspended operation requires its controller lease", "stop from its controller or supply the lease"))?;
            match self.call(&a.launch_id, Call::Release { lease: held, request_id: request.derived("stop-release") }).await? {
                Reply::Released => self.connections.forget(&a.launch_id),
                other => return Err(unexpected(other)),
            }
        }
        let reply = self.call(&a.launch_id, Call::Mutate { lease, request_id: request, op: Op::Stop }).await?;
        self.connections.forget(&a.launch_id);
        // `stop` returns once the generation is gone: its recorder records `Stopped` after answering.
        let dir = ws.runtime_dir(&a.launch_id);
        for _ in 0..500 {
            match runtime::load_runtime(&dir).ok().flatten() {
                Some(r) if r.state == RuntimeState::Stopped || !r.recorder.is_alive() => break,
                _ => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            }
        }
        match reply {
            Reply::Done { outcome: Outcome::Stopped { finalized }, .. } => Ok(StopResult { finalized, bundle }),
            other => Err(unexpected(other)),
        }
    }

    async fn wait(&self, a: WaitArgs) -> ToolResult<serde_json::Value> {
        if let WaitOp::Now {} = a.op {
            let s = self.status_report(&a.launch_id).await?;
            return Ok(serde_json::to_value(clock_of(&s)).expect("serializes"));
        }
        let op = wait_op(a.op)?;
        let interception = matches!(op, Op::Next { event: frontcap_core::event::ProgressEvent::Interception, .. });
        let outcome = completed!(self.mutate(&a.launch_id, self.lease(&a.launch_id, a.lease.as_deref())?, request_id(a.request_id)?, op).await?);
        let mut value = self.waited(&a.launch_id, outcome).await?;
        // A request already held when the wait was armed is not its next interception; a consumer spent a
        // wait's budget on one (scenario 5 rerun). Requests held after a timed-out wait were held before it.
        if interception && value["outcome"]["type"] == "event_timed_out" {
            let held = self.status_report(&a.launch_id).await?.held_requests;
            if !held.is_empty() {
                value["already_held"] = held.iter().map(|h| serde_json::json!({"ordinal": h.ordinal, "method": h.method, "url": h.url, "rule": h.rule})).collect();
            }
        }
        Ok(value)
    }

    /// A wait's result: its clock outcome and what happened while time ran.
    pub(crate) async fn waited(&self, id: &LaunchId, outcome: Outcome) -> ToolResult<serde_json::Value> {
        let answered = match &outcome {
            Outcome::Progress { result: ClockOutcome::Event { triggers, .. }, .. } => triggers.iter().any(|t| matches!(t, frontcap_core::event::ProgressTrigger::Response { .. })),
            _ => false,
        };
        let span = match &outcome {
            Outcome::Progress { start, end, .. } | Outcome::Clock { start: Some(start), end: Some(end), .. } => Some((*start, *end)),
            Outcome::Settled { start: Some(start), end, .. } => Some((*start, *end)),
            _ => None,
        };
        let mut value = clock_value(outcome)?;
        // What happened while time ran, as an act reports it (app-state consumer rerun: a wait reported
        // none, so the consumer moved time with a hover to see a window's message).
        if let Some((start, end)) = span {
            let effects = match self.call(id, Call::Effects { after_seq: start.seq, before_seq: Some(end.seq) }).await? {
                Reply::Effects { effects } => *effects,
                other => return Err(unexpected(other)),
            };
            value["effects"] = serde_json::to_value(EffectsResult { effects, dom_mutations: end.dom_mutations.saturating_sub(start.dom_mutations) }).expect("serializes");
        }
        // A response stop names the request each response answered, as `debug requests` describes it:
        // it may have been issued before the wait.
        if answered {
            let filter = frontcap_recorder_protocol::NetworkFilter::default();
            let frontcap_recorder_protocol::Observation::Network { entries } = self.observation(id, frontcap_recorder_protocol::ObserveOp::Network { filter }).await? else {
                return Err(ToolError::new("internal", "unexpected requests observation", "report the defect"));
            };
            for trigger in value["outcome"]["triggers"].as_array_mut().into_iter().flatten() {
                if let Some(entry) = entries.iter().find(|e| trigger["kind"] == "response" && trigger["ordinal"] == e.ordinal.as_str()) {
                    trigger["request"] = entry.request.clone().into();
                }
            }
        }
        Ok(value)
    }

    /// One journaled request for the whole act, so a retry with other arguments conflicts; its outcome
    /// carries the trace positions around it, so a retry reports exactly what the first one did.
    async fn act(&self, a: ActArgs) -> ToolResult<serde_json::Value> {
        let l = self.lease(&a.launch_id, a.lease.as_deref())?;
        let rid = request_id(a.request_id)?;
        let op = act_op(a.action, a.then, a.context)?;
        let outcome = completed!(self.mutate(&a.launch_id, l, rid, op).await?);
        self.acted(&a.launch_id, outcome).await
    }

    /// An act's result: the trace positions around it, how it settled and what it caused.
    pub(crate) async fn acted(&self, id: &LaunchId, outcome: Outcome) -> ToolResult<serde_json::Value> {
        let (start, waited, settle, end) = match outcome {
            Outcome::Acted { start, waited, settle, end, .. } => (start, waited, settle.unwrap_or(SettleOutcome::NotRequested), end),
            other => return Err(ToolError::new("internal", format!("unexpected outcome {other:?}"), "stop the generation and report the defect")),
        };
        let effects = match self.call(id, Call::Effects { after_seq: start.seq, before_seq: Some(end.seq) }).await? {
            Reply::Effects { effects } => *effects,
            other => return Err(unexpected(other)),
        };
        Ok(serde_json::to_value(ActResult {
            observation_seq: end.seq,
            t_before_us: start.t_us,
            t_after_us: end.t_us,
            settle,
            waited,
            effects: EffectsResult { effects, dom_mutations: end.dom_mutations.saturating_sub(start.dom_mutations) },
        }).expect("serializes"))
    }

    async fn network(&self, id: LaunchId, c: Common, op: NetworkOp) -> ToolResult<serde_json::Value> {
        let op = match op {
            NetworkOp::SetRules { expected_revision, boundary, mut rules } => {
                server_cassettes(rules.rules.iter_mut())?;
                Op::SetRules { expected_revision, boundary, rules }
            }
            NetworkOp::AddRule { expected_revision, boundary, mut rule, before } => {
                server_cassettes(std::iter::once(&mut *rule))?;
                Op::EditRules { expected_revision, boundary, edit: frontcap_core::rules::RuleEdit::Add { rule, before } }
            }
            NetworkOp::RemoveRule { expected_revision, boundary, id } => Op::EditRules {
                expected_revision, boundary, edit: frontcap_core::rules::RuleEdit::Remove { id },
            },
            NetworkOp::ListRules { bodies } => {
                return match self.observation(&id, frontcap_recorder_protocol::ObserveOp::NetworkRules).await? {
                    frontcap_recorder_protocol::Observation::NetworkRules(rules) => {
                        let mut value = serde_json::to_value(rules).expect("rules serialize");
                        for rule in value["rules"].as_array_mut().into_iter().flatten() {
                            if rule["id"].as_str().is_some_and(|id| id.starts_with("default-")) {
                                rule["default"] = true.into();
                            }
                        }
                        if !bodies {
                            summarize_bodies(&mut value);
                        }
                        crate::result_pages::bound(&self.config.workspace.root, &id, value)
                    }
                    other => Err(ToolError::new("internal", format!("unexpected network rules {other:?}"), "report the defect")),
                };
            }
            NetworkOp::ListHeld {} => {
                let s = self.status_report(&id).await?;
                return Ok(serde_json::to_value(HeldResult { held: s.held_requests }).expect("serializes"));
            }
            NetworkOp::Respond { ordinal, response } => Op::Respond { ordinal, response },
            NetworkOp::Fail { ordinal, reason } => Op::FailHeld { ordinal, reason },
            NetworkOp::Send { ordinal, data, event, id } => Op::StreamSend { ordinal, command: frontcap_recorder_protocol::StreamCommand::Message { data, event, id } },
            NetworkOp::Close { ordinal, code } => Op::StreamSend { ordinal, command: frontcap_recorder_protocol::StreamCommand::Close { code } },
        };
        // An answer to the request a suspended operation's page waits for lets that operation go on.
        let waiting = match &op {
            Op::Respond { ordinal, .. } | Op::FailHeld { ordinal, .. } => self.status_report(&id).await?.suspension
                .filter(|s| s.request.as_ref().is_some_and(|r| r.ordinal == *ordinal))
                .map(|s| (s.request_id, ordinal.clone())),
            _ => None,
        };
        let outcome = completed!(self.mutate(&id, self.lease(&id, c.lease.as_deref())?, request_id(c.request_id)?, op).await?);
        if let Some((parent, ordinal)) = waiting {
            let (result, next_action) = self.parent_after_answer(&id, &parent, &ordinal).await;
            return Ok(serde_json::json!({ "ok": true, "parent_operation_id": parent, "parent": result, "next_action": next_action }));
        }
        if let Outcome::RulesInstalled(receipt) = outcome {
            return Ok(serde_json::to_value(receipt).expect("receipt serializes"));
        }
        // The answer changes the page: the boundary a following rule edit or state change checks.
        let boundary = match self.observation(&id, frontcap_recorder_protocol::ObserveOp::InputState).await? {
            frontcap_recorder_protocol::Observation::Input(state) => Some(state.boundary),
            _ => None,
        };
        Ok(serde_json::json!({"ok": true, "boundary": boundary}))
    }

    async fn capture(&self, a: CaptureArgs) -> CallToolResult {
        let result: ToolResult<CallToolResult> = async {
            match a.op {
                CaptureOp::Keyframes { since_us, until_us, max_frames, rect, event_seqs } => {
                    if a.context.as_deref().is_some_and(|c| c != "page") {
                        return Err(ToolError::new("unsupported_capability", "recorded frames belong to the launch page", "omit context or use page"));
                    }
                    if event_seqs.is_some() && (since_us.is_some() || until_us.is_some() || max_frames.is_some()) {
                        return Err(ToolError::new("invalid_arguments","event_seqs excludes range and max_frames","choose event neighbors or bounded visual changes"));
                    }
                    let bundle = self.bundle_dir(&a.launch_id)?;
                    let (sheet, png) = tokio::task::spawn_blocking(move || match event_seqs {
                        Some(seqs) => crate::keyframes::event_frames(&bundle,&seqs,rect),
                        None => crate::keyframes::derive(&bundle, since_us, until_us, max_frames.unwrap_or(12), rect),
                    }).await.map_err(|e| ToolError::new("internal", e.to_string(), "retry keyframes"))??;
                    let metadata=crate::result_pages::bound(&self.config.workspace.root,&a.launch_id,serde_json::to_value(sheet).expect("sheet serializes"))?;
                    Ok(CallToolResult::with_png(&metadata, &png))
                }
                CaptureOp::RecordedFrames { since_us, until_us } => {
                    if a.context.as_deref().is_some_and(|c| c != "page") {
                        return Err(ToolError::new("unsupported_capability", "recorded frames belong to the launch page", "omit context or use page"));
                    }
                    let frames = crate::recordings::frames(&self.bundle_dir(&a.launch_id)?, since_us, until_us)?;
                    let value = crate::result_pages::bound(&self.config.workspace.root, &a.launch_id, serde_json::to_value(frames).expect("recording serializes"))?;
                    Ok(CallToolResult::structured(&value))
                }
                CaptureOp::RecordedFrame { file } => {
                    if a.context.as_deref().is_some_and(|c| c != "page") {
                        return Err(ToolError::new("unsupported_capability", "recorded frames belong to the launch page", "omit context or use page"));
                    }
                    let (frame, png) = crate::recordings::image(&self.bundle_dir(&a.launch_id)?, &file)?;
                    Ok(CallToolResult::with_png(&frame, &png))
                }
                CaptureOp::Screenshot { inline, target } => {
                    if let Some(target) = target {
                        let image = match self.call(&a.launch_id, Call::Observe { op: frontcap_recorder_protocol::ObserveOp::ScreenshotTarget { target: frontcap_core::predicate::Locator::Ref { id: target } }, context: a.context.clone() }).await? {
                            Reply::Observed { result: frontcap_recorder_protocol::Observation::Image(image) } => image,
                            other => return Err(unexpected(other)),
                        };
                        return if inline {
                            let png = std::fs::read(&image.capture.path).map_err(|e| ToolError::new("internal", e.to_string(), "capture again"))?;
                            Ok(CallToolResult::with_png(&image, &png))
                        } else {
                            Ok(CallToolResult::structured(&image))
                        };
                    }
                    let shot = match self.call(&a.launch_id, Call::Observe { op: frontcap_recorder_protocol::ObserveOp::Screenshot {}, context: a.context.clone() }).await? {
                        Reply::Observed { result: frontcap_recorder_protocol::Observation::Screenshot(s) } => s,
                        other => return Err(unexpected(other)),
                    };
                    if inline {
                        let png = std::fs::read(&shot.path).map_err(|e| ToolError::new("internal", e.to_string(), "capture again"))?;
                        Ok(CallToolResult::with_png(&shot, &png))
                    } else {
                        Ok(CallToolResult::structured(&shot))
                    }
                }
                CaptureOp::RecordStart {} => {
                    match self.mutate(&a.launch_id, self.lease(&a.launch_id, a.lease.as_deref())?, request_id(a.request_id)?, Op::RecordStart).await? {
                        Mutation::Completed(Outcome::Ack) => Ok(CallToolResult::structured(&Ok_ { ok: true })),
                        Mutation::Suspended(result) => Ok(CallToolResult::structured(&result)),
                        other => Err(ToolError::new("internal", format!("unexpected outcome {other:?}"), "report the defect")),
                    }
                }
                CaptureOp::RecordStop {} => match self.mutate(&a.launch_id, self.lease(&a.launch_id, a.lease.as_deref())?, request_id(a.request_id)?, Op::RecordStop).await? {
                    Mutation::Completed(Outcome::Recorded(rec)) => Ok(CallToolResult::structured(&rec)),
                    Mutation::Suspended(result) => Ok(CallToolResult::structured(&result)),
                    other => Err(ToolError::new("internal", format!("unexpected outcome {other:?}"), "stop the generation and report the defect")),
                },
            }
        }
        .await;
        result.unwrap_or_else(|e| e.result())
    }

    async fn observe_result(&self, a: ObserveArgs) -> CallToolResult {
        // Answers that grow with the page are paged when they pass the inline budget (`unfold: true`).
        let query = matches!(&a.op, ObserveOp::Query { .. } | ObserveOp::Snapshot { .. } | ObserveOp::Diff { .. });
        let launch = a.launch_id.clone();
        let result = self.observe(a).await;
        if query {
            return match result {
                Ok(value) => ok(crate::result_pages::bound(&self.config.workspace.root, &launch, serde_json::to_value(value).expect("observation serializes"))),
                Err(error) => crate::result_pages::error(&self.config.workspace.root, &launch, error),
            };
        }
        let capture = match &result {
            Ok(frontcap_recorder_protocol::Observation::Scene(scene)) => scene.presentation.capture.as_ref(),
            Ok(frontcap_recorder_protocol::Observation::Image(image)) => Some(&image.capture),
            _ => None,
        };
        if let Some(capture) = capture {
            return match std::fs::read(&capture.path) {
                Ok(png) => CallToolResult::with_png(result.as_ref().unwrap(), &png),
                Err(e) => ToolError::new("internal", e.to_string(), "request a fresh scene").result(),
            };
        }
        ok(result)
    }

    async fn observe(&self, a: ObserveArgs) -> ToolResult<frontcap_recorder_protocol::Observation> {
        self.observation_in(&a.launch_id, observe_op(a.op), a.context).await
    }

    pub(crate) async fn observation(&self, id: &LaunchId, op: frontcap_recorder_protocol::ObserveOp) -> ToolResult<frontcap_recorder_protocol::Observation> {
        self.observation_in(id, op, None).await
    }

    async fn observation_in(&self, id: &LaunchId, op: frontcap_recorder_protocol::ObserveOp, context: Option<String>) -> ToolResult<frontcap_recorder_protocol::Observation> {
        use frontcap_recorder_protocol::{Observation, ObserveOp};
        match self.call(id, Call::Observe { op: op.clone(), context }).await {
            Ok(Reply::Observed { result }) => Ok(result),
            Ok(other) => Err(unexpected(other)),
            // A stopped generation's histories are its finalized evidence.
            Err(e) if e.code == "stopped" => match op {
                ObserveOp::Console { filter } => Ok(Observation::Console { entries: frontcap_recorder::observe::console(&self.stopped_trace(id)?, &filter) }),
                ObserveOp::Network { filter } => Ok(Observation::Network {
                    entries: frontcap_recorder::observe::network(&self.stopped_trace(id)?, &filter, &Default::default()),
                }),
                _ => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    /// The behavior log of a stopped generation, from its finalized bundle; no recorder is involved.
    fn stopped_trace(&self, id: &LaunchId) -> ToolResult<Vec<frontcap_core::event::BehaviorEvent>> {
        let bundle = self.stopped_bundle(id)?;
        frontcap_core::log::BehaviorLog::read(&bundle.join("trace.jsonl"))
            .map_err(|e| ToolError::new("bundle_corrupt", format!("{id}'s trace: {e}"), "check it with debug bundle"))
    }

    fn stopped_bundle(&self, id: &LaunchId) -> ToolResult<PathBuf> {
        let bundle = self.config.workspace.runs().join(launcher::run_id_of(id).as_str());
        let manifest: frontcap_core::bundle::Manifest = std::fs::read(bundle.join("manifest.json")).ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| ToolError::new("not_finalized", format!("{id} is stopped without finalized evidence"), "finalize it with debug finalize"))?;
        if manifest.finalization != frontcap_core::bundle::Finalization::Finalized {
            return Err(ToolError::new("not_finalized", format!("{id} is not finalized"), "finalize it with debug finalize"));
        }
        Ok(bundle)
    }

    /// What `status` says of a stopped generation, read from its finalized evidence.
    fn stopped_status(&self, id: &LaunchId) -> ToolResult<serde_json::Value> {
        let bundle = self.stopped_bundle(id)?;
        let manifest: frontcap_core::bundle::Manifest = serde_json::from_slice(&std::fs::read(bundle.join("manifest.json")).map_err(|e| ToolError::new("internal", e.to_string(), "report the defect"))?)
            .map_err(|e| ToolError::new("bundle_corrupt", e.to_string(), "check it with debug bundle"))?;
        let t_us = self.stopped_trace(id)?.last().map_or(0, |e| e.t_us.0);
        let commands = frontcap_core::inputs::read_commands(&bundle).ok();
        Ok(serde_json::json!({
            "launch_id": id, "run_id": manifest.run, "stopped": true,
            "clock": {"state": "stopped", "t_us": t_us}, "t_us": t_us,
            "finalized": true, "replayable": manifest.replayable,
            "commands": commands.as_ref().map(Vec::len),
            "last_completed_request_id": commands.as_ref().and_then(|c| c.last()).map(|c| c.request_id.clone()),
            "bundle": bundle,
        }))
    }

    async fn state(&self, id: LaunchId, c: Common, op: StateOp) -> ToolResult<serde_json::Value> {
        let value = match op {
            StateOp::StorageScope { target, context } => serde_json::to_value(self.observation_in(&id, frontcap_recorder_protocol::ObserveOp::StorageScope { target }, context).await?),
            StateOp::Read { target, field, context } => serde_json::to_value(self.observation_in(&id, frontcap_recorder_protocol::ObserveOp::StateRead { target, field }, context).await?),
            StateOp::Apply { boundary, changes, context, progress_ms, atomic } => {
                validate_atomicity(atomic)?;
                if progress_ms.is_some_and(|ms| ms > 10_000) {
                    return Err(ToolError::new("invalid_arguments", "progress_ms is at most 10000", "grant at most 10 s"));
                }
                match completed!(self.mutate(&id, self.lease(&id, c.lease.as_deref())?, request_id(c.request_id)?, Op::StateApply { boundary, changes, context, progress_us: progress_ms.map(|ms| ms * 1000) }).await?) {
                    Outcome::StateApplied(result) => serde_json::to_value(result),
                    other => return Err(ToolError::new("internal", format!("unexpected outcome {other:?}"), "stop the generation and report the defect")),
                }
            }
            StateOp::Save { .. } | StateOp::DescribeSaved { .. } | StateOp::DeleteSaved { .. } | StateOp::Branch { .. } | StateOp::Probe { .. } => {
                return Err(ToolError::new("invalid_arguments", "this operation takes no launch_id", "call state describe"));
            }
        };
        value.map_err(|e| ToolError::new("internal", e.to_string(), "report the defect"))
    }

    async fn input(&self, id: LaunchId, c: Common, op: InputOp) -> ToolResult<serde_json::Value> {
        use frontcap_core::action::Action;
        let (action, context) = match op {
            InputOp::Status {} => {
                return match self.observation(&id, frontcap_recorder_protocol::ObserveOp::InputState).await? {
                    frontcap_recorder_protocol::Observation::Input(state) => serde_json::to_value(state).map_err(|e| ToolError::new("internal", e.to_string(), "report the defect")),
                    other => Err(ToolError::new("internal", format!("unexpected input state {other:?}"), "report the defect")),
                };
            }
            InputOp::Begin { target, point, button, pointer, boundary, context } => (Action::PointerBegin { target, point, button, pointer, boundary }, context),
            InputOp::Wheel { target, point, delta, boundary, context } => (Action::Wheel { target, point, delta, boundary }, context),
            InputOp::Move { handle, point, boundary } => (Action::PointerMove { handle, point, boundary }, None),
            InputOp::Press { handle, button, boundary } => (Action::PointerPress { handle, button, boundary }, None),
            InputOp::Release { handle, button } => (Action::PointerRelease { handle, button }, None),
            InputOp::TouchAdd { handle, target, point, boundary } => (Action::TouchAdd { handle, target, point, boundary }, None),
            InputOp::TouchMove { handle, contact, point, boundary } => (Action::TouchMove { handle, contact, point, boundary }, None),
            InputOp::TouchEnd { handle, contact } => (Action::TouchEnd { handle, contact }, None),
            InputOp::End { handle } => (Action::PointerEnd { handle }, None),
            InputOp::Cancel { handle } => (Action::PointerCancel { handle }, None),
            InputOp::KeyDown { key, context } => (Action::KeyDown { key }, context),
            InputOp::KeyUp { key } => (Action::KeyUp { key }, None),
            InputOp::KeyRepeat { key } => (Action::KeyRepeat { key }, None),
            InputOp::Compose { text, context } => (Action::Compose { text }, context),
            InputOp::Commit { key } => (Action::Commit { key }, None),
        };
        match completed!(self.mutate(&id, self.lease(&id, c.lease.as_deref())?, request_id(c.request_id)?, Op::Act { action, then: None, context }).await?) {
            Outcome::Pointer { state, start, end } => self.inputted(&id, state, start, end).await,
            other => Err(ToolError::new("internal", format!("unexpected input outcome {other:?}"), "report the defect")),
        }
    }

    /// Held input's state after an input operation, with what the page did in response, as an act
    /// reports it (scenario 3 consumer run: held input had none).
    pub(crate) async fn inputted(&self, id: &LaunchId, state: Box<frontcap_core::pointer::PointerState>, start: frontcap_recorder_protocol::Boundary, end: frontcap_recorder_protocol::Boundary) -> ToolResult<serde_json::Value> {
        let effects = match self.call(id, Call::Effects { after_seq: start.seq, before_seq: Some(end.seq) }).await? {
            Reply::Effects { effects } => *effects,
            other => return Err(unexpected(other)),
        };
        let mut value = serde_json::to_value(state).map_err(|e| ToolError::new("internal", e.to_string(), "report the defect"))?;
        value["effects"] = serde_json::to_value(EffectsResult { effects, dom_mutations: end.dom_mutations.saturating_sub(start.dom_mutations) }).expect("serializes");
        Ok(value)
    }

    async fn debug(&self, id: LaunchId, c: Common, op: DebugOp) -> ToolResult<serde_json::Value> {
        use frontcap_recorder_protocol::{ConsoleFilter, NetworkFilter, ObserveOp as P};
        #[derive(Serialize)]
        struct Evaluated {
            value: serde_json::Value,
        }
        let bounded = matches!(&op, DebugOp::Events { .. } | DebugOp::Video { .. } | DebugOp::Artifacts { .. } | DebugOp::Evaluate { .. } | DebugOp::Console { .. } | DebugOp::Requests { .. });
        let value = match op {
            DebugOp::Events { after_seq, limit } => {
                serde_json::to_value(crate::history::page(&self.bundle_dir(&id)?, after_seq, limit)?)
            }
            DebugOp::Video { since_us, until_us, tail_ms } => {
                Ok(crate::video::derive(self.bundle_dir(&id)?, since_us, until_us, tail_ms.unwrap_or(100)).await?)
            }
            DebugOp::Artifacts {} => {
                let artifacts = crate::derived::list(&self.bundle_dir(&id)?)?;
                Ok(serde_json::json!({"artifacts": artifacts, "coverage": "published catalog entries; hashes verify derived files and provenance, not source recording completeness"}))
            }
            DebugOp::Result { result_id, pointer, offset } => return crate::result_pages::read(&self.config.workspace.root, &id, &result_id, &pointer, offset),
            DebugOp::Evaluate { expression, frame } => match completed!(self.mutate(&id, self.lease(&id, c.lease.as_deref())?, request_id(c.request_id)?, match frame {
                Some(frame) => Op::EvaluateInFrame { expression, frame },
                None => Op::Evaluate { expression },
            }).await?) {
                Outcome::Evaluated { value } => serde_json::to_value(Evaluated { value }),
                other => return Err(ToolError::new("internal", format!("unexpected outcome {other:?}"), "stop the generation and report the defect")),
            },
            DebugOp::Console { filter: f } => {
                let filter = ConsoleFilter { since_us: f.since_us, until_us: f.until_us, level: f.level, text_contains: f.text_contains };
                serde_json::to_value(self.observation(&id, P::Console { filter }).await?)
            }
            DebugOp::Requests { filter: f } => {
                let filter = NetworkFilter { since_us: f.since_us, until_us: f.until_us, url_contains: f.url_contains };
                serde_json::to_value(self.observation(&id, P::Network { filter }).await?)
            }
            DebugOp::Bundle {} => {
                let bundle = self.bundle_dir(&id)?;
                serde_json::to_value(self.inspect(&bundle)?)
            }
            DebugOp::Finalize {} => {
                let manifest = launcher::finalize_failed(&self.config.workspace, &id).await.map_err(|e| from_reply(e.0))?;
                serde_json::to_value(manifest)
            }
            DebugOp::Replay { url } => {
                let bundle = self.bundle_dir(&id)?;
                serde_json::to_value(replay(&self.config, &bundle, url).await?)
            }
        };
        let value = value.expect("serializes");
        if bounded { crate::result_pages::bound(&self.config.workspace.root, &id, value) } else { Ok(value) }
    }

    async fn watch(&self, id: LaunchId, c: Common, op: WatchOp) -> ToolResult<serde_json::Value> {
        let op = match op {
            WatchOp::List {} => {
                let s = self.status_report(&id).await?;
                let mut predicates = s.watch_predicates;
                let watches = s.watches.into_iter().map(|name| WatchEntry { predicate: predicates.remove(&name), name }).collect();
                return Ok(serde_json::to_value(WatchList { watches }).expect("serializes"));
            }
            WatchOp::History { name, changes_only } => {
                let samples = match self.call(&id, Call::WatchHistory { name: name.clone() }).await {
                    Ok(Reply::History { samples }) => samples,
                    Ok(other) => return Err(unexpected(other)),
                    Err(e) if e.code == "stopped" => frontcap_recorder::observe::history(&self.stopped_trace(&id)?, &name),
                    Err(e) => return Err(e),
                };
                let history = if changes_only { changes_of(samples) } else { History { samples, omitted: None } };
                return Ok(serde_json::to_value(history).expect("serializes"));
            }
            change => watch_change(change)?,
        };
        let name = match &op {
            Op::WatchAdd { name, .. } | Op::WatchRemove { name } => name.clone(),
            _ => unreachable!("watch_change only accepts registration changes"),
        };
        match completed!(self.mutate(&id, self.lease(&id, c.lease.as_deref())?, request_id(c.request_id)?, op).await?) {
            Outcome::Sample { value } => Ok(serde_json::to_value(WatchValue { name, value }).expect("serializes")),
            other => Err(ToolError::new("internal", format!("unexpected outcome {other:?}"), "stop the generation and report the defect")),
        }
    }

    fn bundle_dir(&self, id: &LaunchId) -> ToolResult<PathBuf> {
        let spec = runtime::load_spec(&self.config.workspace.runtime_dir(id)).ok().flatten();
        spec.map(|s| s.runs_root.join(s.run_id.as_str())).ok_or_else(|| ToolError::new("not_found", format!("no generation {id}"), "session list names the generations; launch one if none is yours"))
    }

    fn inspect(&self, bundle: &std::path::Path) -> ToolResult<frontcap_core::bundle::Inspection> {
        let runs_root = bundle.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let run = frontcap_core::ids::RunId::new(bundle.file_name().and_then(|n| n.to_str()).unwrap_or_default())
            .map_err(|e| ToolError::new("internal", e.to_string(), "report the defect"))?;
        let b = frontcap_core::bundle::Bundle::open(&runs_root, &run).map_err(|e| ToolError::new("internal", e.to_string(), "report the defect"))?;
        b.inspect().map_err(|e| ToolError::new("internal", e.to_string(), "report the defect"))
    }
}

/// A replay cassette names this server's file: the recorder may have been started from elsewhere.
fn server_cassettes<'a>(rules: impl Iterator<Item = &'a mut frontcap_core::rules::Rule>) -> Result<(), ToolError> {
    let here = std::env::current_dir().map_err(|e| ToolError::new("internal", e.to_string(), "report the defect"))?;
    for rule in rules {
        if let frontcap_core::rules::Action::Replay { cassette, .. } = &mut rule.action {
            *cassette = here.join(&*cassette).display().to_string();
        }
    }
    Ok(())
}
