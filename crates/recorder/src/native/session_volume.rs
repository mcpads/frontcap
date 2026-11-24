//! Recorder-held authority for a bounded UIKit session; records never recreate a volume owner.
use super::*;
use frontcap_native::storage_volume::{Config, Worker};
use std::{fs, os::unix::fs::MetadataExt};

pub(super) fn validate(config: Option<&Config>, root: Option<&Path>, target: &target::Launch, minimum: u64) -> io::Result<()> {
    if let target::Launch::Ios(target) = target { target.validate_resources()?; }
    let Some(config) = config else { return Ok(()); };
    config.validate()?;
    if root.is_some() || !matches!(target, target::Launch::Ios(value)
        if value.surface == frontcap_native::ios_execution::UiSurface::Uikit) {
        return Err(io::Error::other("session volume requires UIKit and no external session_root"));
    }
    if minimum >= u64::from(config.size_mib - config.reserve_mib) * 1024 * 1024 {
        return Err(io::Error::other("session volume free-space floor must leave room within its capacity"));
    }
    if let target::Launch::Ios(target) = target
        && target.xctest.simulator.data_volume.is_some()
        && target.xctest.simulator.data_volume_root.is_none() {
        return Err(io::Error::other("managed session and Simulator data volumes require a separate data_volume_root; nested image mounts are unsupported"));
    }
    Ok(())
}

