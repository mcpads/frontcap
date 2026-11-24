//! Journaled execution of one state-changing request (runtime-lifecycle.md, Mutation requests): intent
//! before any effect, dispatched immediately before the first command that can change the target, and
//! completed with the answer — success or failure, both definite outcomes.
use crate::control::Inflight;
use crate::fault;
use crate::server::Recorder;
use frontcap_chromium::ChromiumError;
use frontcap_chromium::generation::{Generation, SettleProgress};
use frontcap_chromium::input::{InputPlan, InputStep};
use frontcap_core::action::Action;
use frontcap_core::ids::RequestId;
use frontcap_core::journal::{Begin, args_sha256};
use frontcap_core::time::{VirtualDuration, VirtualTime};
use frontcap_recorder_protocol::{Answer, Boundary, ErrorCode, ErrorReply, Op, OpClass, Outcome, Reply};
use std::sync::Arc;

pub fn code_of(e: &ChromiumError) -> ErrorCode {
    match e {
        ChromiumError::NotActionable(_) | ChromiumError::Offscreen { .. } => ErrorCode::NotActionable,
        ChromiumError::Ambiguous { .. } => ErrorCode::AmbiguousLocator,
        ChromiumError::NotFound(_) => ErrorCode::NotFound,
        ChromiumError::AtomicityViolation { .. } => ErrorCode::AtomicityViolation,
        ChromiumError::Unresponsive { .. } | ChromiumError::RendererCrashed => ErrorCode::PageUnresponsive,
        ChromiumError::Unsupported(_) | ChromiumError::DocumentNotStarted => ErrorCode::UnsupportedCapability,
        e if e.sandbox_restricted() => ErrorCode::UnsupportedCapability,
        ChromiumError::NoSuchHold(_) => ErrorCode::NoSuchHold,
        ChromiumError::NoSuchContext(_) => ErrorCode::NoSuchContext,
        ChromiumError::ContextClosed(_) => ErrorCode::ContextClosed,
        ChromiumError::StaleRef(_) => ErrorCode::StaleRef,
        ChromiumError::StaleCursor(_) => ErrorCode::StaleCursor,
        ChromiumError::Script(_) => ErrorCode::ScriptFailed,
        ChromiumError::NavigationCancelled => ErrorCode::NavigationCancelled,
        ChromiumError::InvalidArgument(_) | ChromiumError::Rules(_) | ChromiumError::Predicate(_) | ChromiumError::ForeignRef(_) => ErrorCode::InvalidArguments,
        ChromiumError::NoFrame => ErrorCode::NoFrame,
        ChromiumError::StaleFrame => ErrorCode::StaleFrame,
        ChromiumError::ObservationChanged(_) | ChromiumError::NotExpected(_) => ErrorCode::ObservationChanged,
        ChromiumError::PageWorkPending(_) => ErrorCode::PageWorkPending,
        ChromiumError::StorageAbsent(_) => ErrorCode::NotFound,
        ChromiumError::InterventionUncertain(_) => ErrorCode::RecorderFault,
        ChromiumError::InputUncertain(_) | ChromiumError::WorkerEnrollment { .. } => ErrorCode::RecorderFault,
        _ => ErrorCode::Internal,
    }
}

/// The absolute virtual time a clock request runs to.
pub fn goal_of(op: &Op, now: VirtualTime) -> Option<VirtualTime> {
    match op {
        Op::Advance { by_us } => Some(now.saturating_add(VirtualDuration(*by_us))),
        Op::Until { max_us, .. } | Op::UntilPredicate { max_us, .. } | Op::Settle { max_us, .. } | Op::Next { max_us, .. } => Some(now.saturating_add(VirtualDuration(*max_us))),
        Op::Act { action: Action::WaitActionable { max_us, .. }, .. } => Some(now.saturating_add(VirtualDuration(*max_us))),
        _ => None,
    }
}

pub(crate) fn boundary(g: &Generation) -> Boundary {
    Boundary { t_us: g.now(), seq: g.next_seq(), dom_mutations: g.dom_mutations() }
}

pub(crate) fn failed(e: ChromiumError) -> Answer {
    let mut reply = ErrorReply::new(code_of(&e), e.to_string());
    match e {
        ChromiumError::ForeignRef(_) => reply.recovery = "observe this generation and use the references it returns".into(),
        ChromiumError::NotExpected(_) => reply.recovery = "read the field and send what it returns as expected".into(),
        ChromiumError::StorageAbsent(_) => reply.recovery = "read the scope or the database without naming what is missing to see what exists".into(),
        ChromiumError::Offscreen { delta_x, delta_y } => {
            reply.recovery = format!("act scroll with this target, delta_x {delta_x}, delta_y {delta_y}, which centers it; then act again, under a new request_id if you named one")
        }
        _ => {}
    }
    Answer::Err(reply)
}

