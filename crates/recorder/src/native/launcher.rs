//! Launch and reconnect to the same live native recorder; never replace an uncertain session.
use super::LaunchSpec;
use super::target;
use crate::{
    launcher::{self, LaunchFailure, Registered, Workspace},
    runtime::{self, RuntimeState},
};
use frontcap_core::{
    ids::{LaunchId, RequestId},
    journal::args_sha256,
    write_budget::{Class, WriteBudget},
};
use frontcap_recorder_protocol::{
    Call, ErrorCode, ErrorReply, LeaseToken, Reply,
    client::RecorderClient,
    native::{Reply as NativeReply, Status},
};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub request_id: RequestId,
    pub target: target::Launch,
    pub session_executable: PathBuf,
    pub acquisition_budget_ms: u64,
    /// Filesystem free-space admission floor; not a reservation or per-run quota.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_bytes: Option<u64>,
    /// Shared cooperating-writer allowance; currently enforced by recorder and controlled-service payload writers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_budget: Option<frontcap_core::write_budget::Limits>,
    /// Optional existing parent for per-launch target evidence; not cleanup authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_root: Option<PathBuf>,
    /// Automatically owned and retained UIKit session filesystem; conflicts with session_root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_volume: Option<frontcap_native::storage_volume::Config>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<super::ServiceMode>,
}
/// Trusted host configuration, separate from agent-selected operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostProfile {
    pub target: target::Launch,
    pub session_executable: PathBuf,
    pub acquisition_budget_ms: u64,
    /// Filesystem free-space admission floor; not a reservation or per-run quota.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_bytes: Option<u64>,
    /// Shared cooperating-writer allowance; currently enforced by recorder and controlled-service payload writers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_budget: Option<frontcap_core::write_budget::Limits>,
    /// Optional existing parent for per-launch target evidence; not cleanup authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_root: Option<PathBuf>,
    /// Automatically owned and retained UIKit session filesystem; conflicts with session_root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_volume: Option<frontcap_native::storage_volume::Config>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<super::ServiceMode>,
}
impl HostProfile {
    pub fn request(self, request_id: RequestId) -> LaunchRequest {
        LaunchRequest {
            request_id,
            target: self.target,
            session_executable: self.session_executable,
            acquisition_budget_ms: self.acquisition_budget_ms,
            min_free_bytes: self.min_free_bytes,
            write_budget: self.write_budget,
            session_root: self.session_root,
            session_volume: self.session_volume,
            service: self.service,
        }
    }
}

/// Register once before launch so a control server can recognize its already-held generation.
pub fn register(ws: &Workspace, request: &LaunchRequest) -> Result<LaunchId, LaunchFailure> {
    if !(1..=240_000).contains(&request.acquisition_budget_ms) {
        return Err(LaunchFailure(ErrorReply::new(
            ErrorCode::InvalidArguments,
            "native acquisition budget must be 1..240000 ms",
        )));
    }
    if let Some(mode) = &request.service {
        mode.validate().map_err(|error| LaunchFailure(ErrorReply::new(ErrorCode::InvalidArguments, error.to_string())))?;
    }
    super::validate_write_budget(request.write_budget)
        .map_err(|error| LaunchFailure(ErrorReply::new(ErrorCode::InvalidArguments, error.to_string())))?;
    super::session_volume::validate(request.session_volume.as_ref(), request.session_root.as_deref(),
        &request.target, request.min_free_bytes.unwrap_or(super::storage::DEFAULT_MIN_FREE_BYTES))
        .map_err(|error| LaunchFailure(ErrorReply::new(ErrorCode::InvalidArguments, error.to_string())))?;
    let hash =
        args_sha256(&serde_json::to_value(("native", request)).expect("launch serializes"));
    Ok(
        match launcher::register_native(ws, &request.request_id, &hash)? {
            Registered::New(id) | Registered::Existing(id) => id,
        },
    )
}

pub struct Attached {
    pub launch_id: LaunchId,
    pub client: RecorderClient,
    pub lease: LeaseToken,
    pub status: Status,
    pub completed_while_detached: Vec<RequestId>,
}
fn failure(detail: impl ToString) -> LaunchFailure {
    LaunchFailure(ErrorReply::new(
        ErrorCode::RecorderFault,
        detail.to_string(),
    ))
}

