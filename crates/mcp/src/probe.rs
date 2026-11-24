//! `state probe`: a branch, then bounded steps in the child as one recorder request. The child's recorder runs and
//! journals the steps; each answer is shaped as the step's own tool shapes it.
use crate::connections::from_reply;
use crate::dispatch::{Frontcap, Mutation, act_op, observe_op, request_id, suspended, unexpected, validate_atomicity, wait_op, watch_change};
use crate::tools::{ObserveOp, ProbeStepArgs, ToolError, WaitOp};
use frontcap_core::basis::BasisKind;
use frontcap_core::ids::{LaunchId, RequestId, SavedId, WatchName};
use frontcap_core::intervention::StateChange;
use frontcap_recorder_protocol::{Answer, MAX_PROBE_STEPS, Op, Outcome, ProbeStep, Reply};
use serde_json::{Value, json};

type ToolResult<T> = Result<T, ToolError>;

/// Which tool a step's answer is shaped as.
#[derive(Clone)]
enum Kind {
    Apply,
    Act,
    Wait,
    Read,
    Watch(WatchName),
    Input,
}

fn translate(step: ProbeStepArgs) -> ToolResult<(Kind, ProbeStep)> {
    Ok(match step {
        ProbeStepArgs::WaitFrom { step_index, after_ms } => (Kind::Wait, ProbeStep::WaitFrom {
            step_index, after_us: after_ms.checked_mul(1000).ok_or_else(|| ToolError::new(
                "invalid_arguments", "relative wait is too large", "reduce after_ms",
            ))?,
        }),
        ProbeStepArgs::Apply { changes, context, progress_ms, atomic } => {
            validate_atomicity(atomic)?;
            if progress_ms.is_some_and(|ms| ms > 10_000) {
                return Err(ToolError::new("invalid_arguments", "progress_ms is at most 10000", "grant at most 10 s per write"));
            }
            (Kind::Apply, ProbeStep::Apply { changes, context, progress_us: progress_ms.map(|ms| ms * 1000) })
        },
        ProbeStepArgs::Act { action, then, context } => (Kind::Act, ProbeStep::Mutate { op: act_op(action, then, context)? }),
        ProbeStepArgs::Wait { op: WaitOp::Now {} } => {
            return Err(ToolError::new("invalid_arguments", "now is not a probe step", "leave it out: the probe answers with the boundary it ends at"));
        }
        ProbeStepArgs::Wait { op } => (Kind::Wait, ProbeStep::Mutate { op: wait_op(op)? }),
        ProbeStepArgs::Read { op: ObserveOp::Scene { image: true, .. }, .. } => {
            return Err(ToolError::new("invalid_arguments", "a probe takes no scene images", "read the scene without image, and observe the child after the probe"));
        }
        ProbeStepArgs::Read { op, context } => (Kind::Read, ProbeStep::Observe { op: observe_op(op), context }),
        ProbeStepArgs::State { target, field, context } => (Kind::Read, ProbeStep::Observe { op: frontcap_recorder_protocol::ObserveOp::StateRead { target, field }, context }),
        ProbeStepArgs::Watch { op } => {
            let op = watch_change(op)?;
            let name = match &op {
                Op::WatchAdd { name, .. } | Op::WatchRemove { name } => name.clone(),
                _ => unreachable!("watch_change only accepts registration changes"),
            };
            (Kind::Watch(name), ProbeStep::Mutate { op })
        }
        ProbeStepArgs::Input { op } => (Kind::Input, ProbeStep::Input { op }),
    })
}