fn internal(e: impl std::fmt::Display) -> Answer {
    Answer::Err(ErrorReply::new(ErrorCode::Internal, e.to_string()))
}

pub(crate) fn done(request_id: &RequestId, outcome: Outcome) -> Answer {
    Answer::Ok(Reply::Done { request_id: request_id.clone(), outcome })
}

pub async fn run(rec: &Arc<Recorder>, id: &RequestId, op: Op) -> Answer {
    let args = args_sha256(&serde_json::to_value(&op).expect("ops serialize"));
    let begun = rec.journal.lock().unwrap().begin(id, &args);
    match begun {
        Err(e) => return internal(e),
        Ok(Begin::Conflict) => {
            return Answer::Err(ErrorReply::new(ErrorCode::RequestIdConflict, format!("{id} was used with other arguments")));
        }
        Ok(Begin::Completed(v)) => return serde_json::from_value(v).unwrap_or_else(internal),
        Ok(Begin::NotApplied) => return done(id, Outcome::NotApplied),
        Ok(Begin::Unresolved) => {
            return Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, format!("{id} was interrupted and is not resolved")));
        }
        Ok(Begin::New) => {}
    }
    fault::point(id.as_str(), "before_dispatch");
    if let Err(e) = rec.update_control(|c| {
        c.pending_ops.insert(id.clone(), op.clone());
    }) {
        return internal(e);
    }
    let mut guard = rec.generation.lock().await;
    let mut answer = match guard.as_mut() {
        None => Answer::Err(ErrorReply::new(ErrorCode::Stopped, "the generation is stopped")),
        Some(g) => {
            rec.flush_controller(g);
            g.arm_cancel(rec.take_armed_cancel(id));
            let start = pending_input(g, &op).await;
            if let Err(e) = rec.update_control(|c| {
                if let Some(missing) = &start.1 {
                    crate::server::missing_input(c, missing);
                }
                c.pending_inputs.insert(id.clone(), start.0);
            }) {
                return internal(e);
            }
            match op.class() {
                OpClass::RecorderState => recorder_state(rec, g, id, &op).await,
                OpClass::Clock => clock(rec, g, id, &op).await,
                OpClass::Input => input(rec, g, id, &op).await,
                OpClass::Navigation => navigation(rec, g, id, &op).await,
                OpClass::Intervention => intervention(rec, g, id, &op).await,
                OpClass::Script => {
                    if let Err(e) = rec.dispatched(id) {
                        return internal(e);
                    }
                    let applied = match &op {
                        Op::Evaluate { expression } => g.evaluate(expression).await.map(|value| Outcome::Evaluated { value }),
                        Op::EvaluateInFrame { expression, frame } => g.evaluate_in_frame(expression, frame).await.map(|value| Outcome::Evaluated { value }),
                        _ => unreachable!("not a script operation"),
                    };
                    let answer = match applied {
                        Ok(outcome) => done(id, outcome),
                        Err(e) => failed(e),
                    };
                    if let Err(e) = save_result(rec, id, &op, &answer) {
                        return internal(e);
                    }
                    fault::point(id.as_str(), "after_effect");
                    answer
                }
                // The server runs a probe's steps as requests of their own (`Recorder::probe`).
                OpClass::Probe => internal("a probe runs through the server"),
                OpClass::Lifecycle => {
                    if let Err(e) = rec.dispatched(id) {
                        return internal(e);
                    }
                    let mut g = guard.take().expect("present");
                    g.set_missing_inputs(missing_inputs(rec, id));
                    let answer = match g.stop().await {
                        Ok(m) => done(id, Outcome::Stopped { finalized: m.finalization == frontcap_core::bundle::Finalization::Finalized }),
                        Err(e) => failed(e),
                    };
                    rec.mark_stopped();
                    answer
                }
            }
        }
    };
    // A wait the controller cancelled stopped at an instant where a replay of this request stops it too.
    if let Some(at) = guard.as_mut().and_then(|g| g.take_cancelled())
        && let Err(e) = rec.update_control(|c| {
            if let Some(p) = c.pending_inputs.get_mut(id) {
                p.cancelled_at_us = Some(at);
            }
        })
    {
        return internal(e);
    }
    if rec.failure_reason().is_some() { return answer; }
    let context_valid = match guard.as_mut() {
        Some(g) => g.pointer_context_valid().await.unwrap_or(false),
        None => true,
    };
    let pointer_failed = !context_valid || (guard.is_some()
        && matches!(&op, Op::Act { action, .. } if action.is_pointer())
        && matches!(&answer, Answer::Err(e) if matches!(e.code, ErrorCode::Internal | ErrorCode::AtomicityViolation | ErrorCode::PageUnresponsive | ErrorCode::RecorderFault)));
    let rules_failed = matches!(&op, Op::SetRules { .. } | Op::EditRules { .. })
        && matches!(&answer, Answer::Err(e) if e.code == ErrorCode::RecorderFault);
    if pointer_failed || rules_failed {
        let reason = if rules_failed { "rule installation publication could not be established" }
            else { "pointer input lost its document or native delivery could not be established" };
        let detail = match &answer {
            Answer::Err(error) => format!("{reason}: {}", error.message),
            _ => reason.to_string(),
        };
        if let Some(mut g) = guard.take() {
            let _ = rec.push_fault(frontcap_recorder_protocol::Fault { at_us: g.now(), affected: vec![id.to_string()], detail: detail.clone() });
            let _ = g.record_recovery_taint(detail.clone());
            g.set_missing_inputs(missing_inputs(rec, id));
            let _ = g.stop().await;
        }
        rec.mark_failed(detail.clone());
        answer = Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, detail));
    }
    if let Some(g) = guard.as_ref() {
        // Counters an answer depends on (DOM mutations of earlier documents) survive a relaunch.
        let state = g.observation_state();
        if let Err(e) = rec.update_observations(state) {
            return internal(e);
        }
    }
    if let Err(reason) = rec.wait_dependents(id).await {
        return Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, reason));
    }
    if let Err(e) = rec.complete(id, &answer) {
        return internal(e);
    }
    fault::point(id.as_str(), "after_complete");
    if let Some(g) = guard.as_ref() {
        rec.refresh(g);
    }
    answer
}

