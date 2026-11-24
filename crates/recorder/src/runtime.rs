//! Files of one runtime directory: the launch arguments, the recorder's own record, and the socket.
use frontcap_chromium::generation::Endpoint;
use frontcap_core::ids::{LaunchId, RequestId, RunId};
use frontcap_core::process::ProcessIdentity;
use frontcap_core::profile::DeterminismProfile;
use frontcap_core::rules::RuleSet;
use frontcap_core::state::{load, save_atomic};
use serde::{Deserialize, Serialize};
use frontcap_recorder_protocol::ErrorReply;
use std::path::{Path, PathBuf};

/// Launch arguments, written by the launcher before the recorder starts (`spec.json`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSpec {
    pub request_id: RequestId,
    pub launch_id: LaunchId,
    pub run_id: RunId,
    pub url: String,
    pub profile: DeterminismProfile,
    pub rules: RuleSet,
    pub passthrough_origins: Option<Vec<String>>,
    pub watchdog_ms: u64,
    pub repo_root: PathBuf,
    pub runs_root: PathBuf,
    /// The saved basis a branch child is constructed from; kept in its bundle as `origin.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<frontcap_core::basis::BranchOrigin>,
    /// The name of a process image (`Call::Image`) in the workspace's `images` the generation is restored
    /// from instead of launched: its browser is the image's, continued from the image's boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeState {
    Starting,
    Ready,
    Stopped,
    Failed {
        reason: String,
        /// Typed startup outcome; old runtime records contain only reason.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorReply>,
    },
}

/// The recorder's record of its generation (`runtime.json`), written only by the recorder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeRecord {
    pub launch_id: LaunchId,
    pub state: RuntimeState,
    pub recorder: ProcessIdentity,
    pub browser: Option<ProcessIdentity>,
    pub endpoint: Option<Endpoint>,
    pub socket: PathBuf,
}

pub fn spec_path(dir: &Path) -> PathBuf {
    dir.join("spec.json")
}

pub fn runtime_path(dir: &Path) -> PathBuf {
    dir.join("runtime.json")
}

pub fn control_path(dir: &Path) -> PathBuf {
    dir.join("control.json")
}

/// The control state's observations (`ControlState::observations`).
pub fn observations_path(dir: &Path) -> PathBuf {
    dir.join("observations.json")
}

/// The proxy authority's keys, so a relaunched recorder's proxy presents what the browser trusts.
pub fn proxy_keys_path(dir: &Path) -> PathBuf {
    dir.join("proxy-keys.json")
}

pub fn journal_path(dir: &Path) -> PathBuf {
    dir.join("journal.jsonl")
}

pub fn save_spec(dir: &Path, spec: &LaunchSpec) -> std::io::Result<()> {
    save_atomic(&spec_path(dir), spec)
}

pub fn load_spec(dir: &Path) -> std::io::Result<Option<LaunchSpec>> {
    load(&spec_path(dir))
}

pub fn save_runtime(dir: &Path, record: &RuntimeRecord) -> std::io::Result<()> {
    save_atomic(&runtime_path(dir), record)
}

pub fn load_runtime(dir: &Path) -> std::io::Result<Option<RuntimeRecord>> {
    load(&runtime_path(dir))
}

/// Records a failure in `runtime.json`, keeping whatever else it says.
pub fn mark_failed(dir: &Path, reason: &str) -> std::io::Result<()> {
    mark_failed_with_error(dir, reason, None)
}

pub(crate) fn mark_failed_with_error(dir: &Path, reason: &str, error: Option<ErrorReply>) -> std::io::Result<()> {
    mark_failed_with_budget(dir, reason, error, None)
}

pub(crate) fn mark_failed_with_budget(
    dir: &Path, reason: &str, error: Option<ErrorReply>,
    budget: Option<&frontcap_core::write_budget::WriteBudget>,
) -> std::io::Result<()> {
    if let Some(mut r) = load_runtime(dir)? {
        let error = if matches!(r.state, RuntimeState::Starting) { error } else { None };
        r.state = RuntimeState::Failed { reason: reason.to_string(), error };
        match budget {
            Some(budget) => budget.save_atomic(frontcap_core::write_budget::Class::Retirement, &runtime_path(dir), &r)?,
            None => save_runtime(dir, &r)?,
        }
    }
    Ok(())
}

/// Unix socket paths are limited (104 bytes on macOS). A runtime directory too deep for one gets its
/// socket in `/tmp`, named from a digest of the runtime directory.
pub fn socket_path(dir: &Path) -> PathBuf {
    let inside = dir.join("recorder.sock");
    if inside.as_os_str().len() <= 100 {
        return inside;
    }
    let digest = frontcap_core::canonical::sha256_hex(&dir.display().to_string());
    PathBuf::from("/tmp").join(format!("frontcap-{}.sock", &digest[..16]))
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;