pub fn begin(
    ws: &Workspace,
    request: LaunchRequest,
    recorder_bin: &Path,
    env: &[(String, String)],
) -> Result<LaunchId, LaunchFailure> {
    if !(1..=240_000).contains(&request.acquisition_budget_ms) {
        return Err(LaunchFailure(ErrorReply::new(
            ErrorCode::InvalidArguments,
            "native acquisition budget must be 1..240000 ms",
        )));
    }
    // Target discrimination shares the workspace registry with Chromium, but not its argument hash.
    let id = register(ws, &request)?;
    let dir = ws.runtime_dir(&id);
    std::fs::create_dir_all(&dir).map_err(failure)?;
    match runtime::load_runtime(&dir).map_err(failure)? {
        None => {
            if dir.join("native-session").exists() || runtime::spec_path(&dir).exists() {
                return Err(failure(
                    "native runtime has prior or conflicting ownership evidence",
                ));
            }
            let spec = LaunchSpec {
                request_id: request.request_id.clone(),
                launch_id: id.clone(),
                run_id: launcher::run_id_of(&id),
                target: request.target,
                session_executable: request.session_executable,
                acquisition_budget_ms: request.acquisition_budget_ms,
                min_free_bytes: request.min_free_bytes,
                write_budget: request.write_budget,
                session_root: request.session_root,
                session_volume: request.session_volume,
                service: request.service,
            };
            // Equivalent retries reuse the admitted spec. The recorder lock admits only one launch.
            publish_spec(&dir, &spec).map_err(failure)?;
            let budget = spec.write_budget.map(|limits| {
                WriteBudget::open(&dir.join("native-write-budget"), id.clone(), limits)
            }).transpose().map_err(failure)?;
            launcher::spawn_with_budget(recorder_bin, "launch", &dir, env, budget.as_ref())?;
        }
        Some(record) => {
            if record.launch_id != id {
                return Err(failure("native runtime launch identity differs"));
            }
            if let RuntimeState::Failed { reason, .. } = &record.state {
                return Err(failure(reason));
            }
            if matches!(record.state, RuntimeState::Stopped) || !record.recorder.is_alive() {
                return Err(failure(
                    "native execution is terminal or its recorder is gone; inspect saved results and retirement evidence",
                ));
            }
        }
    }
    Ok(id)
}

/// Serialize preparation across equivalent launcher retries; never reset a partial allowance.
pub(super) fn publish_spec(dir: &Path, spec: &LaunchSpec) -> std::io::Result<()> {
    let lock = std::fs::File::options().read(true).write(true).create(true).truncate(false)
        .open(dir.join("native-spec.lock"))?;
    lock.lock()?;
    let path = super::spec_path(dir);
    let budget_path = dir.join("native-write-budget");
    if let Some(existing) = frontcap_core::state::load::<LaunchSpec>(&path)? {
        if serde_json::to_vec(&existing)? != serde_json::to_vec(spec)? {
            return Err(std::io::Error::other("native launch specification differs"));
        }
        if let Some(limits) = spec.write_budget {
            WriteBudget::open(&budget_path, spec.launch_id.clone(), limits)?;
        }
        return Ok(());
    }
    let budget = spec.write_budget.map(|limits| {
        if budget_path.try_exists()? {
            WriteBudget::open(&budget_path, spec.launch_id.clone(), limits)
        } else {
            WriteBudget::create(&budget_path, spec.launch_id.clone(), limits)
        }
    }).transpose()?;
    super::save_budgeted(budget.as_ref(), Class::Ordinary, &path, spec)
}

pub async fn launch(
    ws: &Workspace,
    request: LaunchRequest,
    recorder_bin: &Path,
    env: &[(String, String)],
) -> Result<Attached, LaunchFailure> {
    let budget = request.acquisition_budget_ms;
    let request_id = request.request_id.clone();
    let id = begin(ws, request, recorder_bin, env)?;
    let dir = ws.runtime_dir(&id);
    // Waiting is observational; timeout never launches another recorder/target.
    launcher::wait_ready(&dir, Duration::from_millis(budget) + Duration::from_secs(15)).await?;
    reattach(ws, &id, request_id).await
}

pub async fn reattach(
    ws: &Workspace,
    id: &LaunchId,
    request_id: RequestId,
) -> Result<Attached, LaunchFailure> {
    let dir = ws.runtime_dir(id);
    let record = runtime::load_runtime(&dir)
        .map_err(failure)?
        .ok_or_else(|| failure("native runtime is missing"))?;
    if &record.launch_id != id {
        return Err(failure("native runtime launch identity differs"));
    }
    if !super::spec_path(&dir).is_file() || !record.recorder.is_alive() {
        return Err(failure(
            "native recorder is not live; use saved-result lookup without replay",
        ));
    }
    let (mut client, hello) = RecorderClient::connect(&record.socket).await?;
    if hello
        != (Reply::Hello {
            launch_id: id.clone(),
        })
    {
        return Err(failure("native socket launch identity differs"));
    }
    match client.call(Call::Reattach { request_id }).await? {
        Reply::Native {
            body: NativeReply::Attached { status, .. },
        } if status.launch_id != *id || status.stopped || !status.lease_held => Err(failure(
            "native attach status does not bind a live lease to the requested generation",
        )),
        Reply::Native {
            body:
                NativeReply::Attached {
                    lease,
                    status,
                    completed_while_detached,
                },
        } => Ok(Attached {
            launch_id: id.clone(),
            client,
            lease,
            status: *status,
            completed_while_detached,
        }),
        other => Err(failure(format!(
            "unexpected native attach reply: {other:?}"
        ))),
    }
}

#[cfg(test)]
#[path = "launcher_tests.rs"]
mod tests;
