//! Browser-side dependent dialog answers. The parent owns Generation; this path never locks it.
use crate::{control::{DialogAnswerOrigin, DialogAnswerPlan}, fault, server::Recorder};
use frontcap_core::ids::RequestId;
use frontcap_core::journal::{Begin, args_sha256};
use frontcap_recorder_protocol::{Answer, DialogResponse, ErrorCode, ErrorReply, LeaseToken, Reply, SuspendedOperation};
use std::sync::Arc;

fn error(code: ErrorCode, message: impl Into<String>) -> Answer {
    Answer::Err(ErrorReply::new(code, message))
}

impl Recorder {
    /// Called under the journal lock before the parent's input and completion are published.
    pub(crate) fn dialog_inputs(&self, journal: &frontcap_core::journal::Journal, parent: &RequestId) -> Result<Vec<frontcap_core::inputs::DialogInput>, String> {
        use frontcap_core::{inputs::{DialogAttempt, DialogInput, DialogInputOutcome}, journal::RequestState};
        let control = self.control.lock().unwrap();
        // Answers to held requests are their own inputs (`held_answer_inputs`).
        journal.dependents(parent).iter().filter(|id| !control.held_answers.contains_key(*id)).map(|id| {
            let hash = journal.argument_hash(id).ok_or_else(|| format!("{id} has no admission fingerprint"))?;
            let plan = control.dialog_answers.get(id);
            if let Some(plan) = plan {
                if &plan.response.parent_request_id != parent || plan.response.dialog_id != plan.dialog.id {
                    return Err(format!("{id} has a conflicting parent or dialog"));
                }
                let expected = args_sha256(&serde_json::to_value((plan.origin, &plan.response)).expect("answer arguments serialize"));
                if expected != hash { return Err(format!("{id} has a conflicting argument fingerprint")) }
            }
            let result = match journal.state(id) {
                Some(RequestState::NotApplied) => DialogInputOutcome::NotApplied,
                Some(RequestState::Completed(value)) => match serde_json::from_value::<Answer>(value.clone()).map_err(|e| e.to_string())? {
                    Answer::Ok(Reply::Done { request_id, outcome: frontcap_recorder_protocol::Outcome::NotApplied }) if &request_id == id => DialogInputOutcome::NotApplied,
                    Answer::Ok(Reply::DialogAnswered { receipt }) => {
                        let plan = plan.ok_or_else(|| format!("{id} answered without a saved plan"))?;
                        let expected_prompt = if plan.dialog.kind == frontcap_core::event::DialogKind::Prompt && plan.response.accept {
                            plan.response.prompt_text.as_deref().unwrap_or(&plan.dialog.default_prompt)
                        } else { "" };
                        if &receipt.request_id != id || receipt.dialog != plan.dialog || receipt.accept != plan.response.accept
                            || receipt.source != frontcap_core::event::NativeDialogSource::Answered || receipt.prompt_text != expected_prompt
                        {
                            return Err(format!("{id} has a conflicting answer receipt"));
                        }
                        DialogInputOutcome::Answered { receipt }
                    }
                    _ => return Err(format!("{id} has no definite dialog outcome")),
                },
                _ => return Err(format!("{id} is unresolved")),
            };
            Ok(DialogInput {
                request_id: id.clone(), args_sha256: hash.into(),
                attempt: plan.map(|p| DialogAttempt { origin: p.origin, dialog: p.dialog.clone(), accept: p.response.accept, prompt_text: p.response.prompt_text.clone() }),
                result,
            })
        }).collect()
    }

    pub(crate) fn current_suspension(&self, id: &RequestId) -> Result<Option<SuspendedOperation>, frontcap_chromium::ChromiumError> {
        if let Some(dialog) = self.dialogs().as_ref().map(|d| d.pending()).transpose()?.flatten() {
            return Ok(Some(SuspendedOperation { request_id: id.clone(), dialog: Some(dialog), request: None }));
        }
        Ok(self.holds().pending().map(|hold| SuspendedOperation { request_id: id.clone(), dialog: None, request: Some(Box::new(crate::blocking::request_of(hold))) }))
    }

