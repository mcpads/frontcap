//! Resolution of requests a dead recorder left between dispatched and completed, per command class
//! (runtime-lifecycle.md, Mutation requests). Every one ends in a definite answer.
use crate::control::Inflight;
use crate::execute::{Stage, continue_act, run_clock};
use frontcap_core::action::ActionKind;
use crate::server::Recorder;
use frontcap_core::time::VirtualTime;
use frontcap_recorder_protocol::{Answer, ErrorCode, ErrorReply, Fault, Op};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Resolve rule publication from its synced installation record.
    ResolveRules,
    /// Already applied: record the stored answer (and redo a missing page effect).
    Complete(Box<Answer>),
    /// Never reached the target.
    NotApplied,
    ContinueClock { goal: VirtualTime },
    /// Dispatch the input plan's substeps from this one (its length = only finish), then the settle.
    ResumeInput { from_substep: usize },
    /// The act's action completed; only its settle (or its answer) is left.
    ResumeSettle,
    ResumeState,
    /// A navigation dispatched and not known to have ended: committed (its action is in the trace), in
    /// flight (its document request retained), or never sent; read from the adopted generation.
    ResumeNavigation,
    /// A popup's close dispatched and not known to have ended: done when its window is gone.
    ResumeClose,
    /// An answer to a held request: whether the engine took it is read from the adopted generation (a
    /// hold still retained, or a response recorded from the trace or the engine's receipt).
    ResolveHeld { ordinal: String },
    /// An event sent on a held stream: delivered by the relaunch when its binding was written.
    ResolveSent,
    /// The evidence did not survive the fault (the engine-patch class).
    Fault { detail: String },
    /// A probe: complete it from its steps' answers, once its interrupted step is resolved.
    ResumeProbe { steps: usize },
}

pub fn decide(op: &Op, inflight: Option<&Inflight>, applied: Option<&Answer>, receipts: Option<&[usize]>) -> Decision {
    // An effect that completed saved its answer first: return it, run nothing again.
    if let Some(result) = inflight.and_then(|i| i.result.as_ref()) {
        return Decision::Complete(Box::new(result.clone()));
    }
    match op {
        // Dialog rules not saved with their answer are not in force after a relaunch (it applies the saved ones).
        Op::WatchAdd { .. } | Op::WatchRemove { .. } | Op::DialogRules { .. } | Op::RecordStart | Op::RecordStop => match applied {
            Some(a) => Decision::Complete(Box::new(a.clone())),
            None => Decision::NotApplied,
        },
        Op::SetRules { .. } | Op::EditRules { .. } => match applied {
            Some(a) => Decision::Complete(Box::new(a.clone())),
            None => Decision::ResolveRules,
        },
        Op::Respond { ordinal, .. } | Op::FailHeld { ordinal, .. } => match applied {
            Some(a) => Decision::Complete(Box::new(a.clone())),
            None => Decision::ResolveHeld { ordinal: ordinal.clone() },
        },
        Op::StreamSend { .. } => match applied {
            Some(a) => Decision::Complete(Box::new(a.clone())),
            None => Decision::ResolveSent,
        },
        Op::Advance { .. } | Op::Settle { .. } | Op::Until { .. } | Op::UntilPredicate { .. } | Op::Next { .. } => match inflight.and_then(|i| i.goal_us) {
            Some(goal) => Decision::ContinueClock { goal },
            None => Decision::Fault { detail: "the clock request's goal was not recorded".into() },
        },
        Op::Act { .. } if inflight.is_some_and(|i| i.acted.is_some()) => Decision::ResumeSettle,
        Op::Act { action, .. } => match action.kind() {
            ActionKind::Wait => match inflight.and_then(|i| i.goal_us) {
                Some(goal) => Decision::ContinueClock { goal },
                None => Decision::Fault { detail: "the wait's goal was not recorded".into() },
            },
            ActionKind::Navigation => Decision::ResumeNavigation,
            ActionKind::Close => Decision::ResumeClose,
            ActionKind::Input => match (inflight.and_then(|i| i.plan.as_ref()), receipts) {
                (Some(_), Some(r)) => Decision::ResumeInput { from_substep: r.last().map_or(0, |last| last + 1) },
                (None, _) => Decision::Fault { detail: "the input's plan was not recorded".into() },
                (_, None) => Decision::Fault { detail: "the document holding the input's receipts was replaced".into() },
            },
        },
        Op::StateApply { .. } => Decision::ResumeState,
        Op::Evaluate { .. } | Op::EvaluateInFrame { .. } => Decision::Fault { detail: "a script evaluation was interrupted; its effects leave no evidence".into() },
        Op::Stop => Decision::Complete(Box::new(Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, "stop was interrupted; stop again")))),
        Op::Probe { steps } => Decision::ResumeProbe { steps: steps.len() },
    }
}

