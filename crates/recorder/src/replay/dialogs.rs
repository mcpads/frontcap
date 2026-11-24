//! Replay dependent answers only after matching the retained opening's stable source and contents.
use super::{answer_of, outcome_tag};
use frontcap_core::{dialog::DialogDescriptor, inputs::{CommandLine, DialogInput, DialogInputOutcome}, projection::rewrite};
use frontcap_recorder_protocol::{client::RecorderClient, Answer, Call, DialogResponse, LeaseToken, Op, Reply, RequestResult, SuspendedOperation};
use std::time::Duration;

type Mismatch = (String, String);

fn mismatch(code: &str, message: impl Into<String>) -> Mismatch { (code.into(), message.into()) }

fn same_opening(expected: &DialogDescriptor, actual: &DialogDescriptor) -> bool {
    let mut expected = expected.clone();
    // This token belongs to one browser execution. Every other opening field must match.
    expected.frame.clone_from(&actual.frame);
    expected == *actual
}

pub(super) fn validate(commands: &[CommandLine]) -> Result<(), super::ReplayError> {
    let mut identities: std::collections::BTreeSet<_> = commands.iter().map(|c| c.request_id.clone()).collect();
    for command in commands {
        for answer in &command.answers {
            if !identities.insert(answer.request_id.clone()) {
                return Err(super::ReplayError::Corrupt(format!("held-request answer {}: duplicate request identity", answer.request_id)));
            }
        }
        for input in &command.dialogs {
            let invalid = |reason| super::ReplayError::Corrupt(format!("dialog input {}: {reason}", input.request_id));
            if !identities.insert(input.request_id.clone()) { return Err(invalid("duplicate request identity")) }
            if let Some(attempt) = &input.attempt {
                let response = DialogResponse { parent_request_id: command.request_id.clone(), dialog_id: attempt.dialog.id.clone(), accept: attempt.accept, prompt_text: attempt.prompt_text.clone() };
                let hash = frontcap_core::journal::args_sha256(&serde_json::to_value((attempt.origin, response)).expect("input arguments serialize"));
                if hash != input.args_sha256 { return Err(invalid("argument fingerprint mismatch")) }
                if attempt.dialog.source.frame_urls.len() != attempt.dialog.source.frame_path.len() + 1
                    || attempt.dialog.source.frame_urls.last() != Some(&attempt.dialog.url)
                { return Err(invalid("inconsistent source ancestry")) }
            }
            if let DialogInputOutcome::Answered { receipt } = &input.result {
                let attempt = input.attempt.as_ref().ok_or_else(|| invalid("answer without attempt"))?;
                let prompt = if attempt.dialog.kind == frontcap_core::event::DialogKind::Prompt && attempt.accept {
                    attempt.prompt_text.as_deref().unwrap_or(&attempt.dialog.default_prompt)
                } else { "" };
                if receipt.request_id != input.request_id || receipt.dialog != attempt.dialog || receipt.accept != attempt.accept
                    || receipt.source != frontcap_core::event::NativeDialogSource::Answered || receipt.prompt_text != prompt
                { return Err(invalid("answer conflicts with its attempt")) }
            }
        }
    }
    Ok(())
}