impl Frontcap {
    pub(crate) async fn probe(&self, request: Option<RequestId>, saved_id: SavedId, kind: BasisKind, changes: Vec<StateChange>, progress_ms: Option<u64>, steps: Vec<ProbeStepArgs>) -> ToolResult<Value> {
        let id = request_id(request)?;
        // Bounds are refused before anything starts.
        if steps.is_empty() || steps.len() > MAX_PROBE_STEPS {
            return Err(ToolError::new("invalid_arguments", format!("a probe has 1 to {MAX_PROBE_STEPS} steps, not {}", steps.len()),
                "send at most 32 steps; continue in the child, or save it and probe from there"));
        }
        for (index, step) in steps.iter().enumerate() {
            if let ProbeStepArgs::WaitFrom { step_index, .. } = step
                && *step_index >= index
            {
                return Err(ToolError::new("invalid_arguments", format!("step {index} refers to step {step_index}, which has not completed"),
                    "use the zero-based index of an earlier step"));
            }
        }
        let (kinds, steps): (Vec<Kind>, Vec<ProbeStep>) = steps.into_iter().map(translate).collect::<ToolResult<Vec<_>>>()?.into_iter().unzip();
        // The child has an id of its own, so a branch under the same request id is another child; the
        // probe runs in it under the caller's id, where session result finds it.
        let (launch_id, lease, mut value) = self.branch(id.derived("probe"), saved_id, kind, changes, progress_ms).await?;
        let in_child = |e: ToolError| ToolError::new(e.code, format!("in child {launch_id}: {}", e.message), e.recovery);
        let outcome = match self.mutate(&launch_id, lease, id, Op::Probe { steps }).await.map_err(in_child)? {
            Mutation::Completed(outcome) => outcome,
            Mutation::Suspended(s) => return Err(in_child(ToolError::new("internal", format!("the probe itself suspended: {s:?}"), "stop the child and report the defect"))),
        };
        let Outcome::Probed { results, completed, end, cancelled, interrupted } = outcome else {
            return Err(in_child(ToolError::new("internal", format!("unexpected outcome {outcome:?}"), "stop the child and report the defect")));
        };
        let of = kinds.len();
        let mut shaped = Vec::new();
        for (kind, answer) in kinds.into_iter().zip(results) {
            shaped.push(self.shaped(&launch_id, kind, answer).await);
        }
        let object = value.as_object_mut().expect("a branch answers an object");
        object.remove("phase");
        object.remove("clock");
        if let Some(start) = object.remove("boundary") {
            object.insert("start".into(), start);
        }
        object.insert("end".into(), json!(end));
        object.insert("completed".into(), json!(completed));
        object.insert("of".into(), json!(of));
        if cancelled {
            object.insert("cancelled".into(), json!(true));
        }
        if interrupted {
            object.insert("interrupted".into(), json!(true));
        }
        object.insert("steps".into(), Value::Array(shaped));
        // Long step results are kept as a readback; the child's identity stays inline.
        if serde_json::to_string(&value).expect("serializes").len() > crate::result_pages::INLINE_BYTES {
            value["steps"] = crate::result_pages::bound(&self.config.workspace.root, &launch_id, value["steps"].take())?;
        }
        Ok(value)
    }

    /// A step's answer as its own tool returns it; a step that failed answers its error.
    async fn shaped(&self, id: &LaunchId, kind: Kind, answer: Answer) -> Value {
        let shaped = match answer {
            Answer::Err(e) => Err(from_reply(e)),
            Answer::Ok(Reply::Suspended { operation }) => Ok(serde_json::to_value(suspended(id, operation, self.initial_date_ms(id))).expect("serializes")),
            Answer::Ok(Reply::Done { outcome: Outcome::NotApplied, request_id }) => {
                Err(ToolError::new("not_applied", format!("step {request_id} did not reach the page"), "probe again with a new request_id"))
            }
            Answer::Ok(Reply::Done { outcome, .. }) => match (kind, outcome) {
                (Kind::Act, outcome) => self.acted(id, outcome).await,
                (Kind::Wait, outcome) => self.waited(id, outcome).await,
                (Kind::Apply, Outcome::StateApplied(result)) => Ok(serde_json::to_value(result).expect("serializes")),
                (Kind::Watch(name), Outcome::Sample { value }) => Ok(json!({ "name": name, "value": value })),
                (Kind::Input, Outcome::Pointer { state, start, end }) => self.inputted(id, state, start, end).await,
                (_, other) => Err(ToolError::new("internal", format!("unexpected outcome {other:?}"), "stop the child and report the defect")),
            },
            Answer::Ok(Reply::Observed { result }) => Ok(serde_json::to_value(result).expect("serializes")),
            Answer::Ok(other) => Err(unexpected(other)),
        };
        shaped.unwrap_or_else(|e| json!({ "error": e }))
    }
}
