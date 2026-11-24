//! Persisted control state (`control.json`): what the recorder must know after a relaunch that neither
//! the engine nor the trace holds.
use frontcap_chromium::input::InputPlan;
use frontcap_core::ids::RequestId;
use frontcap_core::predicate::WatchRegistry;
use frontcap_core::time::VirtualTime;
use frontcap_recorder_protocol::{Answer, Fault, Op};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlState {
    #[serde(default)]
    pub suspension: Option<frontcap_recorder_protocol::SuspendedOperation>,
    /// Dependent answers have their own plans; they must never replace the parent's inflight state.
    #[serde(default)]
    pub dialog_answers: BTreeMap<RequestId, DialogAnswerPlan>,
    /// Answers to held requests the page's main thread waited for, by their dependent request id.
    #[serde(default)]
    pub held_answers: BTreeMap<RequestId, HeldAnswerPlan>,
    #[serde(default)]
    pub state_plans: BTreeMap<RequestId, frontcap_chromium::generation::StatePlan>,
    #[serde(default)]
    pub state_receipts: BTreeMap<RequestId, Vec<frontcap_core::intervention::ChangeReceipt>>,
    pub watches: WatchRegistry,
    /// Temporary watches whose removal must finish before their parent can complete.
    #[serde(default)]
    pub predicate_watches: BTreeMap<RequestId, frontcap_core::ids::WatchName>,
    /// Recorder-state requests whose change this state includes, with their answers. Written in the same
    /// atomic write as the change itself.
    pub applied: BTreeMap<RequestId, Answer>,
    /// Requests past intent whose operation recovery must know, cleared at completion.
    pub pending_ops: BTreeMap<RequestId, Op>,
    pub inflight: Option<Inflight>,
    pub faults: Vec<Fault>,
    /// Node references and recent snapshots, so references survive a relaunch within a document.
    /// They are large and change only with an observation, so they are kept in their own file
    /// (`runtime::observations_path`) and are not part of what a write of this state saves.
    #[serde(default, skip_serializing)]
    pub observations: frontcap_chromium::generation::ObservationState,
    /// Whether frames are being recorded; a relaunched recorder goes on recording.
    #[serde(default)]
    pub recording: bool,
    /// What re-execution needs of each request in `pending_ops`, taken before it dispatched.
    #[serde(default)]
    pub pending_inputs: BTreeMap<RequestId, PendingInput>,
    /// Re-execution inputs the run lacks (`missing_inputs` of the manifest).
    #[serde(default)]
    pub missing_inputs: Vec<String>,
    /// The agent's dialog rules; a relaunched recorder puts them in force again.
    #[serde(default)]
    pub dialog_rules: frontcap_core::dialog::DialogRules,
    /// A running probe's step answers that no journaled request holds (its reads), by step id, until
    /// the probe completes: a probe a fault interrupted completes from them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub probe_reads: BTreeMap<RequestId, Answer>,
}

impl ControlState {
    /// What a child restored from a process image keeps of its source's state at the image's boundary:
    /// the watches, dialog rules, recording and missing inputs. It starts with no request in progress or
    /// fault, and issues its own references.
    pub fn for_child(&self) -> ControlState {
        ControlState {
            watches: self.watches.clone(),
            recording: self.recording,
            missing_inputs: self.missing_inputs.clone(),
            dialog_rules: self.dialog_rules.clone(),
            ..ControlState::default()
        }
    }
}

/// A dependent `Respond` or `FailHeld` for a held request the page waits for, and its result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldAnswerPlan {
    pub parent: RequestId,
    pub op: Op,
    pub result: Option<Answer>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogAnswerPlan {
    #[serde(default)]
    pub origin: DialogAnswerOrigin,
    pub response: frontcap_recorder_protocol::DialogResponse,
    pub dialog: frontcap_core::dialog::DialogDescriptor,
    pub result: Option<Answer>,
}

pub use frontcap_core::inputs::DialogAnswerOrigin;

/// The start of a request, for its line in `inputs/commands.jsonl`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingInput {
    pub t_us: VirtualTime,
    pub refs: BTreeMap<String, frontcap_core::inputs::Resolution>,
    /// An upload's files as the bundle keeps them (`inputs/uploads/<sha256>/<name>`), in its order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uploads: Vec<String>,
    /// Where the controller's cancellation stopped the request's wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at_us: Option<VirtualTime>,
}

/// The clock or input request in progress, saved before each step that changes the target.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inflight {
    #[serde(default)]
    pub event_arm: Option<frontcap_chromium::generation::EventArm>,
    pub request_id: RequestId,
    pub op: Op,
    pub goal_us: Option<VirtualTime>,
    /// The input step owning `goal_us`; recovery must not finish its goal and then run that same
    /// interval again when the process died before the engine-side sent marker.
    #[serde(default)]
    pub goal_step: Option<usize>,
    pub plan: Option<InputPlan>,
    /// Where the generation stood before the request's first effect.
    #[serde(default)]
    pub start: Option<frontcap_recorder_protocol::Boundary>,
    /// The answer, saved as soon as the request's effect completed and before the journal records it;
    /// recovery returns it instead of running any part again.
    #[serde(default)]
    pub result: Option<Answer>,
    /// The start of a running settle's quiet window, saved after each of its steps; a resumed settle
    /// continues it (its end is `goal_us`).
    #[serde(default)]
    pub quiet_since: Option<VirtualTime>,
    /// Where the act's action itself ended, saved before its settle starts: recovery then resumes only
    /// the settle.
    #[serde(default)]
    pub acted: Option<frontcap_recorder_protocol::Boundary>,
}

impl ControlState {
    /// A retained answer, not disappearance of a navigation request, proves cancellation.
    pub(crate) fn dismissed_navigation(&self, parent: &RequestId) -> Option<VirtualTime> {
        self.dialog_answers.values().find_map(|plan| {
            if &plan.response.parent_request_id != parent { return None }
            match &plan.result {
                Some(Answer::Ok(frontcap_recorder_protocol::Reply::DialogAnswered { receipt }))
                    if receipt.dialog.kind == frontcap_core::event::DialogKind::Beforeunload
                        && !receipt.accept && receipt.source == frontcap_core::event::NativeDialogSource::Answered => Some(receipt.dialog.t_us),
                _ => None,
            }
        })
    }
}

#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