    pub(crate) fn persist_suspension(&self, operation: &SuspendedOperation) -> std::io::Result<bool> {
        let journal = self.journal.lock().unwrap();
        if !matches!(journal.state(&operation.request_id), Some(frontcap_core::journal::RequestState::Dispatched)) {
            return Ok(false);
        }
        crate::fault::point(operation.request_id.as_str(), "before_suspension");
        self.update_control(|c| c.suspension = Some(operation.clone()))?;
        crate::fault::point(operation.request_id.as_str(), "after_suspension");
        Ok(true)
    }

    /// Register for notification before checking the journal so a fast answer cannot lose its wakeup.
    pub(crate) async fn wait_dependents(&self, id: &RequestId) -> Result<(), String> {
        loop {
            let changed = self.dependent_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(reason) = self.failure_reason() { return Err(reason) }
            if self.journal.lock().unwrap().pending_dependents(id).is_empty() { return Ok(()) }
            changed.await;
        }
    }

    pub(crate) async fn answer_dialog(self: &Arc<Self>, lease: LeaseToken, id: RequestId, response: DialogResponse) -> Answer {
        self.answer_dialog_as(Some(lease), id, response).await
    }

    fn dialog_authorized(&self, live: &Option<(LeaseToken, u64)>, lease: &Option<LeaseToken>) -> bool {
        match (live, lease) {
            (Some((current, _)), Some(expected)) => current == expected,
            (None, None) => self.cleanup_pending.load(std::sync::atomic::Ordering::SeqCst),
            _ => false,
        }
    }