pub(super) struct SessionVolume {
    worker: Worker,
    pub session: PathBuf,
    launch_id: LaunchId,
}
impl SessionVolume {
    pub fn start(runtime: &Path, launch: &LaunchId, config: &Config, deadline: Instant,
        budget: Option<&WriteBudget>, minimum: u64) -> io::Result<Self> {
        storage::admit(runtime, u64::from(config.size_mib) * 1024 * 1024 + minimum)?;
        // Publish the immutable plan before the new filesystem or its writers exist.
        save_budgeted(budget, Class::Ordinary, &runtime.join("native-session-volume-plan.json"), config)?;
        let worker = Worker::start(config, &runtime.join("native-session-volume"), deadline)?;
        if fs::symlink_metadata(&worker.mount)?.dev() == fs::symlink_metadata(runtime)?.dev() {
            return Err(io::Error::other("session volume did not establish a separate filesystem"));
        }
        Ok(Self { session: worker.mount.join(launch.as_str()), worker, launch_id: launch.clone() })
    }
    pub fn root(&self) -> &Path { &self.worker.mount }
    pub fn watch_uikit(&mut self, session: &target::Session, deadline: Instant) -> io::Result<()> {
        let target::Session::Ios(session) = session else { return Err(io::Error::other("UIKit session required for volume ownership watch")); };
        self.worker.watch_uikit(&self.session, &session.worker_identity()?, session.retirement_reader(), deadline)
    }
    pub fn watch_acquisition(&mut self, session: &frontcap_native::ios_session::Session, deadline: Instant) -> io::Result<()> {
        self.worker.watch_uikit_acquisition(&self.session, &session.worker_identity()?, session.retirement_reader(), deadline)
    }
    /// The recorder has not dispatched Session::start; no consumer writer has been acquired.
    fn retire_unstarted(self, runtime: &Path, deadline: Instant, budget: Option<&WriteBudget>) -> io::Result<()> {
        match fs::symlink_metadata(&self.session) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
            Ok(_) => return Err(io::Error::other("unstarted session volume unexpectedly contains a session; retain original owner evidence")),
        }
        let volume = self.worker.retire(deadline)?;
        save_budgeted(budget, Class::Retirement, &runtime.join("native-session-volume-acquisition-retirement.json"),
            &UnstartedCompletion { launch_id: self.launch_id, volume })
    }

    /// Fresh session metadata failed before diagnostic/session worker creation was attempted.
    pub fn retire_metadata_failure(self, runtime: &Path, deadline: Instant, budget: Option<&WriteBudget>) -> io::Result<()> {
        let volume = self.worker.retire_compressed(deadline)?;
        save_budgeted(budget, Class::Retirement, &runtime.join("native-session-volume-metadata-failure-retirement.json"),
            &UnstartedCompletion { launch_id: self.launch_id, volume })
    }

    /// The live startup outcome proves no session worker was spawned. Diagnostic writers still
    /// require their own EOF/exit proof before retaining this partial session image.
    pub fn retire_failed_start(self, runtime: &Path, deadline: Instant, budget: Option<&WriteBudget>) -> io::Result<()> {
        wait_for_diagnostics(&self.session, deadline)?;
        let volume = self.worker.retire_compressed(deadline)?;
        save_budgeted(budget, Class::Retirement, &runtime.join("native-session-volume-start-failure-retirement.json"),
            &UnstartedCompletion { launch_id: self.launch_id, volume })
    }

    /// Failed diagnostic setup returned original-child exit/no-spawn authority, before any
    /// session worker could inherit a writer. Stream completeness is not asserted.
    pub fn retire_diagnostic_failure(self, runtime: &Path, collector_pid: Option<u32>, deadline: Instant,
        budget: Option<&WriteBudget>) -> io::Result<()> {
        let volume = self.worker.retire_compressed(deadline)?;
        save_budgeted(budget, Class::Retirement, &runtime.join("native-session-volume-diagnostic-failure-retirement.json"),
            &DiagnosticFailureCompletion { launch_id: self.launch_id, collector_pid, volume })
    }

    /// Only the stopped original worker can supply pre-acquisition authority.
    pub fn retire_before_acquisition(self, runtime: &Path, worker: ProcessIdentity, deadline: Instant,
        budget: Option<&WriteBudget>) -> io::Result<()> {
        if !worker.has_exited()? { return Err(io::Error::other("pre-acquisition session worker remains live")); }
        wait_for_diagnostics(&self.session, deadline)?;
        let volume = self.worker.retire_compressed(deadline)?;
        save_budgeted(budget, Class::Retirement, &runtime.join("native-session-volume-before-acquisition-retirement.json"),
            &BeforeAcquisitionCompletion { launch_id: self.launch_id, worker, volume })
    }

    /// Acquisition failed, but the original session has since stopped successfully.
    pub fn retire_failed_acquisition(self, runtime: &Path,
        retirement: frontcap_core::native::ownership::Retirement, deadline: Instant,
        budget: Option<&WriteBudget>) -> io::Result<()> {
        if !retirement.passed { return Err(io::Error::other("failed acquisition target retirement is unproven")); }
        wait_for_diagnostics(&self.session, deadline)?;
        let volume = self.worker.retire_compressed(deadline)?;
        save_budgeted(budget, Class::Retirement,
            &runtime.join("native-session-volume-failed-acquisition-retirement.json"),
            &FailedAcquisitionCompletion { launch_id: self.launch_id, retirement, volume })
    }

    /// Only called after Session::stop returns passing target retirement and reaps its worker.
    pub fn retire(self, runtime: &Path, request_id: &RequestId, retirement: &frontcap_core::native::ownership::Retirement, deadline: Instant, budget: Option<&WriteBudget>) -> io::Result<()> {
        wait_for_diagnostics(&self.session, deadline)?;
        let evidence = self.worker.retire_compressed(deadline)?;
        save_budgeted(budget, Class::Retirement, &runtime.join("native-session-volume-retirement.json"), &Completion {
            launch_id: self.launch_id, request_id: request_id.clone(), retirement: retirement.clone(), volume: evidence,
        })
    }
}

/// Only callers that have not dispatched target acquisition may dispose of this volume.
pub(super) async fn retire_after_preparation_failure(volume: Option<SessionVolume>, runtime: &Path,
    error: io::Error, budget: Option<&WriteBudget>) -> io::Error {
    let Some(volume) = volume else { return error; };
    let root = runtime.to_owned(); let budget = budget.cloned();
    let retired = tokio::task::spawn_blocking(move || volume.retire_unstarted(
        &root, Instant::now() + Duration::from_secs(30), budget.as_ref())).await;
    let disposition = match retired {
        Ok(Ok(())) => "unused session volume retired".to_owned(),
        Ok(Err(cleanup)) => format!("unused session volume retirement unresolved: {cleanup}"),
        Err(cleanup) => format!("unused session volume retirement worker failed: {cleanup}"),
    };
    io::Error::other(format!("native preparation failed: {error}; {disposition}"))
}

