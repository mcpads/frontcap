//! Browser-side dialog reconciliation while renderer initialization is blocked by a retained modal.
use crate::{control::{ControlState, DialogAnswerOrigin, DialogAnswerPlan}, runtime, RecorderError};
use frontcap_chromium::generation::{DialogSession, Generation, ReconnectOptions, Reconnected};
use frontcap_core::{ids::RequestId, journal::{args_sha256, Begin, Journal, RequestState}};
use frontcap_recorder_protocol::{Answer, DialogResponse, Outcome, Reply};
use std::{path::Path, time::Duration};

struct Recovery<'a> {
    dir: &'a Path,
    control: &'a mut ControlState,
    journal: &'a mut Journal,
    dialogs: DialogSession,
    deadline: Option<tokio::time::Instant>,
    answered: usize,
    phase: &'static str,
}

impl Recovery<'_> {
    fn save(&self) -> Result<(), RecorderError> {
        frontcap_core::state::save_atomic(&runtime::control_path(self.dir), self.control)?;
        Ok(())
    }

    fn complete(&mut self, id: &RequestId, answer: Answer) -> Result<(), RecorderError> {
        if let Some(plan) = self.control.dialog_answers.get_mut(id) { plan.result = Some(answer.clone()); }
        self.save()?;
        crate::fault::point(id.as_str(), "after_result");
        self.journal.complete(id, serde_json::to_value(answer).expect("answer serializes"))?;
        self.dialogs.acknowledge(id);
        Ok(())
    }

    async fn answer(&mut self, id: &RequestId, plan: &DialogAnswerPlan) -> Result<Answer, RecorderError> {
        if self.answered == 64 { return Err(RecorderError::Missing("dialog recovery exceeded 64 consecutive dialogs".into())) }
        self.phase = "answering the native dialog";
        let receipt = self.dialogs.answer(&plan.response.dialog_id, id, plan.response.accept, plan.response.prompt_text.clone()).await?;
        self.answered += 1;
        crate::fault::point(id.as_str(), "after_effect");
        Ok(Answer::Ok(Reply::DialogAnswered { receipt: Box::new(receipt) }))
    }

    async fn reconcile(&mut self) -> Result<(), RecorderError> {
        self.phase = "reconciling dependent answer evidence";
        // Include intent-only children; unresolved() intentionally returns only dispatched requests.
        let roots: Vec<_> = self.journal.unresolved().into_iter().filter(|id| self.journal.parent(id).is_none()).collect();
        for parent in roots {
            for id in self.journal.pending_dependents(&parent) {
                let state = self.journal.state(&id).cloned();
                let plan = self.control.dialog_answers.get(&id).cloned();
                let answer = match (state, plan) {
                    (Some(RequestState::Intent), _) => crate::execute::done(&id, Outcome::NotApplied),
                    (Some(RequestState::Dispatched), Some(plan)) => {
                        if let Some(answer) = plan.result.clone() { answer }
                        else if let Some(receipt) = self.dialogs.saved_answer(&id).await? { Answer::Ok(Reply::DialogAnswered { receipt: Box::new(receipt) }) }
                        else if self.dialogs.refresh_pending().await?.is_some_and(|d| d == plan.dialog) {
                            // The same native callback is still retained: this answer was not applied.
                            // An agent's authority ended with its recorder; cleanup already authorized
                            // by lease loss may resume its exact idempotent native request.
                            if plan.origin == DialogAnswerOrigin::LeaseCleanup { self.answer(&id, &plan).await? }
                            else { crate::execute::done(&id, Outcome::NotApplied) }
                        } else {
                            return Err(RecorderError::Missing(format!("dependent {id} has neither its pending dialog nor a definite answer receipt")));
                        }
                    }
                    (Some(RequestState::Dispatched), None) => return Err(RecorderError::Missing(format!("dependent {id} has no answer plan"))),
                    _ => continue,
                };
                self.complete(&id, answer)?;
            }
        }
        // A process can die after journal completion but before the in-memory native acknowledgement.
        for id in self.control.dialog_answers.keys() {
            if matches!(self.journal.state(id), Some(RequestState::Completed(_))) { self.dialogs.acknowledge(id); }
        }
        Ok(())
    }

    async fn dismiss_pending(&mut self) -> Result<(), RecorderError> {
        self.phase = "querying the pending native dialog";
        let Some(dialog) = self.dialogs.refresh_pending().await? else { return Ok(()) };
        let roots: Vec<_> = self.journal.unresolved().into_iter().filter(|id| self.journal.parent(id).is_none()).collect();
        let [parent] = roots.as_slice() else { return Err(RecorderError::Missing("a recovered held dialog requires one dispatched parent".into())) };
        let digest = frontcap_core::canonical::sha256_hex(&format!("{}:{}", parent, dialog.id));
        let base = format!("mcp-dialog-cleanup-{digest}");
        let mut id = RequestId::new(base.clone()).expect("bounded cleanup id");
        // A cleanup intent lost before dispatch was resolved as not applied. A fresh attempt needs
        // its own journal identity; never overwrite that definite historical result.
        let mut attempt = 0;
        while self.journal.state(&id).is_some() {
            attempt += 1;
            id = RequestId::new(format!("{base}-{attempt}")).expect("bounded cleanup id");
        }
        let response = DialogResponse { parent_request_id: parent.clone(), dialog_id: dialog.id.clone(), accept: false, prompt_text: None };
        let origin = DialogAnswerOrigin::LeaseCleanup;
        let args = args_sha256(&serde_json::to_value((origin, &response)).expect("answer arguments serialize"));
        assert_eq!(self.journal.begin_dependent(parent, &id, &args)?, Begin::New);
        let plan = DialogAnswerPlan { origin, response, dialog, result: None };
        self.control.dialog_answers.insert(id.clone(), plan.clone());
        self.save()?;
        crate::fault::point(id.as_str(), "before_dispatch");
        self.journal.dispatched(&id)?;
        crate::fault::point(id.as_str(), "after_dispatch");
        let answer = self.answer(&id, &plan).await?;
        self.complete(&id, answer)?;
        crate::fault::point(id.as_str(), "after_complete");
        Ok(())
    }

    async fn service(&mut self) -> Result<(), RecorderError> {
        if self.dialogs.is_retired() { return Ok(()) }
        let pending_children = self.journal.unresolved().iter().any(|id| !self.journal.pending_dependents(id).is_empty());
        if pending_children || self.dialogs.pending()?.is_some() {
            self.deadline.get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_secs(20));
        }
        let end = self.deadline;
        let work = async { self.reconcile().await?; self.dismiss_pending().await };
        let result = if let Some(end) = end {
            tokio::time::timeout_at(end, work).await
                .unwrap_or_else(|_| Err(RecorderError::Missing(format!("dialog recovery exceeded its host deadline while {}", self.phase))))
        } else { work.await };
        match result {
            Ok(()) => Ok(()),
            // Target replacement can retire this handle during an in-flight inventory query.
            Err(_) if self.dialogs.is_retired() => Ok(()),
            Err(error) => Err(self.fail(error).await),
        }
    }

    async fn fail(&mut self, error: RecorderError) -> RecorderError {
        let mut detail = format!("pre-renderer recovery failed: {error}");
        let at_us = self.dialogs.pending().ok().flatten().map(|d| d.t_us)
            .or_else(|| self.control.suspension.as_ref().map(|s| s.t_us()))
            .unwrap_or_default();
        let browser = runtime::load_runtime(self.dir).ok().flatten().and_then(|r| r.browser);
        let termination = match browser {
            Some(identity) => tokio::task::spawn_blocking(move || {
                identity.kill()?;
                if identity.is_alive() { Err(std::io::Error::other("owned browser remains alive")) } else { Ok(()) }
            }).await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| e.to_string())),
            None => Err("owned browser identity is unavailable".into()),
        };
        if let Err(error) = termination { detail.push_str(&format!("; browser termination failed: {error}")); }
        self.control.faults.push(frontcap_recorder_protocol::Fault {
            at_us, affected: self.journal.unresolved().iter().map(ToString::to_string).collect(), detail: detail.clone(),
        });
        if let Err(error) = self.save() { detail.push_str(&format!("; fault persistence failed: {error}")); }
        RecorderError::Missing(detail)
    }
}