    async fn answer_dialog_as(self: &Arc<Self>, lease: Option<LeaseToken>, id: RequestId, response: DialogResponse) -> Answer {
        let _admission = self.dialog_admission.lock().await;
        if !self.dialog_authorized(&self.lease.lock().unwrap(), &lease) {
            return error(ErrorCode::LeaseRequired, "the dialog answer requires the live lease");
        }
        let Some(dialogs) = self.dialogs() else {
            return error(ErrorCode::UnsupportedCapability, "native dialog control is unavailable");
        };
        let origin = if lease.is_some() { DialogAnswerOrigin::Agent } else { DialogAnswerOrigin::LeaseCleanup };
        let args = args_sha256(&serde_json::to_value((&origin, &response)).expect("dialog admission serializes"));
        let existing = self.journal.lock().unwrap().state(&id).is_some();
        if existing {
            return match self.journal.lock().unwrap().begin_dependent(&response.parent_request_id, &id, &args) {
                Ok(Begin::Completed(value)) => serde_json::from_value(value).unwrap_or_else(|e| error(ErrorCode::RecorderFault, format!("invalid saved dialog answer: {e}"))),
                Ok(Begin::Conflict) => error(ErrorCode::RequestIdConflict, format!("{id} has different arguments or parent")),
                Ok(Begin::NotApplied) => crate::execute::done(&id, frontcap_recorder_protocol::Outcome::NotApplied),
                Ok(Begin::Unresolved) => error(ErrorCode::RecorderFault, format!("{id} needs native receipt reconciliation")),
                Ok(Begin::New) => unreachable!("request exists"),
                Err(e) => error(ErrorCode::RecorderFault, e.to_string()),
            };
        }
        if self.running.lock().unwrap().as_ref() != Some(&response.parent_request_id) {
            return error(ErrorCode::InvalidArguments, "the named parent operation is not running");
        }
        let dialog = match dialogs.refresh_pending().await {
            Ok(Some(dialog)) if dialog.id == response.dialog_id => dialog,
            Ok(_) => return error(ErrorCode::NoSuchHold, "the named dialog is not held by this operation"),
            Err(e) => return error(ErrorCode::RecorderFault, e.to_string()),
        };
        if response.prompt_text.is_some() && (!response.accept || dialog.kind != frontcap_core::event::DialogKind::Prompt) {
            return error(ErrorCode::InvalidArguments, "prompt text requires an accepted prompt");
        }
        // The lease may have expired during the browser query. No intent or effect precedes this check.
        let begun = {
            let live = self.lease.lock().unwrap();
            if !self.dialog_authorized(&live, &lease) {
                return error(ErrorCode::LeaseRequired, "the dialog answer's lease expired before admission");
            }
            self.journal.lock().unwrap().begin_dependent(&response.parent_request_id, &id, &args)
        };
        if !matches!(begun, Ok(Begin::New)) {
            return error(ErrorCode::RecorderFault, format!("cannot admit dependent answer: {begun:?}"));
        }
        if let Err(e) = self.update_control(|c| {
            c.dialog_answers.insert(id.clone(), DialogAnswerPlan { origin, response: response.clone(), dialog, result: None });
            // Until ordered dependent replay is wired, never label a run with such answers replayable.
        }) {
            return error(ErrorCode::RecorderFault, e.to_string());
        }
        fault::point(id.as_str(), "before_dispatch");
        if let Err(e) = self.dispatched(&id) { return error(ErrorCode::RecorderFault, e.to_string()) }
        fault::point(id.as_str(), "after_dispatch");
        let receipt = match dialogs.answer(&response.dialog_id, &id, response.accept, response.prompt_text.clone()).await {
            Ok(receipt) => receipt,
            Err(e) => match dialogs.saved_answer(&id).await {
                Ok(Some(receipt)) => receipt,
                _ => return error(ErrorCode::RecorderFault, format!("dependent answer outcome is unresolved: {e}")),
            },
        };
        fault::point(id.as_str(), "after_effect");
        let answer = Answer::Ok(Reply::DialogAnswered { receipt: Box::new(receipt) });
        if let Err(e) = self.update_control(|c| {
            c.dialog_answers.get_mut(&id).expect("admitted answer plan").result = Some(answer.clone());
        }) { return error(ErrorCode::RecorderFault, e.to_string()) }
        fault::point(id.as_str(), "after_result");
        if let Err(e) = self.journal.lock().unwrap().complete(&id, serde_json::to_value(&answer).expect("answer serializes")) {
            return error(ErrorCode::RecorderFault, e.to_string());
        }
        dialogs.acknowledge(&id);
        self.dependent_changed.notify_waiters();
        fault::point(id.as_str(), "after_complete");
        answer
    }
    async fn reconcile_dialog_dependents(&self, parent: &RequestId) -> Result<(), String> {
        // An admitted agent answer finishes before cleanup reconciles its journal evidence.
        let _admission = self.dialog_admission.lock().await;
        let requests = self.journal.lock().unwrap().pending_dependents(parent);
        for id in requests {
            let state = self.journal.lock().unwrap().state(&id).cloned();
            let plan = self.control.lock().unwrap().dialog_answers.get(&id).cloned();
            let answer = match state {
                Some(frontcap_core::journal::RequestState::Intent) => crate::execute::done(&id, frontcap_recorder_protocol::Outcome::NotApplied),
                Some(frontcap_core::journal::RequestState::Dispatched) => {
                    let plan = plan.ok_or_else(|| format!("dispatched dependent {id} has no answer plan"))?;
                    if let Some(answer) = plan.result { answer } else {
                        let dialogs = self.dialogs().ok_or_else(|| "native dialog control is unavailable".to_string())?;
                        let receipt = dialogs.saved_answer(&id).await.map_err(|e| e.to_string())?
                            .ok_or_else(|| format!("dependent {id} has no definite native answer receipt"))?;
                        Answer::Ok(Reply::DialogAnswered { receipt: Box::new(receipt) })
                    }
                }
                _ => continue,
            };
            self.update_control(|c| {
                if let Some(plan) = c.dialog_answers.get_mut(&id) { plan.result = Some(answer.clone()); }
            }).map_err(|e| e.to_string())?;
            self.journal.lock().unwrap().complete(&id, serde_json::to_value(&answer).expect("answer serializes")).map_err(|e| e.to_string())?;
            if let Some(dialogs) = self.dialogs() { dialogs.acknowledge(&id); }
            self.dependent_changed.notify_waiters();
        }
        Ok(())
    }

