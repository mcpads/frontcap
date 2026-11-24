//! System-only XCTest beside an ordinary host, with separate runner and app identities.
use super::*;
use crate::{ios::{UiCall, UiReply}, ios_system_store::{Consumer, Store}, xctest::{AppProfile, RunnerLaunch, UiReady}};
use frontcap_core::native::{ios_host::Operation, ios_ui::{UiCompletion, UiIdentity, UiOperation}};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub runner: crate::ios_system_runner::Launch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_bundle_volume: Option<crate::storage_volume::Config>,
    pub python: PathBuf,
    pub script: PathBuf,
}
impl Config {
    pub(crate) fn runner_launch(&self, consumer: &str) -> RunnerLaunch {
        RunnerLaunch {
                bootstrap_app: None,
                result_bundle_volume: self.result_bundle_volume.clone(),
                xcodebuild: self.runner.xcodebuild.clone(), products: self.runner.products.clone(),
                manifest: self.runner.manifest.clone(), app: AppProfile {
                    bundle: consumer.into(), runner_bundle: "dev.frontcap.system-ui.tests.xctrunner".into(),
                    test_identifier: "SystemUITests/SystemUITests/testSystemBridge".into(),
                    ui_descriptor: "Documents/system-ui/ui-control.json".into(),
                    clock_descriptor: "Documents/unused-clock.json".into(),
                },
            }
    }
    pub fn validate(&self) -> io::Result<()> {
        self.runner.validate()?;
        if let Some(volume) = &self.result_bundle_volume { volume.validate()?; }
        if !self.python.is_absolute() || !self.script.is_absolute() {
            return Err(io::Error::other("system bridge paths must be absolute"));
        }
        Ok(())
    }
}
pub(crate) struct System {
    pub ready: UiReady,
    config: Config,
    source: String,
    directory: PathBuf,
    store: Store<Owner>,
}
#[allow(clippy::too_many_arguments)]
fn exchange(app: &mut App, config: &Config, ready: &UiReady, directory: &Path,
    name: &str, call: &UiCall, deadline: Instant, cancelled: &AtomicBool) -> io::Result<UiReply> {
    if app.entry_pending() { app.ensure_platform_running(deadline, cancelled)?; } else { app.ensure_running(deadline, cancelled)?; }
    if ready.process.has_exited()? { return Err(io::Error::other("system runner process exited")); }
    let supervision = app.simulator_mut()?.supervision();
    let reply = crate::ios_target::exchange_with_group(&config.python, &config.script, &ready.descriptor,
        &directory.join("client"), &directory.join(name), call, deadline, cancelled, supervision.as_ref())
        .map_err(|error| {
            if let crate::ios_execution::Error::Bridge { cleanup_failure: Some(failure), .. } = &error {
                app.retain_cleanup_failure(failure.clone());
            }
            io::Error::other(error)
        })?;
    let reply: UiReply = reply;
    reply.validate(call).map_err(io::Error::other)?;
    if ready.process.has_exited()? { return Err(io::Error::other("system runner exited before publication")); }
    if app.entry_pending() { app.ensure_platform_running(deadline, cancelled)?; } else { app.ensure_running(deadline, cancelled)?; }
    Ok(reply)
}
impl Target {
    pub(crate) fn enable_system(&mut self, config: Config, source: &str,
        deadline: Instant, cancelled: &AtomicBool) -> Result<(), AcquisitionError> {
        let work = (|| -> io::Result<()> {
            config.validate()?;
            if self.system.is_some() { return Err(io::Error::other("system runner already attached")); }
            let directory = self.directory.join("system-ui");
            let ready = self.app.attach_xctest(config.runner_launch(self.app.bundle()), deadline, cancelled)
                .map_err(|e| io::Error::other(e.cause))?;
            self.owner.validate_runner(&ready.identity)?;
            self.app.verify_system_runner(&ready, &config.runner.manifest, deadline, cancelled)?;
            let store = Store::create(&directory, self.owner.clone(), ready.identity.clone())?;
            save_evidence(&directory.join("config.json"), &config)?;
            save_evidence(&directory.join("ready.json"), &ready)?;
            let call = UiCall::Initialize { identity: ready.identity.clone(), source_sha256: source.into() };
            let reply = exchange(&mut self.app, &config, &ready, &directory, "initialize", &call, deadline, cancelled)?;
            save_evidence(&directory.join("initialized.json"), &reply)?;
            self.system = Some(System { ready, config, source: source.into(), directory, store });
            Ok(())
        })();
        work.map_err(|e| AcquisitionError { cause: e.to_string(), retirement: Some(self.stop()) })
    }
    /// Cached host requests are resolved before this admission. Never resurrect their old basis.
    pub(crate) fn prepare_system(&mut self, id: &RequestId, operation: &Operation) -> io::Result<()> {
        if let Operation::SetAppState {owner,state} = operation {
            return self.system.as_ref().ok_or_else(|| io::Error::other("system runner unavailable"))?.store.validate_ui(id,
                &UiOperation::SetHostAppState {bundle:self.app.bundle().into(),pid:owner.process.pid,state:*state});
        }
        if let Some(command) = operation.system_operation() {
            return self.system.as_ref().ok_or_else(|| io::Error::other("system runner unavailable"))?
                .store.validate_ui(id, &command);
        }
        if let Some(system) = &mut self.system {
            system.store.ensure_resolved()?;
            if !matches!(operation, Operation::Page { command: frontcap_core::native::ios_host::PageOperation::Read, .. } | Operation::PollEntry { .. }) {
                system.store.invalidate(id)?;
            }
        }
        Ok(())
    }
    pub(crate) fn execute_system(&mut self, id: &RequestId, operation: &UiOperation,
        deadline: Instant, cancelled: &AtomicBool) -> Result<UiCompletion, AcquisitionError> {
        let system = self.system.as_mut().ok_or_else(|| io::Error::other("system runner unavailable"))?;
        system.store.validate_ui(id, operation)?;
        let request = system.store.dispatch_ui(id.clone(), operation)?;
        self.input.capture_basis = None;
        let work = (|| -> io::Result<UiCompletion> {
            let call = UiCall::Execute { identity: system.ready.identity.clone(), app: None, request: request.clone() };
            let reply = exchange(&mut self.app, &system.config, &system.ready, &system.directory,
                &format!("exchange-{id}"), &call, deadline, cancelled)?;
            let UiReply::Completed { completion } = &reply else { return Err(io::Error::other("system UI unresolved")); };
            crate::ios_receipt::verify_system(&system.directory.join("client"), &system.ready.identity, completion)?;
            std::fs::create_dir_all(system.directory.join("results"))?;
            save_evidence(&system.directory.join("results").join(format!("{}.json",request.request_id)), &reply)?;
            system.store.complete(id, *completion.clone())?;
            Ok(*completion.clone())
        })();
        work.map_err(|e| AcquisitionError { cause:e.to_string(), retirement:Some(self.stop()) })
    }
    pub(crate) fn system_launch(&self) -> Option<(Config, String)> {
        self.system.as_ref().map(|s| (s.config.clone(), s.source.clone()))
    }
}
/// Verify frozen runner initialization and copied receipt bytes, never the current live endpoint.
pub(crate) fn read_completion(host: &Path, config: &Config, source: &str, owner: &Owner,
    runner: &UiIdentity, id: &RequestId, operation: &UiOperation) -> io::Result<Option<UiCompletion>> {
    verify_ready(host, config, source, owner, runner)?;
    let directory = host.join("system-ui");
    crate::ios_system_store::read_at(&directory.join("requests.json"), &directory.join("client"),
        &directory.join("results"), owner, runner, id, operation)
}
pub(crate) fn verify_ready(host: &Path, config: &Config, source: &str, owner: &Owner, runner: &UiIdentity) -> io::Result<()> {
    owner.validate_runner(runner)?; config.validate()?;
    let directory = host.join("system-ui");
    for name in ["ready.json", "config.json", "initialized.json", "requests.json", "client", "results"] {
        let path=directory.join(name);
        for p in [&directory, &path] {
            match std::fs::symlink_metadata(p) {
                Ok(meta) if meta.file_type().is_symlink() => return Err(io::Error::other("linked system evidence")),
                Ok(_) => (), Err(e) if e.kind()==io::ErrorKind::NotFound => (), Err(e) => return Err(e),
            }
        }
    }
    let ready: UiReady = load(&directory.join("ready.json"))?.ok_or_else(|| io::Error::other("missing system readiness"))?;
    let saved: Config = load(&directory.join("config.json"))?.ok_or_else(|| io::Error::other("missing system launch"))?;
    if ready.identity != *runner || !ready.system_only || !ready.system_alerts
        || ready.process.pid != runner.pid || serde_json::to_value(saved)? != serde_json::to_value(config)? {
        return Err(io::Error::other("saved system runner differs"));
    }
    let initialized: UiReply = load(&directory.join("initialized.json"))?.ok_or_else(|| io::Error::other("missing system initialization"))?;
    initialized.validate(&UiCall::Initialize {identity:runner.clone(),source_sha256:source.into()}).map_err(io::Error::other)?;
    Ok(())
}

#[cfg(test)]
#[path = "ios_host_system_tests.rs"]
mod tests;