pub(crate) async fn reconnect(dir: &Path, control: &mut ControlState, journal: &mut Journal, mut options: ReconnectOptions) -> Result<(Generation, Reconnected), RecorderError> {
    if control.inflight.as_ref().is_some_and(|inflight| inflight.result.is_none()
        && matches!(journal.state(&inflight.request_id), Some(RequestState::Dispatched))
        && matches!(inflight.op, frontcap_recorder_protocol::Op::Next { event: frontcap_core::event::ProgressEvent::Dialog, .. }))
    {
        // The dead recorder's lease is gone. Retain openings for journaled cleanup, not automatic
        // configured answers, before any renderer command or dialog driver can run.
        options.dialog_rules = frontcap_core::dialog::DialogRules::hold_all();
    }
    let Some(mut prepared) = Generation::prepare_dialog_recovery(&options).await? else {
        if let Some(arm) = pending_interception_arm(control, journal) {
            return Ok(Generation::reconnect_with_interception_arm(options, None, arm).await?);
        }
        return Ok(Generation::reconnect(options).await?);
    };
    if pending_interception_arm(control, journal).is_some() { prepared.preserve_interception_stop(); }
    let mut recovery = Recovery { dir, control, journal, dialogs: prepared.dialogs(), deadline: None, answered: 0, phase: "starting" };
    if let Err(error) = cleanup_blocking_hold(dir, recovery.control, recovery.journal, &mut prepared).await {
        return Err(recovery.fail(error).await);
    }
    recovery.service().await?;
    if let Some(inflight) = &recovery.control.inflight
        && let frontcap_recorder_protocol::Op::Act { action, .. } = &inflight.op
        && action.kind() == frontcap_core::action::ActionKind::Navigation
        && let Some(at_us) = recovery.control.dismissed_navigation(&inflight.request_id)
    {
        prepared.preserve_cancelled_navigation(inflight.request_id.clone(), action.clone(), at_us)?;
    }
    let arm = pending_interception_arm(recovery.control, recovery.journal);
    let reconnect = async move {
        match arm {
            Some(arm) => Generation::reconnect_with_interception_arm(options, Some(prepared), arm).await,
            None => Generation::reconnect_with_dialog_recovery(options, prepared).await,
        }
    };
    tokio::pin!(reconnect);
    loop {
        tokio::select! {
            biased;
            result = &mut reconnect => return Ok(result?),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {
                if let Some(result) = service_with_reconnect(reconnect.as_mut(), recovery.service()).await? {
                    return Ok(result);
                }
            },
        }
    }
}