pub(super) async fn complete(client: &mut RecorderClient, lease: &LeaseToken, cmd: &CommandLine, rewrites: &[(String, String)], initial: Answer, watchdog_ms: u64) -> Result<Answer, Mismatch> {
    let inputs: Vec<DialogInput> = serde_json::from_str(&rewrite(&serde_json::to_string(&cmd.dialogs).expect("inputs serialize"), &super::with_rule_hashes_origins(rewrites)))
        .map_err(|e| mismatch("dialog_inputs_invalid", e.to_string()))?;
    // Undispatched/not-applied intent is evidence, not an instruction to send an answer.
    let applied: Vec<_> = inputs.iter().filter(|d| matches!(d.result, DialogInputOutcome::Answered { .. })).collect();
    // Answers to held requests the page waited for, as the recorded run gave them.
    let held: Vec<frontcap_core::inputs::HeldAnswerInput> = serde_json::from_str(&rewrite(&serde_json::to_string(&cmd.answers).expect("inputs serialize"), &super::with_rule_hashes_origins(rewrites)))
        .map_err(|e| mismatch("held_answers_invalid", e.to_string()))?;
    let held: Vec<_> = held.into_iter().filter(|a| a.outcome != "not_applied").collect();
    // What a suspension waits on: a dialog by id, a held request by ordinal.
    let blocker = |o: &SuspendedOperation| o.dialog.as_ref().map(|d| d.id.clone()).or_else(|| o.request.as_ref().map(|r| r.ordinal.clone()));
    let work = async {
        let mut answer = initial;
        let mut next = 0;
        let mut next_held = 0;
        let mut answered = None;
        loop {
            match answer {
                Answer::Ok(Reply::Suspended { operation }) if operation.request.is_some() => {
                    if operation.request_id != cmd.request_id { return Err(mismatch("held_parent_mismatch", "suspension belongs to another parent")) }
                    let request = operation.request.as_ref().expect("a held request");
                    if answered.as_ref() != Some(&request.ordinal) {
                        let Some(input) = held.get(next_held) else { return Err(mismatch("held_unexpected", format!("replay held {} {}, which the recording did not wait for", request.method, request.url))) };
                        let op: Op = serde_json::from_value(input.op.clone()).map_err(|e| mismatch("held_answers_invalid", e.to_string()))?;
                        if !matches!(&op, Op::Respond { ordinal, .. } | Op::FailHeld { ordinal, .. } if *ordinal == request.ordinal) {
                            return Err(mismatch("held_mismatch", format!("recorded answer {op:?}, replay holds {}", request.ordinal)));
                        }
                        let reply = answer_of(client.call(Call::Mutate { lease: Some(lease.clone()), request_id: input.request_id.clone(), op }).await);
                        if outcome_tag(&reply) != input.outcome {
                            return Err(mismatch("held_answer_mismatch", format!("{} answered {}, recorded {}", input.request_id, outcome_tag(&reply), input.outcome)));
                        }
                        next_held += 1;
                        answered = Some(request.ordinal.clone());
                    }
                }
                Answer::Ok(Reply::Suspended { operation }) => {
                    if operation.request_id != cmd.request_id { return Err(mismatch("dialog_parent_mismatch", "suspension belongs to another parent")) }
                    let Some(dialog) = operation.dialog.as_ref() else { return Err(mismatch("dialog_inputs_invalid", "suspended without a dialog or a held request")) };
                    if answered.as_ref() != Some(&dialog.id) {
                        let Some(input) = applied.get(next) else { return Err(mismatch("dialog_unexpected", "replay opened an unrecorded dialog")) };
                        let attempt = input.attempt.as_ref().ok_or_else(|| mismatch("dialog_inputs_invalid", "answered input has no attempt"))?;
                        if !same_opening(&attempt.dialog, dialog) {
                            return Err(mismatch("dialog_mismatch", format!("recorded opening {:?}, replay opening {:?}", attempt.dialog, dialog)));
                        }
                        let reply = answer_of(client.call(Call::AnswerDialog {
                            lease: lease.clone(), request_id: input.request_id.clone(), response: DialogResponse {
                                parent_request_id: cmd.request_id.clone(), dialog_id: dialog.id.clone(),
                                accept: attempt.accept, prompt_text: attempt.prompt_text.clone(),
                            },
                        }).await);
                        let Answer::Ok(Reply::DialogAnswered { receipt }) = reply else {
                            return Err(mismatch("dialog_answer_failed", format!("{}: {reply:?}", outcome_tag(&reply))));
                        };
                        let DialogInputOutcome::Answered { receipt: expected } = &input.result else { unreachable!() };
                        if !same_opening(&expected.dialog, &receipt.dialog) || expected.request_id != receipt.request_id
                            || expected.source != receipt.source || expected.accept != receipt.accept || expected.prompt_text != receipt.prompt_text
                        {
                            return Err(mismatch("dialog_answer_mismatch", "native answer differs from the recorded receipt"));
                        }
                        next += 1;
                        answered = Some(dialog.id.clone());
                    }
                }
                terminal => {
                    return if next != applied.len() { Err(mismatch("dialog_missing", "parent ended before all recorded dialogs opened")) }
                    else if next_held != held.len() { Err(mismatch("held_missing", "parent ended before all recorded held requests were waited for")) }
                    else { Ok(terminal) };
                }
            }
            // The original parent continues independently. Its durable result takes precedence over
            // a stale cached suspension while the next native dialog is being announced.
            loop {
                match answer_of(client.call(Call::Result { request_id: cmd.request_id.clone() }).await) {
                    Answer::Ok(Reply::Result { result: RequestResult::Completed { answer: terminal }, .. }) => {
                        // Journal publication precedes the owned task's final status refresh. Do not
                        // dispatch the next command while that task still owns the generation.
                        loop {
                            match answer_of(client.call(Call::Status).await) {
                                Answer::Ok(Reply::Status { status }) if status.running.is_none() => break,
                                Answer::Ok(Reply::Status { status }) if status.running.as_ref() == Some(&cmd.request_id) => {}
                                other => return Err(mismatch("dialog_parent_unavailable", format!("{other:?}"))),
                            }
                            tokio::time::sleep(Duration::from_millis(1)).await;
                        }
                        answer = *terminal; break;
                    }
                    Answer::Ok(Reply::Result { result: RequestResult::Pending { .. }, .. }) => {}
                    other => return Err(mismatch("dialog_parent_unavailable", format!("{other:?}"))),
                }
                match answer_of(client.call(Call::Status).await) {
                    Answer::Ok(Reply::Status { status }) => {
                        if let Some(operation) = status.suspension
                            && blocker(&operation).is_some() && answered != blocker(&operation)
                        { answer = Answer::Ok(Reply::Suspended { operation }); break; }
                    }
                    other => return Err(mismatch("dialog_parent_unavailable", format!("{other:?}"))),
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }
    };
    tokio::time::timeout(Duration::from_millis(watchdog_ms.saturating_add(20_000)), work).await
        .map_err(|_| mismatch("dialog_replay_timeout", "dependent replay exceeded its host service deadline"))?
}
