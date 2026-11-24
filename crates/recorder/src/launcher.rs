//! Starting, finding, and relaunching recorders from a control server. A launch request id is scoped to
//! the workspace: its registry entry is created exclusively before anything starts, so a retried launch
//! finds the generation the first attempt created instead of starting a second one.
use crate::runtime::{self, LaunchSpec, RuntimeState};
use frontcap_core::ids::{LaunchId, RequestId, RunId};
use frontcap_core::profile::DeterminismProfile;
use frontcap_core::rules::RuleSet;
use frontcap_recorder_protocol::client::{ClientError, RecorderClient};
use frontcap_recorder_protocol::{Call, ErrorCode, ErrorReply, LeaseToken, Reply, StatusReport};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The `.frontcap` directory of a working repository.
#[derive(Debug, Clone)]
pub struct Workspace {
    pub root: PathBuf,
}

impl Workspace {
    pub fn launches(&self) -> PathBuf {
        self.root.join("launches")
    }

    pub fn runtime_dir(&self, id: &LaunchId) -> PathBuf {
        self.root.join("runtimes").join(id.as_str())
    }

    pub fn runs(&self) -> PathBuf {
        self.root.join("runs")
    }

    /// Immutable saved bases, one directory per `saved_id`.
    pub fn saved(&self) -> PathBuf {
        self.root.join("saved")
    }

    /// Process images of generations' browsers, one directory per name; only the recorder's user may enter.
    pub fn images(&self) -> PathBuf {
        self.root.join("images")
    }

    /// The workspace a recorder's runtime directory (`runtime_dir`) belongs to.
    pub fn of_runtime(dir: &Path) -> Option<Workspace> {
        Some(Workspace { root: dir.parent()?.parent()?.to_path_buf() })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub request_id: RequestId,
    pub url: String,
    pub profile: DeterminismProfile,
    pub rules: RuleSet,
    pub passthrough_origins: Option<Vec<String>>,
    pub watchdog_ms: u64,
    pub repo_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<frontcap_core::basis::BranchOrigin>,
    /// The name of a process image in the workspace's `images` to restore the generation from instead of
    /// launching it (`LaunchSpec::restore`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<String>,
}

/// A controller attached to a generation, holding its lease.
pub struct Attached {
    pub launch_id: LaunchId,
    pub client: RecorderClient,
    pub lease: LeaseToken,
    pub status: StatusReport,
    pub completed_while_detached: Vec<RequestId>,
}

impl std::fmt::Debug for Attached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attached").field("launch_id", &self.launch_id).field("status", &self.status).finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct LaunchFailure(pub ErrorReply);

impl std::fmt::Display for LaunchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.0.message, self.0.recovery)
    }
}

impl std::error::Error for LaunchFailure {}

fn failure(code: ErrorCode, message: impl Into<String>) -> LaunchFailure {
    LaunchFailure(ErrorReply::new(code, message))
}

fn io(e: std::io::Error) -> LaunchFailure {
    failure(ErrorCode::Internal, e.to_string())
}

