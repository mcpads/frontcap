//! Read-only acquisition evidence. No lease, target readiness or effect is manufactured.
use super::LaunchSpec;
use crate::runtime::{self, RuntimeRecord};
use frontcap_core::{ids::LaunchId, native::ownership::Retirement, process::ProcessIdentity, state::load};
use frontcap_service_fixture::{Record, owned::Binding};
use serde::Serialize;
use std::{io, path::Path};

#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub launch_id: LaunchId,
    /// None means not yet published, not proof that no process was started.
    pub runtime: Option<RuntimeRecord>,
    /// OS identity inspection, distinct from persisted runtime state.
    pub recorder_alive: Option<bool>,
    pub service: Option<Record>,
    /// Original target only; not authority to dispose of service/session-volume storage.
    pub target_retirement: TargetRetirement,
}

#[derive(Debug, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TargetRetirement {
    Pending,
    Recorded { retirement: Retirement },
    Unavailable { reason: String },
}

fn target_retirement(directory: &Path, spec: &LaunchSpec, recorder_alive: Option<bool>) -> TargetRetirement {
    if recorder_alive != Some(false) { return TargetRetirement::Pending; }
    let inspect = || -> io::Result<Option<Retirement>> {
        if let Some(retirement) = super::session_volume::saved_acquisition_target(directory, spec)? {
            return Ok(Some(retirement));
        }
        let session = super::session_directory(directory)?;
        let worker_path = if matches!(spec.target, super::target::Launch::Flutter(_)) {
            session.join("session/worker-process.json")
        } else { session.join("worker.json") };
        let Some(worker): Option<ProcessIdentity> = load(&worker_path)? else { return Ok(None); };
        // A published result and a released session lock may precede process exit.
        if !worker.has_exited()? { return Ok(None); }
        use super::target::Launch;
        Ok(match &spec.target {
            Launch::IosHost(_) | Launch::Webview(_) =>
                frontcap_native::webview_session::inspect(&session)?.retirement(),
            Launch::AndroidHost(_) | Launch::AndroidWebview(_) =>
                frontcap_native::android_webview_session::inspect(&session)?.retirement(),
            Launch::Android(_) => match frontcap_native::session::inspect(&session)? {
                frontcap_native::session::Recovery::Finished { result, .. } => result.retirement,
                _ => None,
            },
            Launch::Ios(_) => frontcap_native::ios_session::inspect(&session)?.retirement(),
            Launch::Flutter(_) => match frontcap_native::flutter_session::inspect(&session.join("session"))? {
                frontcap_native::flutter_session::Recovery::Finished(result) => Some(result.retirement),
                _ => None,
            },
        })
    };
    match inspect() {
        Ok(Some(retirement)) => TargetRetirement::Recorded { retirement },
        Ok(None) => TargetRetirement::Pending,
        Err(error) => TargetRetirement::Unavailable { reason: error.to_string().chars().take(512).collect() },
    }
}

pub fn inspect(directory: &Path, launch: &LaunchId) -> io::Result<Snapshot> {
    let spec: LaunchSpec = load(&super::spec_path(directory))?
        .ok_or_else(|| io::Error::other("native launch specification missing"))?;
    if &spec.launch_id != launch {
        return Err(io::Error::other("native preparation launch differs"));
    }
    // Publication uses separate atomic files. Retry a phase transition rather than combining owners.
    for _ in 0..3 {
        let runtime = runtime::load_runtime(directory)?;
        if runtime
            .as_ref()
            .is_some_and(|record| &record.launch_id != launch)
        {
            return Err(io::Error::other("native runtime launch differs"));
        }
        let binding: Option<Binding> = load(&directory.join("service/owner.json"))?;
        let service: Option<Record> = load(&directory.join("service/state.json"))?;
        if runtime != runtime::load_runtime(directory)? {
            continue;
        }
        let service = match (binding, service) {
            (Some(binding), Some(service)) => {
                if spec.service.is_none()
                    || &binding.owner != launch
                    || binding.service != service.generation
                    || runtime
                        .as_ref()
                        .is_none_or(|record| record.recorder.pid != service.pid)
                {
                    return Err(io::Error::other("service preparation ownership differs"));
                }
                Some(service)
            }
            // The service state is written before its owner binding. Do not expose it as owned yet.
            (None, _) => None,
            (Some(_), None) => return Err(io::Error::other("owned service state missing")),
        };
        let recorder_alive = runtime
            .as_ref()
            .map(|record| record.recorder.has_exited().map(|gone| !gone))
            .transpose()?;
        return Ok(Snapshot {
            launch_id: launch.clone(),
            runtime,
            recorder_alive,
            service,
            target_retirement: target_retirement(directory, &spec, recorder_alive),
        });
    }
    Err(io::Error::other(
        "native preparation changed during observation; read again",
    ))
}