/// The popup a watch or predicate wait judges in: the context it names, or the one its predicate's
/// references name; `None` for the launch page.
pub(crate) fn watch_context(g: &Generation, context: &Option<String>, predicate: &frontcap_core::predicate::Predicate) -> Result<Option<String>, Box<Answer>> {
    use frontcap_chromium::generation::PAGE;
    let mut named = context.clone();
    for id in predicate.target().refs() {
        let Some(of_ref) = g.context_of_ref(id) else { continue };
        if context.as_ref().is_some_and(|c| *c != of_ref) {
            let message = format!("{id} names a node in context {of_ref}, not {}", context.as_deref().unwrap_or(PAGE));
            return Err(Box::new(Answer::Err(ErrorReply::new(ErrorCode::InvalidArguments, message))));
        }
        named = Some(of_ref);
    }
    Ok(named.filter(|c| c != PAGE))
}

/// The popup an act addresses: the context it names, the one its references name, or for input that
/// continues held input (a held pointer, keys, a composition) the one that input is held in; `None` for
/// the launch page. Granted drag intervals address the launch page; `close` addresses a popup.
pub(crate) fn act_context(g: &Generation, op: &Op) -> Result<Option<String>, Box<Answer>> {
    use frontcap_chromium::generation::PAGE;
    let Op::Act { action, context, .. } = op else { return Ok(None) };
    let named_page = |c: &Option<String>| c.clone().filter(|c| c != PAGE);
    let mut named = context.clone();
    for id in action.locators().into_iter().flat_map(|l| l.refs()) {
        let Some(of_ref) = g.context_of_ref(id) else { continue };
        if context.as_ref().is_some_and(|c| *c != of_ref) {
            let message = format!("{id} names a node in context {of_ref}, not {}", context.as_deref().unwrap_or(PAGE));
            return Err(Box::new(Answer::Err(ErrorReply::new(ErrorCode::InvalidArguments, message))));
        }
        named = Some(of_ref);
    }
    if let Some(held) = g.held_input_context(action) {
        if context.is_some() && named_page(&named) != held {
            let message = format!("{} continues the input held in {}, not {}", action.name(), held.as_deref().unwrap_or(PAGE), named.as_deref().unwrap_or(PAGE));
            return Err(Box::new(Answer::Err(ErrorReply::new(ErrorCode::InvalidArguments, message))));
        }
        named = held;
    }
    let Some(popup) = named.filter(|c| c != PAGE) else {
        if action.kind() == frontcap_core::action::ActionKind::Close {
            let message = "close closes a popup named by context (session contexts); the launch page ends with stop";
            return Err(Box::new(Answer::Err(ErrorReply::new(ErrorCode::InvalidArguments, message))));
        }
        return Ok(None);
    };
    Ok(Some(popup))
}