/// The same lease-loss answer as live held-request cleanup, before Page.enable can run.
async fn cleanup_blocking_hold(dir: &Path, control: &mut ControlState, journal: &mut Journal,
    prepared: &mut frontcap_chromium::generation::DialogRecovery) -> Result<(), RecorderError>
{
    tokio::time::timeout(Duration::from_secs(20), cleanup_blocking_holds_inner(dir, control, journal, prepared)).await
        .unwrap_or_else(|_| Err(RecorderError::Missing("blocking recovery exceeded its host deadline".into())))
}

async fn cleanup_blocking_holds_inner(dir: &Path, control: &mut ControlState, journal: &mut Journal,
    prepared: &mut frontcap_chromium::generation::DialogRecovery) -> Result<(), RecorderError>
{
    if control.suspension.is_none()
        && let Some(inflight) = &control.inflight
        && matches!(journal.state(&inflight.request_id), Some(RequestState::Dispatched))
        && let Some(hold) = prepared.recover_blocking_request(inflight.goal_us.is_some()).await?
    {
        let parent = inflight.request_id.clone();
        control.suspension = Some(frontcap_recorder_protocol::SuspendedOperation {
            request_id: parent.clone(), dialog: None, request: Some(Box::new(crate::blocking::request_of(hold))),
        });
        frontcap_core::state::save_atomic(&runtime::control_path(dir), control)?;
        crate::fault::point(parent.as_str(), "after_recovered_suspension");
    }
    let parent = control.suspension.as_ref().map(|s| s.request_id.clone());
    if parent.as_ref().is_none_or(|id| !matches!(journal.state(id), Some(RequestState::Dispatched))) { return Ok(()) }
    for (id, plan) in &control.held_answers {
        if Some(&plan.parent) == parent.as_ref() && journal.state(id).is_some() {
            let ordinal = match &plan.op {
                frontcap_recorder_protocol::Op::Respond { ordinal, .. } | frontcap_recorder_protocol::Op::FailHeld { ordinal, .. } => ordinal,
                _ => continue,
            };
            prepared.preserve_bound_answer_time(ordinal)?;
        }
    }
    for _ in 0..64 {
        cleanup_one_blocking_hold(dir, control, journal, prepared).await?;
        let Some(request) = control.suspension.as_ref().and_then(|s| s.request.as_ref()) else { return Ok(()) };
        let Some(next) = prepared.next_blocking_hold(&request.ordinal).await? else { return Ok(()) };
        crate::fault::point(control.suspension.as_ref().unwrap().request_id.as_str(), "after_next_blocking_binding");
        control.suspension.as_mut().expect("suspended parent").request = Some(Box::new(crate::blocking::request_of(next)));
        frontcap_core::state::save_atomic(&runtime::control_path(dir), control)?;
        crate::fault::point(control.suspension.as_ref().unwrap().request_id.as_str(), "after_next_blocking_hold");
    }
    Err(RecorderError::Missing("blocking recovery exceeded 64 consecutive synchronous holds".into()))
}