    /// Service native modals before waiting for the parent that they block. No new virtual grant.
    pub(crate) async fn finish_or_dismiss_dialogs(self: &Arc<Self>) -> Result<(), String> {
        let mut deadline = None;
        let mut answered = 0;
        loop {
            let parent = self.running.lock().unwrap().clone();
            let Some(parent) = parent else { return Ok(()) };
            if let Some(reason) = self.failure_reason() { return Err(reason) }
            if deadline.is_some_and(|end| tokio::time::Instant::now() >= end) {
                return Err("dialog cleanup exceeded its 20-second host-service deadline".into());
            }
            if !self.journal.lock().unwrap().pending_dependents(&parent).is_empty() {
                let end = *deadline.get_or_insert_with(|| tokio::time::Instant::now() + std::time::Duration::from_secs(20));
                tokio::time::timeout_at(end, self.reconcile_dialog_dependents(&parent)).await
                    .map_err(|_| "dependent reconciliation exceeded its host deadline".to_string())??;
            }
            if let Some(dialogs) = self.dialogs()
                && let Some(dialog) = dialogs.pending().map_err(|e| e.to_string())?
            {
                if answered == 64 { return Err("dialog cleanup exceeded 64 consecutive dialogs".into()) }
                let end = *deadline.get_or_insert_with(|| tokio::time::Instant::now() + std::time::Duration::from_secs(20));
                let digest = frontcap_core::canonical::sha256_hex(&format!("{}:{}", parent, dialog.id));
                let id = RequestId::new(format!("mcp-dialog-cleanup-{digest}")).expect("bounded cleanup id");
                let response = DialogResponse { parent_request_id: parent, dialog_id: dialog.id, accept: false, prompt_text: None };
                let answer = tokio::time::timeout_at(end, self.answer_dialog_as(None, id, response)).await
                    .map_err(|_| "dialog cleanup answer exceeded its host deadline".to_string())?;
                match answer {
                    Answer::Ok(Reply::DialogAnswered { .. }) => answered += 1,
                    // An already-admitted agent answer can remove this modal while cleanup waits for
                    // admission. Inspect the next state; never dismiss a different dialog under this id.
                    Answer::Err(e) if matches!(e.code, ErrorCode::NoSuchHold | ErrorCode::InvalidArguments) => {},
                    other => return Err(format!("dialog cleanup could not establish a recorded answer: {other:?}")),
                }
            } else if let Some(hold) = self.holds().pending() {
                // A held request the page waits for ends as a dismissed dialog does: failed, as the
                // recorder's own recorded answer, so the parent can finish without a controller.
                if answered == 64 { return Err("held-request cleanup exceeded 64 consecutive holds".into()) }
                let end = *deadline.get_or_insert_with(|| tokio::time::Instant::now() + std::time::Duration::from_secs(20));
                let digest = frontcap_core::canonical::sha256_hex(&format!("{}:{}", parent, hold.ordinal));
                let id = RequestId::new(format!("mcp-held-cleanup-{digest}")).expect("bounded cleanup id");
                let op = frontcap_recorder_protocol::Op::FailHeld { ordinal: hold.ordinal, reason: frontcap_core::rules::FailReason::Aborted };
                let answer = tokio::time::timeout_at(end, self.answer_blocking_hold(None, id, parent, op)).await
                    .map_err(|_| "held-request cleanup exceeded its host deadline".to_string())?;
                match answer {
                    Answer::Ok(Reply::Done { .. }) => answered += 1,
                    Answer::Err(e) if matches!(e.code, ErrorCode::NoSuchHold | ErrorCode::InvalidArguments) => {},
                    other => return Err(format!("held-request cleanup could not establish a recorded answer: {other:?}")),
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

}

#[cfg(test)]
#[path = "dialog_tests.rs"]
mod tests;