/// Where the request starts and the resolution record of the reference it acts on; the second value
/// names an input that cannot be recorded (a node a path from `documentElement` does not reach).
async fn pending_input(g: &mut Generation, op: &Op) -> (crate::control::PendingInput, Option<String>) {
    let context = match op {
        Op::WatchAdd { predicate, context, .. } | Op::UntilPredicate { predicate, context, .. } => watch_context(g, context, predicate).ok().flatten(),
        _ => act_context(g, op).ok().flatten(),
    };
    let entered = match context {
        Some(popup) => g.enter_context(&popup).is_ok(),
        None => false,
    };
    let pending = pending_resolutions(g, op).await;
    if entered {
        g.leave_context();
    }
    pending
}

async fn pending_resolutions(g: &mut Generation, op: &Op) -> (crate::control::PendingInput, Option<String>) {
    let mut refs = std::collections::BTreeMap::new();
    let mut missing = None;
    let locators = match op {
        Op::Act { action, .. } => action.locators(),
        Op::StateApply { changes, .. } => changes.iter().map(|change| &change.target).collect(),
        Op::WatchAdd { predicate, .. } | Op::UntilPredicate { predicate, .. } => vec![predicate.target()],
        _ => Vec::new(),
    };
    for id in locators.into_iter().flat_map(|l| l.refs()) {
        match g.resolution_of(id).await {
            Ok(Some(r)) => { refs.insert(id.to_string(), r); }
            Ok(None) => missing = Some(format!("resolution of {id}")),
            Err(frontcap_chromium::ChromiumError::StaleRef(_)) => {}
            Err(_) => missing = Some(format!("resolution of {id}")),
        }
    }
    // An upload's bundle paths come from its plan, which reads each file once (`input`).
    (crate::control::PendingInput { t_us: g.now(), refs, uploads: Vec::new(), cancelled_at_us: None }, missing)
}

/// The re-execution inputs the run lacks; a recorder fault makes it not replayable (trace-bundle.md).
fn missing_inputs(rec: &Recorder, stop: &RequestId) -> Vec<String> {
    let c = rec.control.lock().unwrap();
    let mut missing = c.missing_inputs.clone();
    // A request that started and never completed (an internal failure ended it) has no line.
    missing.extend(c.pending_inputs.keys().filter(|id| *id != stop).map(|id| format!("inputs of {id}")));
    if !c.faults.is_empty() {
        missing.push("recorder_fault".into());
    }
    missing
}

/// A state application runs in the context it names: the launch page or a popup (app-state consumer run:
/// a popup's state could be read but not applied).
async fn intervention(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op) -> Answer {
    let Op::StateApply { context, .. } = op else { unreachable!() };
    if let Err(e) = g.enter_context(context.as_deref().unwrap_or(frontcap_chromium::generation::PAGE)) { return failed(e); }
    let answer = plan_intervention(rec, g, id, op).await;
    g.leave_context();
    answer
}

async fn plan_intervention(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op) -> Answer {
    let Op::StateApply { boundary, changes, progress_us, .. } = op else { unreachable!() };
    let plan = match g.state_plan_with(boundary, changes, progress_us.unwrap_or(0)).await { Ok(p) => p, Err(e) => return failed(e) };
    if let Err(e) = rec.update_control(|c| { c.state_plans.insert(id.clone(), plan); }) { return internal(e); }
    if let Err(e) = g.state_begin(id).await { return failed(e); }
    if let Err(e) = rec.dispatched(id) { return internal(e); }
    fault::point(id.as_str(), "after_dispatch");
    continue_intervention(rec, g, id, op).await
}

/// Runs or resumes a planned state application in its context.
pub(crate) async fn continue_intervention(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op) -> Answer {
    let Op::StateApply { context, .. } = op else { unreachable!() };
    if let Err(e) = g.enter_context(context.as_deref().unwrap_or(frontcap_chromium::generation::PAGE)) { return failed(e); }
    let answer = run_intervention(rec, g, id, op).await;
    g.leave_context();
    answer
}

