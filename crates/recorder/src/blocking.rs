//! Answers to a held request the page's main thread waits for (engine patch 53). The operation that was
//! running is suspended and keeps the generation; an agent's `Respond` or `FailHeld` naming that request
//! is admitted as the operation's dependent, and the operation's own waiting loop applies it.
use crate::{control::HeldAnswerPlan, fault, server::Recorder};
use frontcap_chromium::generation::{BlockingAnswer, BlockingHold};
use frontcap_core::ids::RequestId;
use frontcap_core::journal::{Begin, args_sha256};
use frontcap_recorder_protocol::{Answer, BlockingRequest, ErrorCode, ErrorReply, LeaseToken, Op, Outcome, Reply};
use std::sync::Arc;

fn error(code: ErrorCode, message: impl Into<String>) -> Answer {
    Answer::Err(ErrorReply::new(code, message))
}

pub(crate) fn request_of(hold: BlockingHold) -> BlockingRequest {
    BlockingRequest { ordinal: hold.ordinal, method: hold.method, url: hold.url, rule: hold.rule, rules_revision: hold.rules_revision, t_us: hold.t_us }
}

impl Recorder {
    /// The answers to held requests given while `parent` ran, in admission order, for its command line.
    pub(crate) fn held_answer_inputs(&self, journal: &frontcap_core::journal::Journal, parent: &RequestId) -> Result<Vec<frontcap_core::inputs::HeldAnswerInput>, String> {
        use frontcap_core::journal::RequestState;
        let control = self.control.lock().unwrap();
        journal.dependents(parent).iter().filter_map(|id| control.held_answers.get(id).map(|plan| (id, plan))).map(|(id, plan)| {
            let outcome = match journal.state(id) {
                Some(RequestState::Completed(value)) => match serde_json::from_value::<Answer>(value.clone()).map_err(|e| e.to_string())? {
                    Answer::Ok(Reply::Done { outcome, .. }) => crate::server::outcome_tag(&Answer::Ok(Reply::Done { request_id: id.clone(), outcome })),
                    Answer::Err(e) => format!("error:{}", serde_json::to_value(e.code).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()),
                    other => return Err(format!("{id} has an unexpected answer {other:?}")),
                },
                Some(RequestState::NotApplied) => "not_applied".into(),
                _ => return Err(format!("{id} is unresolved")),
            };
            Ok(frontcap_core::inputs::HeldAnswerInput {
                request_id: id.clone(),
                op: serde_json::to_value(&plan.op).expect("operations serialize"),
                outcome,
            })
        }).collect()
    }

    /// Admits and applies an answer to the held request the running `parent`'s page waits for. Repeating
    /// the same request id answers what the first attempt did.
    pub(crate) async fn answer_blocking_hold(self: &Arc<Self>, lease: Option<LeaseToken>, id: RequestId, parent: RequestId, op: Op) -> Answer {
        let _admission = self.dialog_admission.lock().await;
        // The live lease, or the recorder's own cleanup after the lease ended.
        let authorized = match (&*self.lease.lock().unwrap(), &lease) {
            (Some((live, _)), Some(given)) => live == given,
            (None, None) => self.cleanup_pending.load(std::sync::atomic::Ordering::SeqCst),
            _ => false,
        };
        if !authorized {
            return error(ErrorCode::LeaseRequired, "answering a held request presents the live lease");
        }
        let args = args_sha256(&serde_json::to_value(&op).expect("operations serialize"));
        if self.journal.lock().unwrap().state(&id).is_some() {
            return match self.journal.lock().unwrap().begin_dependent(&parent, &id, &args) {
                Ok(Begin::Completed(value)) => serde_json::from_value(value).unwrap_or_else(|e| error(ErrorCode::RecorderFault, format!("invalid saved answer: {e}"))),
                Ok(Begin::Conflict) => error(ErrorCode::RequestIdConflict, format!("{id} has different arguments or parent")),
                Ok(Begin::NotApplied) => crate::execute::done(&id, Outcome::NotApplied),
                Ok(Begin::Unresolved) => error(ErrorCode::RecorderFault, format!("{id} was interrupted and is not resolved")),
                Ok(Begin::New) => unreachable!("request exists"),
                Err(e) => error(ErrorCode::RecorderFault, e.to_string()),
            };
        }
        let (ordinal, answer) = match &op {
            Op::Respond { ordinal, response } => (ordinal.clone(), BlockingAnswer::Respond(response.clone())),
            Op::FailHeld { ordinal, reason } => (ordinal.clone(), BlockingAnswer::Fail(*reason)),
            _ => return error(ErrorCode::Internal, "not an answer to a held request"),
        };
        if self.holds().pending().is_none_or(|h| h.ordinal != ordinal) {
            return error(ErrorCode::NoSuchHold, format!("{ordinal} is not a held request the page waits for"));
        }
        let begun = self.journal.lock().unwrap().begin_dependent(&parent, &id, &args);
        if !matches!(begun, Ok(Begin::New)) {
            return error(ErrorCode::RecorderFault, format!("cannot admit the answer: {begun:?}"));
        }
        if let Err(e) = self.update_control(|c| { c.held_answers.insert(id.clone(), HeldAnswerPlan { parent: parent.clone(), op: op.clone(), result: None }); }) {
            return error(ErrorCode::RecorderFault, e.to_string());
        }
        fault::point(id.as_str(), "before_dispatch");
        if let Err(e) = self.dispatched(&id) { return error(ErrorCode::RecorderFault, e.to_string()) }
        fault::point(id.as_str(), "after_dispatch");
        let answer = match self.holds().answer(&ordinal, answer).await {
            Ok(()) => Answer::Ok(Reply::Done { request_id: id.clone(), outcome: Outcome::Ack }),
            Err(e) => crate::execute::failed(e),
        };
        fault::point(id.as_str(), "after_effect");
        if let Err(e) = self.update_control(|c| {
            if let Some(plan) = c.held_answers.get_mut(&id) { plan.result = Some(answer.clone()); }
        }) { return error(ErrorCode::RecorderFault, e.to_string()) }
        if let Err(e) = self.journal.lock().unwrap().complete(&id, serde_json::to_value(&answer).expect("answer serializes")) {
            return error(ErrorCode::RecorderFault, e.to_string());
        }
        self.dependent_changed.notify_waiters();
        answer
    }
}