pub async fn resolve(rec: &Arc<Recorder>) -> Result<(), crate::RecorderError> {
    let mut unresolved = {
        let journal = rec.journal.lock().unwrap();
        journal.unresolved().into_iter().filter(|id| journal.parent(id).is_none()).collect::<Vec<_>>()
    };
    // A probe completes from its steps' answers: the step it was running is resolved first.
    {
        let c = rec.control.lock().unwrap();
        unresolved.sort_by_key(|id| matches!(c.pending_ops.get(id), Some(Op::Probe { .. })));
    }
    let mut guard = rec.generation.lock().await;
    let g = guard.as_mut().expect("a relaunched recorder has a generation");
    for id in unresolved {
        *rec.running.lock().unwrap() = Some(id.clone());
        let resolution = async {
            let (op, inflight, applied) = {
                let c = rec.control.lock().unwrap();
                (c.pending_ops.get(&id).cloned(), c.inflight.clone().filter(|i| i.request_id == id), c.applied.get(&id).cloned())
            };
            let Some(op) = op else {
                let detail = format!("{id} was dispatched without a recorded operation");
                rec.push_fault(Fault { at_us: g.now(), affected: vec![id.to_string()], detail: detail.clone() })?;
                complete_recovered(rec, &id, &Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, detail))).await?;
                return Ok::<_, crate::RecorderError>(());
            };
            // An input's receipts, and an HTML drag it started, are in the window it addressed; a window
            // that closed took them with it.
            let input_window = match &op {
                Op::Act { action, .. } if action.kind() == ActionKind::Input => crate::execute::act_context(g, &op).ok().flatten(),
                _ => None,
            };
            let in_window = input_window.as_ref().is_none_or(|popup| g.enter_context(popup).is_ok());
            let receipts = match &op {
                Op::Act { action, .. } if action.kind() == ActionKind::Input && in_window => g.input_receipts(&id).await?,
                _ => None,
            };
            // An HTML drag and drop the engine ran ended with the recorder's session: nothing the new session
            // sends continues it (S17). The button is released, and the drag is a fault (engine patch 4).
            let interrupted_drag = matches!(&op, Op::Act { action, .. } if matches!(action, frontcap_core::action::Action::Drag { .. }) || action.is_pointer())
                && inflight.as_ref().is_some_and(|i| i.result.is_none() && i.acted.is_none())
                && receipts.is_some()
                && g.input_html_drag(&id).await?;
            let decision = if interrupted_drag {
                if let Some(plan) = inflight.as_ref().and_then(|i| i.plan.as_ref()) {
                    g.input_abort(&id, plan, plan.steps.len().saturating_sub(1)).await;
                }
                Decision::Fault { detail: format!("{id}: the HTML drag and drop it started ended with the recorder's session") }
            } else {
                decide(&op, inflight.as_ref(), applied.as_ref(), receipts.as_deref())
            };
            g.leave_context();
            match decision {
                Decision::ResumeState => {
                    let answer = crate::execute::continue_intervention(rec, g, &id, &op).await;
                    complete_recovered(rec, &id, &answer).await?;
                }
                Decision::Complete(answer) => {
                    if let Err(e) = crate::predicate_wait::cleanup(rec, g, &id).await {
                        let detail = rec.fail_operation(&id, g.now(), format!("predicate wait recovery cleanup failed: {e}")).await;
                        return Err(crate::RecorderError::Missing(detail));
                    }
                    redo_page_effect(rec, g, &op).await?;
                    complete_recovered(rec, &id, &answer).await?;
                }
                Decision::NotApplied => {
                    redo_page_effect(rec, g, &op).await?;
                    rec.journal.lock().unwrap().resolve_unapplied(&id)?;
                    rec.update_control(|c| {
                        c.pending_ops.remove(&id);
                        c.state_plans.remove(&id);
                        c.state_receipts.remove(&id);
                        c.pending_inputs.remove(&id);
                        if c.inflight.as_ref().is_some_and(|i| i.request_id == id) {
                            c.inflight = None;
                        }
                    })?;
                }
                Decision::ContinueClock { goal } => {
                    let quiet_since = inflight.as_ref().and_then(|i| i.quiet_since);
                    let answer = run_clock(rec, g, &id, &op, goal, quiet_since).await;
                    complete_recovered(rec, &id, &answer).await?;
                }
                Decision::ResumeInput { mut from_substep } => {
                    let i = inflight.expect("decided only with a plan");
                    let plan = i.plan.expect("decided only with a plan");
                    let start = i.start.unwrap_or_else(|| crate::execute::boundary(g));
                    // Finish the originally authorized interval first. A held move sent but not received
                    // remains explicit missing-effect evidence; it is never dispatched twice.
                    if let Some(goal) = i.goal_us {
                        g.finish_input_progress(goal).await?;
                        if let Some(step) = i.goal_step {
                            // This goal may have been persisted just before dispatch: its engine marker
                            // need not exist yet. It is now finished and must not grant time a second time.
                            from_substep = from_substep.max(step + 1);
                        }
                    }
                    // Progress markers have no page event to receive. Only native moves can be lost input. A
                    // window that closed meanwhile ends the input when it continues.
                    let unreceived = match &input_window {
                        None => g.input_unreceived(&id).await?,
                        Some(popup) => match g.enter_context(popup) {
                            Ok(()) => {
                                let unreceived = g.input_unreceived(&id).await;
                                g.leave_context();
                                unreceived?
                            }
                            Err(_) => Vec::new(),
                        },
                    };
                    let lost: Vec<_> = unreceived.into_iter().filter(|step|
                        matches!(plan.steps.get(*step), Some(frontcap_chromium::input::InputStep::Mouse { kind: frontcap_chromium::input::MouseKind::Drag, .. }))
                    ).collect();
                    if !lost.is_empty() {
                        g.record_recovery_taint(format!("{id}: substeps {lost:?} were sent before the fault and never received"))?;
                    }
                    let answer = continue_act(rec, g, &id, &op, start, Stage::Input { plan: Box::new(plan), from: from_substep }).await;
                    complete_recovered(rec, &id, &answer).await?;
                }
                Decision::ResumeSettle => {
                    let i = inflight.expect("decided only with an action's end");
                    let acted = i.acted.expect("decided only with an action's end");
                    let start = i.start.unwrap_or(acted);
                    let resume = i.goal_us.map(|goal| (goal, i.quiet_since));
                    let answer = continue_act(rec, g, &id, &op, start, Stage::Settle { acted, resume }).await;
                    complete_recovered(rec, &id, &answer).await?;
                }
                Decision::ResumeNavigation => {
                    let Op::Act { action, .. } = &op else { unreachable!("a navigation is an act") };
                    if rec.control.lock().unwrap().dismissed_navigation(&id).is_some() {
                        let answer = Answer::Err(ErrorReply::new(ErrorCode::NavigationCancelled, frontcap_chromium::ChromiumError::NavigationCancelled.to_string()));
                        complete_recovered(rec, &id, &answer).await?;
                        return Ok(());
                    }
                    let start = inflight.as_ref().and_then(|i| i.start).unwrap_or_else(|| crate::execute::boundary(g));
                    // A popup's navigation continues in its window.
                    let popup = crate::execute::act_context(g, &op).ok().flatten();
                    let resumed = if g.navigation_recorded(&id) { true } else {
                        if let Some(popup) = &popup { g.enter_context(popup)?; }
                        let resumed = g.resume_navigation(&id, action).await;
                        if popup.is_some() { g.leave_context(); }
                        resumed?
                    };
                    let stage = if resumed {
                        Stage::Settle { acted: crate::execute::boundary(g), resume: None }
                    } else {
                        // No commit and no retained document request: the browser never received it.
                        Stage::Navigate
                    };
                    let answer = continue_act(rec, g, &id, &op, start, stage).await;
                    complete_recovered(rec, &id, &answer).await?;
                }
                Decision::ResumeClose => {
                    let Op::Act { context, .. } = &op else { unreachable!("a close is an act") };
                    let popup = context.clone().unwrap_or_default();
                    let start = inflight.as_ref().and_then(|i| i.start).unwrap_or_else(|| crate::execute::boundary(g));
                    let stage = if g.resume_close(&id, &popup)? {
                        Some(Stage::Settle { acted: crate::execute::boundary(g), resume: None })
                    } else if !g.navigation_recorded(&id) {
                        // Its action is recorded before the close is sent: the browser never received it.
                        Some(Stage::Navigate)
                    } else {
                        None
                    };
                    if let Some(stage) = stage {
                        let answer = continue_act(rec, g, &id, &op, start, stage).await;
                        complete_recovered(rec, &id, &answer).await?;
                    } else {
                        // Its beforeunload may have run, and would run again: it is not sent twice.
                        let detail = format!("{id}: the close of {popup} was interrupted while the window was still open; whether its beforeunload ran is unknown");
                        rec.push_fault(Fault { at_us: g.now(), affected: vec![id.to_string()], detail: detail.clone() })?;
                        complete_recovered(rec, &id, &Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, detail))).await?;
                    }
                }
                Decision::ResolveRules => {
                    if let Some(receipt) = g.rule_receipt(&id)? {
                        let answer = crate::execute::done(&id, frontcap_recorder_protocol::Outcome::RulesInstalled(receipt));
                        rec.update_control(|c| { c.applied.insert(id.clone(), answer.clone()); })?;
                        complete_recovered(rec, &id, &answer).await?;
                    } else {
                        rec.journal.lock().unwrap().resolve_unapplied(&id)?;
                        rec.update_control(|c| {
                            c.pending_ops.remove(&id);
                            c.pending_inputs.remove(&id);
                            if c.inflight.as_ref().is_some_and(|i| i.request_id == id) { c.inflight = None; }
                        })?;
                    }
                }
                Decision::ResolveHeld { ordinal } => {
                    if g.held_ordinals().contains(&ordinal) {
                        // Retained by the engine and adopted: the answer never took effect.
                        rec.journal.lock().unwrap().resolve_unapplied(&id)?;
                        rec.update_control(|c| {
                            c.pending_ops.remove(&id);
                            if c.inflight.as_ref().is_some_and(|i| i.request_id == id) {
                                c.inflight = None;
                            }
                        })?;
                    } else if g.has_response(&ordinal)? {
                        let answer = crate::execute::done(&id, frontcap_recorder_protocol::Outcome::Ack);
                        rec.update_control(|c| { c.applied.insert(id.clone(), answer.clone()); })?;
                        complete_recovered(rec, &id, &answer).await?;
                    } else {
                        let detail = format!("held request {ordinal} is neither still held nor answered in the trace");
                        rec.push_fault(Fault { at_us: g.now(), affected: vec![id.to_string()], detail: detail.clone() })?;
                        complete_recovered(rec, &id, &Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, detail))).await?;
                    }
                }
                Decision::ResolveSent => {
                    if g.stream_sent_by(&id)? {
                        let answer = crate::execute::done(&id, frontcap_recorder_protocol::Outcome::Ack);
                        rec.update_control(|c| { c.applied.insert(id.clone(), answer.clone()); })?;
                        complete_recovered(rec, &id, &answer).await?;
                    } else {
                        rec.journal.lock().unwrap().resolve_unapplied(&id)?;
                        rec.update_control(|c| {
                            c.pending_ops.remove(&id);
                            if c.inflight.as_ref().is_some_and(|i| i.request_id == id) {
                                c.inflight = None;
                            }
                        })?;
                    }
                }
                Decision::Fault { detail } => {
                    rec.push_fault(Fault { at_us: g.now(), affected: vec![id.to_string()], detail: detail.clone() })?;
                    complete_recovered(rec, &id, &Answer::Err(ErrorReply::new(ErrorCode::RecorderFault, detail))).await?;
                }
                Decision::ResumeProbe { steps } => {
                    let answer = rec.recovered_probe(g, &id, steps).await?;
                    complete_recovered(rec, &id, &answer).await?;
                    rec.forget_probe_reads(&crate::server::probe_steps(&id, steps))?;
                }
            }
            Ok::<_, crate::RecorderError>(())
        };
        tokio::pin!(resolution);
        let result = tokio::select! {
            biased;
            result = &mut resolution => result,
            cleanup = rec.finish_or_dismiss_dialogs() => {
                match cleanup {
                    Ok(()) => resolution.await,
                    Err(detail) => {
                        let detail = rec.fail_dialog_cleanup(&detail).await;
                        Err(crate::RecorderError::Missing(detail))
                    }
                }
            }
        };
        *rec.running.lock().unwrap() = None;
        result?;
    }
    rec.refresh(g);
    Ok(())
}

/// A recovered parent can unblock before its cleanup answer receipt arrives on the control session.
async fn complete_recovered(rec: &Recorder, id: &frontcap_core::ids::RequestId, answer: &Answer) -> Result<(), crate::RecorderError> {
    rec.wait_dependents(id).await.map_err(crate::RecorderError::Missing)?;
    rec.complete(id, answer)
}

/// Brings the page's observer in line with the persisted watches after an interrupted watch change.
async fn redo_page_effect(rec: &Recorder, g: &mut frontcap_chromium::generation::Generation, op: &Op) -> Result<(), crate::RecorderError> {
    let name = match op {
        Op::WatchAdd { name, .. } | Op::WatchRemove { name } => name,
        _ => return Ok(()),
    };
    let (predicate, context) = {
        let control = rec.control.lock().unwrap();
        (control.watches.get(name).cloned(), control.watches.context(name).map(str::to_string))
    };
    match predicate {
        Some(predicate) => g.sync_page_watch_in(context.as_deref(), name, Some(&predicate)).await?,
        // Removed: wherever it was, no page keeps it.
        None => g.drop_page_watch(name).await?,
    }
    Ok(())
}

#[cfg(test)]
#[path = "recover_tests.rs"]
mod tests;