async fn run_intervention(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op) -> Answer {
    let (plan, saved) = {
        let c = rec.control.lock().unwrap();
        (c.state_plans.get(id).cloned(), c.state_receipts.get(id).cloned().unwrap_or_default())
    };
    let Some(plan) = plan else { return failed(ChromiumError::InterventionUncertain("state plan was not persisted".into())); };
    let result = if saved.len() == plan.changes.len() || saved.last().is_some_and(|r| !r.verified) {
        g.state_finish(id, &plan, saved).await
    } else {
        g.state_run(id, &plan, |stage, index, receipts| {
            if stage == "state_saved" {
                rec.update_control(|c| { c.state_receipts.insert(id.clone(), receipts.to_vec()); })?;
            }
            fault::point(id.as_str(), &format!("{stage}_{index}"));
            Ok(())
        }).await
    };
    let answer = match result {
        Ok(result) => done(id, Outcome::StateApplied(result)),
        Err(e) => {
            let prefix = rec.control.lock().unwrap().state_receipts.get(id).cloned().unwrap_or_default();
            let detail = format!("{e}; durable state receipts: {}", serde_json::to_string(&prefix).unwrap());
            if let Err(e) = rec.push_fault(frontcap_recorder_protocol::Fault { at_us: g.now(), affected: vec![id.to_string()], detail: detail.clone() }) { return internal(e); }
            Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, detail))
        }
    };
    if let Err(e) = save_result(rec, id, op, &answer) { return internal(e); }
    fault::point(id.as_str(), "after_effect");
    answer
}

async fn recorder_state(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op) -> Answer {
    if let Err(e) = rec.dispatched(id) {
        return internal(e);
    }
    fault::point(id.as_str(), "after_dispatch");
    let answer = match op {
        Op::WatchAdd { name, predicate, context } => match watch_context(g, context, predicate) {
            Ok(context) => g.watch_add_in(context.as_deref(), name.clone(), predicate.clone()).await.map(|value| Outcome::Sample { value }),
            Err(answer) => return *answer,
        },
        Op::WatchRemove { name } => g.watch_remove(name).await.map(|value| Outcome::Sample { value }),
        Op::Respond { ordinal, response } => g.respond(ordinal, response.clone()).await.map(|_| Outcome::Ack),
        Op::FailHeld { ordinal, reason } => g.fail_held(ordinal, *reason).await.map(|_| Outcome::Ack),
        Op::StreamSend { ordinal, command } => match command.clone() {
            frontcap_recorder_protocol::StreamCommand::Message { data, event, id: sse_id } => g.stream_send(id, ordinal, false, Some(data), event, sse_id, None).await,
            frontcap_recorder_protocol::StreamCommand::Close { code } => g.stream_send(id, ordinal, true, None, None, None, code).await,
        }.map(|_| Outcome::Ack),
        Op::EditRules { expected_revision, boundary, edit } => {
            let origins = rec.spec.passthrough_origins.clone().unwrap_or_else(|| frontcap_chromium::encode::origin_of(&rec.spec.url).into_iter().collect());
            g.edit_network_rules(id, *expected_revision, boundary, edit, &origins).await.map(Outcome::RulesInstalled)
        },
        Op::SetRules { expected_revision, boundary, rules } => {
            let origins = rec.spec.passthrough_origins.clone().unwrap_or_else(|| frontcap_chromium::encode::origin_of(&rec.spec.url).into_iter().collect());
            g.replace_network_rules(id, *expected_revision, boundary, rules.clone(), &origins).await.map(Outcome::RulesInstalled)
        },
        Op::DialogRules { rules } => g.set_dialog_rules(rules.clone()).await.map(|_| Outcome::Ack),
        Op::RecordStart => g.record_start().await.map(|_| Outcome::Ack),
        Op::RecordStop => g.record_stop().await.map(Outcome::Recorded),
        _ => unreachable!("not a recorder-state op"),
    };
    let answer = match answer {
        Ok(outcome) => done(id, outcome),
        Err(e) => return failed(e),
    };
    fault::point(id.as_str(), "after_engine");
    // The state change and the answer are one atomic write; recovery trusts it.
    if let Err(e) = rec.update_control(|c| {
        c.watches = g.watches().clone();
        c.recording = g.is_recording();
        c.dialog_rules = g.dialog_rules();
        c.applied.insert(id.clone(), answer.clone());
    }) {
        return internal(e);
    }
    fault::point(id.as_str(), "after_effect");
    answer
}