impl From<ClientError> for LaunchFailure {
    fn from(e: ClientError) -> LaunchFailure {
        match e {
            ClientError::Recorder(reply) => LaunchFailure(reply),
            other => failure(ErrorCode::RecorderFault, other.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryEntry {
    request_id: RequestId,
    args_sha256: String,
    launch_id: LaunchId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Registered {
    New(LaunchId),
    Existing(LaunchId),
}

/// Creates the registry entry for `request_id`, or returns the one a previous attempt created. The entry
/// is written to a private file and then linked into place, so it appears complete or not at all.
pub fn register(ws: &Workspace, request_id: &RequestId, args_sha256: &str) -> Result<Registered, LaunchFailure> {
    register_inner(ws, request_id, args_sha256, false)
}

/// Native ownership cannot infer that a damaged registry means no target was started.
pub(crate) fn register_native(ws: &Workspace, request_id: &RequestId, args_sha256: &str) -> Result<Registered, LaunchFailure> {
    register_inner(ws, request_id, args_sha256, true)
}

fn register_inner(ws: &Workspace, request_id: &RequestId, args_sha256: &str, strict: bool) -> Result<Registered, LaunchFailure> {
    std::fs::create_dir_all(ws.launches()).map_err(io)?;
    let path = ws.launches().join(format!("{}.json", request_id.as_str()));
    let entry = RegistryEntry { request_id: request_id.clone(), args_sha256: args_sha256.to_string(), launch_id: LaunchId::generate() };
    let tmp = ws.launches().join(format!(".{}.{}.tmp", request_id.as_str(), entry.launch_id.as_str()));
    {
        let mut f = std::fs::File::create(&tmp).map_err(io)?;
        f.write_all(frontcap_core::canonical::canonical_of(&entry).as_bytes()).map_err(io)?;
        f.sync_all().map_err(io)?;
    }
    let outcome = loop {
        match std::fs::hard_link(&tmp, &path) {
            Ok(()) => {
                std::fs::File::open(ws.launches()).and_then(|d| d.sync_all()).map_err(io)?;
                break Ok(Registered::New(entry.launch_id.clone()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match frontcap_core::state::load::<RegistryEntry>(&path) {
                Ok(Some(existing)) if existing.args_sha256 != args_sha256 || existing.request_id != *request_id => {
                    break Err(failure(ErrorCode::RequestIdConflict, format!("launch {request_id} was used with other arguments")));
                }
                Ok(Some(existing)) => break Ok(Registered::Existing(existing.launch_id)),
                _ if strict => break Err(failure(ErrorCode::RecorderFault, "native launch registry is unreadable; preserve it and inspect the original execution")),
                // An entry is never written in place, so an unreadable one is left over from a crash of a
                // launcher that wrote in place; nothing was started for it.
                _ => {
                    let aside = ws.launches().join(format!(".{}.unreadable-{}", request_id.as_str(), entry.launch_id.as_str()));
                    if let Err(e) = std::fs::rename(&path, &aside)
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        break Err(io(e));
                    }
                }
            },
            Err(e) => break Err(io(e)),
        }
    };
    let _ = std::fs::remove_file(&tmp);
    outcome
}

/// The generation `request_id` names in the registry, if it was registered with these arguments.
/// Reading creates nothing.
pub fn registered(ws: &Workspace, request_id: &RequestId, args_sha256: &str) -> Option<LaunchId> {
    let path = ws.launches().join(format!("{}.json", request_id.as_str()));
    match frontcap_core::state::load::<RegistryEntry>(&path) {
        Ok(Some(entry)) if entry.args_sha256 == args_sha256 => Some(entry.launch_id),
        _ => None,
    }
}

/// The generation launched under `request_id`, whatever its arguments.
pub fn registered_launch(ws: &Workspace, request_id: &RequestId) -> Option<LaunchId> {
    let path = ws.launches().join(format!("{}.json", request_id.as_str()));
    frontcap_core::state::load::<RegistryEntry>(&path).ok().flatten().map(|entry| entry.launch_id)
}

pub fn run_id_of(launch: &LaunchId) -> RunId {
    RunId::new(launch.as_str().replacen("g-", "r-", 1)).expect("derived from a valid launch id")
}

pub(crate) fn spawn(recorder_bin: &Path, mode: &str, dir: &Path, env: &[(String, String)]) -> Result<(), LaunchFailure> {
    spawn_with_budget(recorder_bin, mode, dir, env, None)
}

pub(crate) fn spawn_with_budget(recorder_bin: &Path, mode: &str, dir: &Path, env: &[(String, String)],
    budget: Option<&frontcap_core::write_budget::WriteBudget>) -> Result<(), LaunchFailure> {
    use std::os::unix::process::CommandExt;
    let native = crate::native::spec_path(dir).exists();
    let log = if native {
        let mut collector = std::process::Command::new(recorder_bin);
        collector.arg("--collect-native-diagnostics");
        std::process::Stdio::from(frontcap_native::diagnostic_log::start_with_budget(&mut collector, &dir.join("recorder.log"),
            &dir.join("recorder-diagnostics"), budget).map_err(io)?)
    } else {
        std::process::Stdio::from(std::fs::OpenOptions::new().create(true).append(true)
            .open(dir.join("recorder.log")).map_err(io)?)
    };
    let mut command = std::process::Command::new(recorder_bin);
    command.args([mode, "--runtime-dir"])
        .arg(dir)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    if native {
        command.env(frontcap_native::diagnostic_log::RECORDER_ENV, recorder_bin.canonicalize().map_err(io)?);
        command.env(frontcap_native::worker_control::ROOT_ENV, dir.canonicalize().map_err(io)?);
    }
    let mut child = command.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .process_group(0)
        .spawn()
        .map_err(io)?;
    // The recorder lives as long as its generation, not as long as this process; reap it if it exits first.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

pub(crate) async fn wait_ready(dir: &Path, timeout: Duration) -> Result<(), LaunchFailure> {
    let start = std::time::Instant::now();
    loop {
        if let Some(r) = runtime::load_runtime(dir).map_err(io)? {
            match r.state {
                RuntimeState::Ready => return Ok(()),
                RuntimeState::Failed { reason, error } => return Err(LaunchFailure(error.unwrap_or_else(|| ErrorReply::new(ErrorCode::RecorderFault, reason)))),
                RuntimeState::Stopped => return Err(failure(ErrorCode::Stopped, "the generation is stopped")),
                RuntimeState::Starting if !r.recorder.is_alive() => {
                    // It may have recorded its outcome after the read above, then exited: that outcome
                    // is the answer.
                    if runtime::load_runtime(dir).map_err(io)?.is_some_and(|again| !matches!(again.state, RuntimeState::Starting)) {
                        continue;
                    }
                    return Err(failure(ErrorCode::RecorderFault, "the recorder died while starting"));
                }
                RuntimeState::Starting => {}
            }
        }
        if start.elapsed() > timeout {
            return Err(failure(ErrorCode::RecorderFault, format!("the recorder in {} did not become ready", dir.display())));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The checks a launch makes of its inputs before anything is registered or started.
pub fn validate_inputs(profile: &DeterminismProfile, passthrough_origins: Option<&[String]>) -> Result<(), LaunchFailure> {
    let invalid = |e: String| failure(ErrorCode::InvalidArguments, e);
    frontcap_core::rules::validate_passthrough_origins(passthrough_origins.unwrap_or_default()).map_err(|e| invalid(e.to_string()))?;
    frontcap_chromium::generation::validate_initial_databases(&profile.initial_indexed_db).map_err(|e| invalid(e.to_string()))?;
    frontcap_chromium::generation::validate_initial_caches(&profile.initial_caches).map_err(|e| invalid(e.to_string()))?;
    frontcap_chromium::generation::validate_initial_storage(profile.initial_storage.as_ref()).map_err(|e| invalid(e.to_string()))?;
    frontcap_chromium::dom_storage::validate_initial_dom_storage(profile).map_err(|e| invalid(e.to_string()))?;
    frontcap_chromium::generation::validate_initial_cookies(&profile.initial_cookies, profile.initial_date_ms).map_err(|e| invalid(e.to_string()))?;
    frontcap_chromium::generation::validate_locale(&profile.locale).map_err(|e| invalid(e.to_string()))?;
    Ok(())
}

/// Launches a generation, or — for a request id already in the registry — finds and attaches to the
/// generation its first attempt created.
pub async fn launch(ws: &Workspace, req: LaunchRequest, recorder_bin: &Path, env: &[(String, String)]) -> Result<Attached, LaunchFailure> {
    validate_inputs(&req.profile, req.passthrough_origins.as_deref())?;
    let args = frontcap_core::journal::args_sha256(&serde_json::to_value(&req).expect("launch requests serialize"));
    let id = match register(ws, &req.request_id, &args)? {
        Registered::New(id) | Registered::Existing(id) => id,
    };
    let dir = ws.runtime_dir(&id);
    std::fs::create_dir_all(&dir).map_err(io)?;
    match runtime::load_runtime(&dir).map_err(io)? {
        None => {
            // Nothing runs yet (or the attempt that registered this launch died before starting it). Several
            // attempts may start a recorder here; the runtime's lock admits exactly one, and the others exit
            // without touching the runtime.
            let spec = LaunchSpec {
                request_id: req.request_id.clone(),
                launch_id: id.clone(),
                run_id: run_id_of(&id),
                url: req.url.clone(),
                profile: req.profile.clone(),
                rules: req.rules.clone(),
                passthrough_origins: req.passthrough_origins.clone(),
                watchdog_ms: req.watchdog_ms,
                repo_root: req.repo_root.clone(),
                runs_root: ws.runs(),
                origin: req.origin.clone(),
                restore: req.restore.clone(),
            };
            runtime::save_spec(&dir, &spec).map_err(io)?;
            spawn(recorder_bin, "launch", &dir, env)?;
            if let Err(e) = wait_ready(&dir, Duration::from_secs(60)).await {
                // A restore that died is resolved now, so the browser it may have brought back does not
                // outlive it; its answer is the launch's failure.
                if req.restore.is_some() {
                    let _ = recover(ws, &id, recorder_bin, env).await;
                }
                return Err(e);
            }
        }
        Some(r) => match r.state {
            RuntimeState::Stopped => return Err(failure(ErrorCode::Stopped, "the generation is stopped")),
            _ if r.recorder.is_alive() => wait_ready(&dir, Duration::from_secs(60)).await?,
            _ => recover(ws, &id, recorder_bin, env).await?,
        },
    }
    reattach(ws, &id, req.request_id).await
}

/// Connects to a live recorder and takes the lease.
pub async fn reattach(ws: &Workspace, id: &LaunchId, request_id: RequestId) -> Result<Attached, LaunchFailure> {
    let dir = ws.runtime_dir(id);
    let r = runtime::load_runtime(&dir).map_err(io)?.ok_or_else(|| failure(ErrorCode::Internal, format!("no runtime {id}")))?;
    let (mut client, _) = RecorderClient::connect(&r.socket).await?;
    match client.call(Call::Reattach { request_id }).await? {
        Reply::Attached { lease, status, completed_while_detached } => {
            Ok(Attached { launch_id: id.clone(), client, lease, status: *status, completed_while_detached })
        }
        other => Err(failure(ErrorCode::Internal, format!("unexpected reply {other:?}"))),
    }
}

/// Relaunches the recorder of a generation whose recorder died while its browser lives on.
pub async fn recover(ws: &Workspace, id: &LaunchId, recorder_bin: &Path, env: &[(String, String)]) -> Result<(), LaunchFailure> {
    let dir = ws.runtime_dir(id);
    let load = || -> Result<runtime::RuntimeRecord, LaunchFailure> {
        runtime::load_runtime(&dir).map_err(io)?.ok_or_else(|| failure(ErrorCode::Internal, format!("no runtime {id}")))
    };
    let recoverable = |r: &runtime::RuntimeRecord| match &r.state {
        RuntimeState::Failed { reason, error } => Err(LaunchFailure(error.clone().unwrap_or_else(|| ErrorReply::new(ErrorCode::RecorderFault, reason.clone())))),
        RuntimeState::Stopped => Err(failure(ErrorCode::Stopped, "the generation has stopped; it cannot resume")),
        _ => Ok(()),
    };
    // A lost connection is not evidence that the recorder died; its process identity decides. A recorder
    // that is still exiting is waited out, one that is still starting is waited for, and one that answers
    // needs no relaunch.
    let start = std::time::Instant::now();
    let mut r = load()?;
    recoverable(&r)?;
    while r.recorder.is_alive() {
        match &r.state {
            RuntimeState::Ready if tokio::net::UnixStream::connect(&r.socket).await.is_ok() => return Ok(()),
            RuntimeState::Starting => return wait_ready(&dir, Duration::from_secs(60)).await,
            _ => {}
        }
        if start.elapsed() > Duration::from_secs(10) {
            return Err(failure(ErrorCode::RecorderFault, "the recorder is alive but does not answer"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        r = load()?;
        recoverable(&r)?;
    }
    // An interrupted restore is resolved by the relaunched recorder whatever is left of it.
    let restore = runtime::load_spec(&dir).ok().flatten().is_some_and(|s| s.restore.is_some());
    if !restore && (!r.browser.as_ref().is_some_and(|b| b.is_alive()) || r.endpoint.is_none()) {
        let mut gone = ErrorReply::new(ErrorCode::RecorderFault, "the recorder and its browser are gone");
        gone.recovery = "debug finalize seals its evidence; launch a new generation, with a new request id, to go on".into();
        return Err(LaunchFailure(gone));
    }
    spawn(recorder_bin, "recover", &dir, env)?;
    // The relaunched recorder writes `Starting` with its own identity first.
    let start = std::time::Instant::now();
    while runtime::load_runtime(&dir).map_err(io)?.is_some_and(|n| n.recorder == r.recorder) {
        if start.elapsed() > Duration::from_secs(10) {
            return Err(failure(ErrorCode::RecorderFault, "the relaunched recorder did not start"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    wait_ready(&dir, Duration::from_secs(60)).await
}

/// Stops only the owned process of a failed execution, without adopting or finalizing it.
/// The ownership lock excludes a living recorder (and therefore its controller lease).
pub async fn terminate_failed(ws: &Workspace, id: &LaunchId, request: &RequestId) -> Result<PathBuf, LaunchFailure> {
    let dir = ws.runtime_dir(id);
    let id = id.clone();
    let request = request.clone();
    tokio::task::spawn_blocking(move || {
        use frontcap_core::journal::{args_sha256, Begin, Journal, RequestState};
        use frontcap_recorder_protocol::{Answer, Op, Outcome};
        let _lock = crate::take_lock(&dir).map_err(|e| failure(ErrorCode::RecorderFault, e.to_string()))?
            .ok_or_else(|| failure(ErrorCode::Busy, "the recorder still owns this execution; retry stop after it exits"))?;
        let record = runtime::load_runtime(&dir).map_err(io)?.ok_or_else(|| failure(ErrorCode::RecorderFault, "missing runtime"))?;
        let spec = runtime::load_spec(&dir).map_err(io)?.ok_or_else(|| failure(ErrorCode::RecorderFault, "missing launch spec"))?;
        if record.launch_id != id || spec.launch_id != id {
            return Err(failure(ErrorCode::RecorderFault, "runtime and launch identities disagree"));
        }
        if !matches!(record.state, RuntimeState::Failed { .. }) {
            return Err(failure(ErrorCode::RecorderFault, "termination-only stop requires a failed execution"));
        }
        let browser = record.browser.ok_or_else(|| failure(ErrorCode::RecorderFault, "owned browser identity is unavailable"))?;
        let mut journal = Journal::open(&runtime::journal_path(&dir)).map_err(|e| failure(ErrorCode::RecorderFault, e.to_string()))?;
        let hash = args_sha256(&serde_json::to_value(Op::Stop).expect("stop serializes"));
        let answer = Answer::Ok(Reply::Done { request_id: request.clone(), outcome: Outcome::Stopped { finalized: false } });
        let value = serde_json::to_value(&answer).expect("answer serializes");
        let map_journal = |e: frontcap_core::journal::JournalError| failure(ErrorCode::RecorderFault, e.to_string());
        match journal.begin(&request, &hash).map_err(map_journal)? {
            Begin::Conflict => return Err(failure(ErrorCode::RequestIdConflict, "request id belongs to different arguments")),
            Begin::Completed(saved) if saved == value => return Ok(spec.runs_root.join(spec.run_id.as_str())),
            Begin::Completed(_) | Begin::NotApplied if matches!(journal.state(&request), Some(RequestState::NotApplied | RequestState::Completed(_))) => {
                return Err(failure(ErrorCode::RequestIdConflict, "request already has a different terminal result; use a new stop request id"));
            }
            _ => {}
        }
        journal.validate_completion(&request).map_err(map_journal)?;
        crate::fault::point(request.as_str(), "terminate_before_dispatch");
        if matches!(journal.state(&request), Some(RequestState::Intent)) {
            journal.dispatched(&request).map_err(map_journal)?;
        }
        crate::fault::point(request.as_str(), "terminate_after_dispatch");
        // Strict process probing must prove exit; inspection/signal failure leaves the request pending.
        browser.kill().map_err(|e| {
            let mut error = failure(ErrorCode::RecorderFault, format!("browser termination failed: {e}"));
            error.0.recovery = "resolve the process inspection or signal error, then retry stop with the same request id".into();
            error
        })?;
        crate::fault::point(request.as_str(), "terminate_after_exit");
        journal.complete(&request, value).map_err(map_journal)?;
        crate::fault::point(request.as_str(), "terminate_after_complete");
        Ok(spec.runs_root.join(spec.run_id.as_str()))
    }).await.map_err(|e| failure(ErrorCode::RecorderFault, e.to_string()))?
}

/// Seals retained failed/stopped evidence without restoring the target or claiming replayability.
pub async fn finalize_failed(ws: &Workspace, id: &LaunchId) -> Result<frontcap_core::bundle::Manifest, LaunchFailure> {
    let dir = ws.runtime_dir(id);
    let id = id.clone();
    tokio::task::spawn_blocking(move || crate::finalization::finalize(&dir, &id).map_err(|reason| {
        let mut error = failure(ErrorCode::RecorderFault, reason);
        error.0.recovery = "resolve the reported evidence or process error, then retry finalization".into();
        error
    })).await.map_err(|e| failure(ErrorCode::RecorderFault, e.to_string()))?
}

/// Reads saved operation evidence without starting a recorder or changing its journal.
/// Callers use the live recorder when available, so an in-memory request is not missed.
pub fn saved_result(ws: &Workspace, id: &LaunchId, request_id: &RequestId) -> Result<frontcap_recorder_protocol::RequestResult, LaunchFailure> {
    use frontcap_core::journal::{Journal, RequestState};
    use frontcap_recorder_protocol::RequestResult;
    let state = Journal::read_state(&runtime::journal_path(&ws.runtime_dir(id)), request_id)
        .map_err(|e| failure(ErrorCode::RecorderFault, e.to_string()))?;
    Ok(match state {
        None => RequestResult::Unknown,
        Some(RequestState::Intent | RequestState::NotApplied) => RequestResult::NotApplied,
        Some(RequestState::Dispatched) if crate::native::spec_path(&ws.runtime_dir(id)).is_file() =>
            crate::native::recovery::saved_result(&ws.runtime_dir(id), id, request_id)
                .map_err(|e| failure(ErrorCode::RecorderFault, e.to_string()))?,
        Some(RequestState::Dispatched) => RequestResult::Pending { dispatched: true },
        Some(RequestState::Completed(value)) => RequestResult::Completed {
            answer: Box::new(serde_json::from_value(value)
                .map_err(|e| failure(ErrorCode::RecorderFault, format!("invalid recorded answer for {request_id}: {e}")))?),
        },
    })
}

#[cfg(test)]
#[path = "launcher_tests.rs"]
mod tests;