/// Separate from a Stop completion: this proves disposal before target acquisition only.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnstartedCompletion {
    launch_id: LaunchId,
    volume: frontcap_native::storage_volume::RetirementEvidence,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DiagnosticFailureCompletion {
    launch_id: LaunchId,
    collector_pid: Option<u32>,
    volume: frontcap_native::storage_volume::RetirementEvidence,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BeforeAcquisitionCompletion {
    launch_id: LaunchId,
    worker: ProcessIdentity,
    volume: frontcap_native::storage_volume::RetirementEvidence,
}

/// Terminal failed acquisition, never a successful launch or a request-bound Stop.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FailedAcquisitionCompletion {
    launch_id: LaunchId,
    retirement: frontcap_core::native::ownership::Retirement,
    volume: frontcap_native::storage_volume::RetirementEvidence,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    launch_id: LaunchId,
    request_id: RequestId,
    retirement: frontcap_core::native::ownership::Retirement,
    volume: frontcap_native::storage_volume::RetirementEvidence,
}

pub(super) fn saved_acquisition_target(runtime: &Path, spec: &LaunchSpec) -> io::Result<Option<frontcap_core::native::ownership::Retirement>> {
    owner_loss::acquisition_target(runtime, spec)
}

pub(super) fn saved_stop(runtime: &Path, spec: &LaunchSpec, id: &RequestId) -> io::Result<Option<frontcap_core::native::ownership::Retirement>> {
    let Some(record): Option<Completion> = load(&runtime.join("native-session-volume-retirement.json"))? else { return Ok(None); };
    let config = spec.session_volume.as_ref().ok_or_else(|| io::Error::other("session volume configuration missing"))?;
    let plan: Config = load(&runtime.join("native-session-volume-plan.json"))?
        .ok_or_else(|| io::Error::other("session volume plan missing"))?;
    if &plan != config || record.launch_id != spec.launch_id || &record.request_id != id || !record.retirement.passed {
        return Err(io::Error::other("session volume completion differs from original launch/Stop"));
    }
    frontcap_native::storage_volume::inspect_retained(&runtime.join("native-session-volume"), config, &record.volume)?;
    Ok(Some(record.retirement))
}

fn wait_for_diagnostics(session: &Path, deadline: Instant) -> io::Result<()> {
    // This checks the original session collector. For opt-in independent resources, the registered
    // volume owner additionally waits for group/supervisor diagnostic writers before detachment.
    let root = session.join("worker.log.diagnostics");
    let mut entries = fs::read_dir(&root)?;
    let directory = entries.next().ok_or_else(|| io::Error::other("session diagnostic owner missing"))??.path();
    if entries.next().is_some() || !fs::symlink_metadata(&directory)?.is_dir() {
        return Err(io::Error::other("session diagnostic ownership is ambiguous"));
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Log { log: PathBuf }
    let config: Log = load(&directory.join("config.json"))?
        .ok_or_else(|| io::Error::other("session diagnostic configuration missing"))?;
    if config.log != session.join("worker.log") {
        return Err(io::Error::other("session diagnostic log differs from owned session"));
    }
    let process: ProcessIdentity = load(&directory.join("ready.json"))?
        .ok_or_else(|| io::Error::other("session diagnostic identity missing"))?;
    loop {
        if process.has_exited()? {
            let result: frontcap_native::diagnostic_log::Summary = load(&directory.join("result.json"))?
                .ok_or_else(|| io::Error::other("session diagnostic terminal proof missing"))?;
            if !result.eof { return Err(io::Error::other("session diagnostic EOF unconfirmed")); }
            // A retained-stream error remains in the original summary; EOF and process exit
            // establish writer retirement, not diagnostic completeness.
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "session diagnostic writer remains live"));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
#[path = "session_volume_tests.rs"]
mod tests;

#[path = "session_volume_artifact.rs"]
pub(super) mod artifact;

#[path = "session_volume_owner_loss.rs"]
mod owner_loss;