async fn clock(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op) -> Answer {
    if let Op::Act { then: Some(_), .. } = op {
        return Answer::Err(ErrorReply::new(ErrorCode::InvalidArguments, "wait_actionable takes no then; settle with a clock request after it"));
    }
    if let Err(a) = act_context(g, op) {
        return *a;
    }
    if let Op::UntilPredicate { predicate, context, .. } = op {
        let context = match watch_context(g, context, predicate) { Ok(context) => context, Err(answer) => return *answer };
        if let Err(e) = crate::predicate_wait::validate(g, id, predicate, context.as_deref()).await { return failed(e); }
    }
    let goal = goal_of(op, g.now()).expect("clock op");
    let event_arm = if let Op::Next { event, .. } = op {
        match g.arm_event(*event).await { Ok(arm) => Some(arm), Err(e) => return failed(e) }
    } else { None };
    let inflight = Inflight { event_arm, request_id: id.clone(), op: op.clone(), goal_us: Some(goal), goal_step: None, plan: None, start: Some(boundary(g)), result: None, quiet_since: None, acted: None };
    if let Err(e) = rec.update_control(|c| c.inflight = Some(inflight)) {
        return internal(e);
    }
    if let Err(e) = rec.dispatched(id) {
        return internal(e);
    }
    fault::point(id.as_str(), "after_dispatch");
    let answer = run_clock(rec, g, id, op, goal, None).await;
    fault::point(id.as_str(), "after_clock");
    if let Err(e) = save_result(rec, id, op, &answer) {
        return internal(e);
    }
    fault::point(id.as_str(), "after_effect");
    answer
}

/// Runs a clock op from the generation's current time to `goal`; also the recovery path, where a settle
/// continues the quiet window it had reached.
pub(crate) async fn run_clock(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op, goal: VirtualTime, quiet_since: Option<VirtualTime>) -> Answer {
    if let Op::UntilPredicate { predicate, context, .. } = op {
        let context = match watch_context(g, context, predicate) { Ok(context) => context, Err(answer) => return *answer };
        return crate::predicate_wait::run(rec, g, id, op, predicate, context.as_deref(), goal).await;
    }
    let left = goal.since(g.now().min(goal));
    // Where the request started: saved with it, so a resumed or repeated one reports the same.
    let begun = rec.control.lock().unwrap().inflight.as_ref().filter(|i| &i.request_id == id).and_then(|i| i.start).unwrap_or_else(|| boundary(g));
    let start_us = begun.t_us;
    let result = match op {
        Op::Next { event, .. } => {
            let inflight = rec.control.lock().unwrap().inflight.clone().filter(|i| &i.request_id == id);
            let Some((arm, start)) = inflight.and_then(|i| Some((i.event_arm?, i.start?))) else {
                return Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, "the event wait's arm boundary was not recorded"));
            };
            g.next_event(*event, &arm, goal).await.map(|result| Outcome::Progress { result, start, end: boundary(g) })
        }
        Op::Advance { .. } => g.advance(left).await.map(|result| Outcome::Clock { result, start_us, t_us: g.now(), start: Some(begun), end: Some(boundary(g)) }),
        Op::Until { watch, .. } => g.until(watch, left).await.map(|result| Outcome::Clock { result, start_us, t_us: g.now(), start: Some(begun), end: Some(boundary(g)) }),
        Op::Settle { idle_us, .. } => g
            .settle_to(VirtualDuration(*idle_us), goal, quiet_since, settle_progress(rec, id))
            .await
            .map(|result| Outcome::Settled { result, start_us, end: boundary(g), start: Some(begun) }),
        Op::Act { action: Action::WaitActionable { target, .. }, .. } => {
            let start = rec.control.lock().unwrap().inflight.as_ref().filter(|i| &i.request_id == id).and_then(|i| i.start);
            let popup = match act_context(g, op) { Ok(popup) => popup, Err(a) => return *a };
            g.wait_actionable_in(popup.as_deref(), target, left).await.map(|waited| {
                let end = boundary(g);
                Outcome::Acted { start: start.unwrap_or(end), acted: end, waited: Some(waited), settle: None, end }
            })
        }
        _ => unreachable!("not a clock op"),
    };
    match result {
        Ok(outcome) => done(id, outcome),
        Err(e) => failed(e),
    }
}

/// Persists a settle's progress after each step, so a fault resumes it with the same budget and quiet
/// window instead of starting both again.
fn settle_progress<'a>(rec: &'a Recorder, id: &'a RequestId) -> impl FnMut(SettleProgress) -> std::io::Result<()> + 'a {
    let mut step = 0;
    move |p| {
        rec.update_control(|c| {
            if let Some(i) = c.inflight.as_mut().filter(|i| &i.request_id == id) {
                i.goal_us = Some(p.goal);
                i.quiet_since = Some(p.quiet_since);
            }
        })?;
        step += 1;
        fault::point(id.as_str(), &format!("settle_step_{step}"));
        Ok(())
    }
}

/// Saves a request's answer once its effect is complete, ahead of the journal's completion.
pub(crate) fn save_result(rec: &Recorder, id: &RequestId, op: &Op, answer: &Answer) -> Result<(), std::io::Error> {
    rec.update_control(|c| match c.inflight.as_mut() {
        Some(i) if &i.request_id == id => i.result = Some(answer.clone()),
        _ => {
            c.inflight = Some(Inflight {
                event_arm: None,
                request_id: id.clone(),
                op: op.clone(),
                goal_us: None,
                goal_step: None,
                plan: None,
                start: None,
                result: Some(answer.clone()),
                quiet_since: None,
                acted: None,
            })
        }
    })
}