async fn cleanup_one_blocking_hold(dir: &Path, control: &mut ControlState, journal: &mut Journal,
    prepared: &mut frontcap_chromium::generation::DialogRecovery) -> Result<(), RecorderError>
{
    let Some(suspension) = control.suspension.clone() else { return Ok(()) };
    let Some(request) = suspension.request else { return Ok(()) };
    let parent = suspension.request_id;
    if !matches!(journal.state(&parent), Some(RequestState::Dispatched)) { return Ok(()) }
    prepared.preserve_blocking_answer_time(request.ordinal.clone(), request.t_us);
    let digest = frontcap_core::canonical::sha256_hex(&format!("{}:{}", parent, request.ordinal));
    let id = RequestId::new(format!("mcp-held-cleanup-{digest}")).expect("bounded cleanup id");
    for child in journal.dependents(&parent).to_vec().into_iter().filter(|child| child != &id) {
        let Some(plan) = control.held_answers.get(&child).cloned() else { continue };
        let answer = match &plan.op {
            frontcap_recorder_protocol::Op::Respond { ordinal, response } if ordinal == &request.ordinal =>
                frontcap_chromium::generation::BlockingAnswer::Respond(response.clone()),
            frontcap_recorder_protocol::Op::FailHeld { ordinal, reason } if ordinal == &request.ordinal =>
                frontcap_chromium::generation::BlockingAnswer::Fail(*reason),
            _ => continue,
        };
        let completed = match journal.state(&child) {
            Some(RequestState::Completed(value)) => Some(serde_json::from_value::<Answer>(value.clone()).map_err(|e| RecorderError::Missing(format!("{child}: invalid saved answer: {e}")))?),
            _ => plan.result.clone(),
        };
        let outcome = if let Some(completed) = completed { completed }
        else if matches!(journal.state(&child), Some(RequestState::Intent)) {
            crate::execute::done(&child, Outcome::NotApplied)
        } else if prepared.blocking_answer_applied(&request.ordinal, &answer).await? {
            crate::execute::done(&child, Outcome::Ack)
        } else { crate::execute::done(&child, Outcome::NotApplied) };
        if matches!(journal.state(&child), Some(RequestState::Intent | RequestState::Dispatched)) {
            control.held_answers.get_mut(&child).expect("saved answer").result = Some(outcome.clone());
            frontcap_core::state::save_atomic(&runtime::control_path(dir), control)?;
            journal.complete(&child, serde_json::to_value(&outcome).expect("answer serializes"))?;
        }
        if matches!(outcome, Answer::Ok(Reply::Done { outcome: Outcome::Ack, .. })) {
            return Ok(());
        }
    }
    let op = frontcap_recorder_protocol::Op::FailHeld {
        ordinal: request.ordinal.clone(), reason: frontcap_core::rules::FailReason::Aborted,
    };
    let args = args_sha256(&serde_json::to_value(&op).expect("cleanup arguments serialize"));
    match journal.begin_dependent(&parent, &id, &args)? {
        Begin::New | Begin::Unresolved => {},
        Begin::NotApplied if matches!(journal.state(&id), Some(RequestState::Intent)) => {},
        Begin::Completed(_) => return Ok(()),
        other => return Err(RecorderError::Missing(format!("blocking cleanup cannot resume: {other:?}"))),
    }
    control.held_answers.entry(id.clone()).or_insert_with(|| crate::control::HeldAnswerPlan {
        parent: parent.clone(), op, result: None,
    });
    frontcap_core::state::save_atomic(&runtime::control_path(dir), control)?;
    crate::fault::point(id.as_str(), "before_dispatch");
    if matches!(journal.state(&id), Some(RequestState::Intent)) { journal.dispatched(&id)?; }
    crate::fault::point(id.as_str(), "after_dispatch");
    prepared.abort_blocking_hold(&request.ordinal).await?;
    crate::fault::point(id.as_str(), "after_effect");
    let answer = crate::execute::done(&id, Outcome::Ack);
    control.held_answers.get_mut(&id).expect("saved cleanup plan").result = Some(answer.clone());
    frontcap_core::state::save_atomic(&runtime::control_path(dir), control)?;
    crate::fault::point(id.as_str(), "after_result");
    journal.complete(&id, serde_json::to_value(answer).expect("answer serializes"))?;
    crate::fault::point(id.as_str(), "after_complete");
    Ok(())
}

