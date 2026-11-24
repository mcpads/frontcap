//! Re-execution inputs of a run: the launch
//! arguments and every completed state-changing request, with the resolution record of each node
//! reference it used, so the run can be executed again without its observations.
use crate::canonical::canonical_of;
use crate::ids::RequestId;
use crate::lines::{Appender, LinesError, open_append, read_complete};
use crate::profile::DeterminismProfile;
use crate::rules::RuleSet;
use crate::time::VirtualTime;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::Path;

/// Element-child indices retain the original wire representation; an explicit boundary enters
/// a shadow root (a closed one only where the observer was told of it, on the patched engine). A last
/// `{text: k}` step names a text node, the `k`th child node of the element before it (a snapshot's text
/// reference).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PathStep {
    Child(u32),
    Boundary(PathBoundary),
    Text { text: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathBoundary { ShadowRoot }

impl From<u32> for PathStep {
    fn from(index: u32) -> Self { Self::Child(index) }
}

/// Where a node reference pointed: its document instance, the element-child paths of the frame elements
/// it is shown in (from the top document inward; none in the top frame), and the path from its own
/// document's `documentElement` down to it, with the role and name the snapshot showed. Paths may
/// enter shadow roots through explicit boundary steps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resolution {
    pub document: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub frames: Vec<Vec<PathStep>>,
    pub path: Vec<PathStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The popup the node is in; none for the launch page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// The launch arguments of a run (`inputs/launch.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchInputs {
    pub url: String,
    pub profile: DeterminismProfile,
    pub rules: RuleSet,
    pub passthrough_origins: Option<Vec<String>>,
}

/// Authority that admitted a dialog answer. This is control provenance, independent of the native
/// receipt source (a lease cleanup dismissal is still an answered native dialog).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogAnswerOrigin {
    #[default]
    Agent,
    LeaseCleanup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogAttempt {
    pub origin: DialogAnswerOrigin,
    pub dialog: crate::dialog::DialogDescriptor,
    pub accept: bool,
    pub prompt_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum DialogInputOutcome {
    NotApplied,
    Answered { receipt: Box<crate::dialog::DialogReceipt> },
}

/// One admitted dependent answer, in admission order within its parent command. An intent can be
/// durable before its plan; only a not-applied attempt may lack that plan. The argument hash still
/// preserves its original journal identity. Raw frame identity remains evidence, not a replay address.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogInput {
    pub request_id: RequestId,
    pub args_sha256: String,
    pub attempt: Option<DialogAttempt>,
    pub result: DialogInputOutcome,
}

/// One completed state-changing request (`inputs/commands.jsonl`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandLine {
    pub request_id: RequestId,
    /// Virtual time when the request began.
    pub t_us: VirtualTime,
    /// The operation as the recorder received it.
    pub op: serde_json::Value,
    /// The answer's outcome tag, or `error:<code>`.
    pub outcome: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub refs: BTreeMap<String, Resolution>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dialogs: Vec<DialogInput>,
    /// Answers to held requests the page's main thread waited for while this request ran, in admission
    /// order: dependent requests, each with the operation as received and its outcome tag.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<HeldAnswerInput>,
    /// Where the controller's cancellation stopped this request's wait; a replay stops it there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancelled_at_us: Option<VirtualTime>,
}

/// An agent's answer to a held request the page waited for, given while its parent request ran.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldAnswerInput {
    pub request_id: RequestId,
    pub op: serde_json::Value,
    pub outcome: String,
}

#[derive(Debug, thiserror::Error)]
pub enum InputsError {
    #[error(transparent)]
    Lines(#[from] LinesError),
    #[error("io error at {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("{path}: {reason}")]
    Corrupt { path: String, reason: String },
}

fn commands_path(root: &Path) -> std::path::PathBuf {
    root.join("inputs/commands.jsonl")
}

fn launch_path(root: &Path) -> std::path::PathBuf {
    root.join("inputs/launch.json")
}

pub fn write_launch(root: &Path, inputs: &LaunchInputs) -> Result<(), InputsError> {
    let p = launch_path(root);
    crate::state::save_atomic(&p, inputs).map_err(|source| InputsError::Io { path: p.display().to_string(), source })
}

pub fn read_launch(root: &Path) -> Result<LaunchInputs, InputsError> {
    let p = launch_path(root);
    let text = std::fs::read_to_string(&p).map_err(|source| InputsError::Io { path: p.display().to_string(), source })?;
    serde_json::from_str(&text).map_err(|e| InputsError::Corrupt { path: p.display().to_string(), reason: e.to_string() })
}

/// Appends command lines, each synced before the request's answer is delivered.
pub struct CommandLog {
    out: Appender<File>,
    path: String,
}

impl CommandLog {
    pub fn open(root: &Path) -> Result<CommandLog, InputsError> {
        let p = commands_path(root);
        let opened = open_append(&p)?;
        Ok(CommandLog { out: Appender::new(opened.file, opened.len), path: p.display().to_string() })
    }

    pub fn append(&mut self, line: &CommandLine) -> Result<(), InputsError> {
        let io = |source| InputsError::Io { path: self.path.clone(), source };
        self.out.append_line(&canonical_of(line)).map_err(io)?;
        self.out.sync().map_err(|source| InputsError::Io { path: self.path.clone(), source })
    }
}

/// The complete command lines of a bundle, in order; none when it has no command log.
pub fn read_commands(root: &Path) -> Result<Vec<CommandLine>, InputsError> {
    let p = commands_path(root);
    if !p.exists() {
        return Ok(Vec::new());
    }
    read_complete(&p)?
        .iter()
        .enumerate()
        .map(|(i, l)| serde_json::from_str(l).map_err(|e| InputsError::Corrupt { path: format!("{}:{}", p.display(), i + 1), reason: e.to_string() }))
        .collect()
}

#[cfg(test)]
#[path = "inputs_tests.rs"]
mod tests;