async fn input(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op) -> Answer {
    let Op::Act { action, .. } = op else { unreachable!("not an input op") };
    if action.is_held_input() && matches!(op, Op::Act { then: Some(_), .. }) {
        return Answer::Err(ErrorReply::new(ErrorCode::InvalidArguments, "held input operations take no implicit settle"));
    }
    let popup = match act_context(g, op) { Ok(p) => p, Err(a) => return *a };
    let start = boundary(g);
    if let Some(popup) = &popup
        && let Err(e) = g.enter_context(popup)
    {
        return failed(e);
    }
    let plan = g.input_plan(action).await;
    g.leave_context();
    let plan = match plan {
        Ok(p) => p,
        Err(e) => return failed(e),
    };
    // The bundle's copies an upload sets, as its command line names them.
    let uploads: Vec<String> = plan
        .steps
        .iter()
        .flat_map(|s| match s {
            InputStep::SetFiles { files } | InputStep::ChooseFiles { files } => files.iter().map(|f| format!("{}/{}/{}", frontcap_chromium::generation::UPLOADS, f.sha256, f.name)).collect(),
            _ => Vec::new(),
        })
        .collect();
    if !uploads.is_empty()
        && let Err(e) = rec.update_control(|c| {
            if let Some(p) = c.pending_inputs.get_mut(id) {
                p.uploads = uploads;
            }
        })
    {
        return internal(e);
    }
    begin_act(rec, g, id, op, start, Stage::Input { plan: Box::new(plan), from: 0 }).await
}

async fn navigation(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op) -> Answer {
    let Op::Act { action, .. } = op else { unreachable!("not a navigation") };
    let popup = match act_context(g, op) {
        Ok(p) => p,
        Err(a) => return *a,
    };
    let checked = match popup {
        // Refused before dispatch when the window is not open.
        Some(popup) if action.kind() == frontcap_core::action::ActionKind::Close => g.enter_context(&popup).map(|()| g.leave_context()),
        // Refused before dispatch when the page alone decides it (`back` with no earlier entry), in the
        // window it addresses.
        Some(popup) => match g.enter_context(&popup) {
            Ok(()) => {
                let checked = g.navigation_check(action).await;
                g.leave_context();
                checked
            }
            Err(e) => Err(e),
        },
        None => g.navigation_check(action).await,
    };
    if let Err(e) = checked {
        return failed(e);
    }
    let start = boundary(g);
    begin_act(rec, g, id, op, start, Stage::Navigate).await
}

/// Persists the act's plan and start, journals the dispatch, and runs it.
async fn begin_act(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op, start: Boundary, stage: Stage) -> Answer {
    let plan = match &stage {
        Stage::Input { plan, .. } => Some((**plan).clone()),
        _ => None,
    };
    let inflight = Inflight { event_arm: None, request_id: id.clone(), op: op.clone(), goal_us: None, goal_step: None, plan, start: Some(start), result: None, quiet_since: None, acted: None };
    if let Err(e) = rec.update_control(|c| c.inflight = Some(inflight)) {
        return internal(e);
    }
    if let Err(e) = rec.dispatched(id) {
        return internal(e);
    }
    fault::point(id.as_str(), "after_dispatch");
    let answer = continue_act(rec, g, id, op, start, stage).await;
    if let Err(e) = save_result(rec, id, op, &answer) {
        return internal(e);
    }
    fault::point(id.as_str(), "after_effect");
    answer
}

/// Where an act stands: an input plan from a substep on, a navigation not yet performed, or the action
/// done (its end saved) with only the settle left, which continues its saved budget and quiet window.
pub(crate) enum Stage {
    Input { plan: Box<InputPlan>, from: usize },
    Navigate,
    Settle { acted: Boundary, resume: Option<(VirtualTime, Option<VirtualTime>)> },
}