fn pending_interception_arm(control: &ControlState, journal: &Journal) -> Option<frontcap_chromium::generation::EventArm> {
    let inflight = control.inflight.as_ref()?;
    if inflight.result.is_none()
        && matches!(journal.state(&inflight.request_id), Some(RequestState::Dispatched))
        && matches!(inflight.op, frontcap_recorder_protocol::Op::Next { event: frontcap_core::event::ProgressEvent::Interception, .. })
    {
        inflight.event_arm.clone()
    } else {
        None
    }
}

// Reconnect can hold the dialog control's serial lock while flushing retained receipts. Keep polling
// it while recovery queries that same control; awaiting service alone can strand the lock owner.
// If reconnect finishes first, finish the admitted service before publishing its generation.
async fn service_with_reconnect<F, T>(
    reconnect: std::pin::Pin<&mut F>,
    service: impl std::future::Future<Output = Result<(), RecorderError>>,
) -> Result<Option<T>, RecorderError>
where F: std::future::Future<Output = Result<T, frontcap_chromium::ChromiumError>> {
    tokio::pin!(service);
    tokio::select! {
        biased;
        result = reconnect => {
            service.await?;
            Ok(Some(result?))
        }
        result = &mut service => { result?; Ok(None) }
    }
}

#[cfg(test)]
#[path = "dialog_recovery_tests.rs"]
mod tests;