/// The rest of an act from `stage`; also the recovery path.
pub(crate) async fn continue_act(rec: &Recorder, g: &mut Generation, id: &RequestId, op: &Op, start: Boundary, stage: Stage) -> Answer {
    let Op::Act { action, then, .. } = op else { unreachable!("not an act") };
    let (acted, resume) = match stage {
        Stage::Settle { acted, resume } => (acted, resume),
        Stage::Input { plan, from } => {
            // The input goes to its context; progress, the settle included, runs with the launch page.
            let popup = match act_context(g, op) { Ok(p) => p, Err(a) => return *a };
            if let Some(popup) = &popup
                && let Err(e) = g.enter_context(popup)
            {
                return failed(e);
            }
            let steps = input_steps(rec, g, id, &plan, from).await;
            g.leave_context();
            if let Err(e) = steps {
                return *e;
            }
            match acted_at(rec, g, id) {
                Ok(b) => (b, None),
                Err(e) => return *e,
            }
        }
        Stage::Navigate if action.kind() == frontcap_core::action::ActionKind::Close => {
            let popup = match act_context(g, op) {
                Ok(Some(p)) => p,
                Ok(None) => unreachable!("refused before dispatch"),
                Err(a) => return *a,
            };
            if let Err(e) = g.enter_context(&popup) {
                return failed(e);
            }
            let closed = g.close_window(id).await;
            g.leave_context();
            if let Err(e) = closed {
                return failed(e);
            }
            fault::point(id.as_str(), "after_close");
            match acted_at(rec, g, id) {
                Ok(b) => (b, None),
                Err(e) => return *e,
            }
        }
        Stage::Navigate => {
            let popup = match act_context(g, op) { Ok(popup) => popup, Err(a) => return *a };
            if let Some(popup) = &popup && let Err(e) = g.enter_context(popup) {
                return failed(e);
            }
            let navigated = g.navigate_action(id, action).await;
            if popup.is_some() { g.leave_context(); }
            if let Err(e) = navigated {
                return failed(e);
            }
            match acted_at(rec, g, id) {
                Ok(b) => (b, None),
                Err(e) => return *e,
            }
        }
    };
    let settle = match then {
        None => None,
        Some(spec) => {
            let (goal, quiet_since) = match resume {
                Some(r) => r,
                None => {
                    let goal = g.now().saturating_add(VirtualDuration(spec.max_us));
                    if let Err(e) = rec.update_control(|c| {
                        if let Some(i) = c.inflight.as_mut().filter(|i| &i.request_id == id) {
                            i.goal_us = Some(goal);
                        }
                    }) {
                        return internal(e);
                    }
                    (goal, None)
                }
            };
            match g.settle_to(VirtualDuration(spec.idle_us), goal, quiet_since, settle_progress(rec, id)).await {
                Ok(result) => Some(result),
                Err(e) => return failed(e),
            }
        }
    };
    if action.is_held_input() {
        return match g.pointer_state().await { Ok(state) => done(id, Outcome::Pointer { state: Box::new(state), start, end: boundary(g) }), Err(e) => failed(e) };
    }
    done(id, Outcome::Acted { start, acted, waited: None, settle, end: boundary(g) })
}

/// Saves where the action itself ended, before its settle.
fn acted_at(rec: &Recorder, g: &Generation, id: &RequestId) -> Result<Boundary, Box<Answer>> {
    let acted = boundary(g);
    rec.update_control(|c| {
        if let Some(i) = c.inflight.as_mut().filter(|i| &i.request_id == id) {
            i.acted = Some(acted);
            // A drag's last interval goal is not the settle's: the settle saves its own.
            i.goal_us = None;
            i.goal_step = None;
        }
    })
    .map_err(|e| Box::new(internal(e)))?;
    fault::point(id.as_str(), "after_action");
    Ok(acted)
}

/// Dispatches the plan's substeps from `from` on and checks its recipients.
async fn input_steps(rec: &Recorder, g: &mut Generation, id: &RequestId, plan: &InputPlan, from: usize) -> Result<(), Box<Answer>> {
    // Recovery reads the page's receipts; a progress step also saves where it runs to, so a recovery
    // finishes the interval before the next move.
    for step in from..plan.steps.len() {
        if let InputStep::Advance { by_us } = plan.steps[step] {
            let goal = g.now().saturating_add(VirtualDuration(by_us));
            if let Err(e) = rec.update_control(|c| {
                if let Some(i) = c.inflight.as_mut().filter(|i| &i.request_id == id) {
                    i.goal_us = Some(goal);
                    i.goal_step = Some(step);
                }
            }) {
                return Err(Box::new(internal(e)));
            }
        }
        fault::point(id.as_str(), &format!("before_substep_{step}"));
        if let Err(e) = g.input_substep(id, plan, step).await {
            g.input_abort(id, plan, step).await;
            return Err(Box::new(failed(e)));
        }
        fault::point(id.as_str(), &format!("after_substep_{step}"));
    }
    g.input_finish(id, plan).await.map_err(|e| Box::new(failed(e)))
}

#[cfg(test)]
#[path = "execute_tests.rs"]
mod tests;
