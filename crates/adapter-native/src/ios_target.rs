//! Own XCTest, initial app acquisition and stored requests as one retirement boundary.
use crate::{
    device::AcquisitionError,
    ios::{ClockCall, ClockReply, UiCall, UiReply},
    ios_execution::{Config, Error, Executor},
    ios_store::{Completion, Operation, Owner},
    process::exchange_typed_recorded,
    xctest::{Xctest, XctestLaunch},
};
use frontcap_core::{
    ids::RequestId,
    native::{
        ios_ui::{UiOperation, UiOutcome, UiRequest},
        ownership::Retirement,
        request::canonical_uuid,
    },
    state::save_atomic,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::AtomicBool,
    time::Instant,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IosLaunch {
    /// Opt-in independent resource ownership; includes acquisition and target use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_lifetime_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub app_environment: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_url_environment: Option<String>,
    #[serde(default)]
    pub surface: crate::ios_execution::UiSurface,
    pub xctest: XctestLaunch,
    pub python: PathBuf,
    pub ui_script: PathBuf,
    pub clock_script: PathBuf,
    pub controller_id: String,
}
impl IosLaunch {
    pub fn validate_resources(&self) -> io::Result<()> {
        if self.resource_lifetime_ms.is_some_and(|value| value == 0 || value > 900_000) {
            return Err(io::Error::other("UIKit resource lifetime must be 1..900000 milliseconds"));
        }
        if self.xctest.simulator.data_volume.is_some() && self.resource_lifetime_ms.is_none() {
            return Err(io::Error::other("UIKit Simulator data volume requires an explicit resource lifetime"));
        }
        self.xctest.simulator.validate_data_volume()?;
        Ok(())
    }
    pub fn validate_app_environment(&self) -> io::Result<()> {
        self.validate_resources()?;
        use frontcap_core::native::ios_ui::{validate_launch_environment, valid_app_environment_key};
        validate_launch_environment(&self.app_environment).map_err(io::Error::other)?;
        if self.service_url_environment.as_ref().is_some_and(|key| !valid_app_environment_key(key))
            || (self.surface != crate::ios_execution::UiSurface::Uikit
                && (!self.app_environment.is_empty() || self.service_url_environment.is_some()))
        {
            return Err(io::Error::other("app environment requires a UIKit profile and a valid key"));
        }
        Ok(())
    }
}
pub struct Target {
    xctest: Xctest,
    executor: Option<Executor>,
    owner: Option<Owner>,
    entries: Option<crate::external_entry::Dispatches>,
    config: IosLaunch,
    directory: PathBuf,
    restarts: Option<crate::ios_restarts::History>,
    retired: Vec<(Executor, Option<crate::external_entry::Dispatches>)>,
}
impl Target {
    pub fn acquire(
        config: IosLaunch,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Self, AcquisitionError> {
        if config.resource_lifetime_ms.is_some() {
            return Err(io::Error::other("UIKit resource lifetime requires the independent session worker").into());
        }
        Self::acquire_with(config, directory, deadline, cancelled, Xctest::start)
    }
    pub(crate) fn acquire_with(
        config: IosLaunch,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
        start: impl FnOnce(
            XctestLaunch,
            &Path,
            Instant,
            &AtomicBool,
        ) -> Result<Xctest, AcquisitionError>,
    ) -> Result<Self, AcquisitionError> {
        config.validate_app_environment()?;
        if !canonical_uuid(&config.controller_id)
            || [&config.python, &config.ui_script, &config.clock_script]
                .iter()
                .any(|p| !p.is_absolute())
        {
            return Err(io::Error::other("invalid iOS owner or bridge paths").into());
        }
        let mut target = Self {
            xctest: start(config.xctest.clone(), directory, deadline, cancelled)?,
            executor: None,
            owner: None,
            entries: None,
            config: config.clone(), directory: directory.into(), restarts: None, retired: vec![],
        };
        if let Err(error) = target.initialize(&config, directory, deadline, cancelled) {
            target.retain_error(&error);
            let cause = error.to_string();
            let _ = save_atomic(&directory.join("app-acquisition-failure.json"), &cause);
            return Err(AcquisitionError {
                cause,
                retirement: Some(target.stop()),
            });
        }
        Ok(target)
    }
    fn initialize(
        &mut self,
        config: &IosLaunch,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<(), Error> {
        let ready = self.xctest.wait_ready(deadline, cancelled)?;
        self.xctest.apply_initial_data(deadline, cancelled)?;
        let source = &config.xctest.simulator.source_sha256;
        let ui = |call: &UiCall, name| -> Result<UiReply, Error> {
            let reply: UiReply = exchange(
                &config.python,
                &config.ui_script,
                &ready.descriptor,
                &directory.join("ui-client"),
                &directory.join(name),
                call,
                deadline,
                cancelled,
            )?;
            if let UiReply::Unresolved { error } = &reply {
                return Err(Error::Bridge { message: error.clone(), cleanup_failure: None });
            }
            reply.validate(call).map_err(io::Error::other)?;
            Ok(reply)
        };
        ui(
            &UiCall::Initialize {
                identity: ready.identity.clone(),
                source_sha256: source.clone(),
            },
            "ui-initialize",
        )?;
        let request = UiRequest {
            generation: ready.identity.generation.clone(),
            request_id: 1,
            operation: UiOperation::Launch { environment: config.app_environment.clone() },
        };
        let UiReply::Completed { completion } = ui(
            &UiCall::Execute {
                identity: ready.identity.clone(),
                app: None,
                request: request.clone(),
            },
            "ui-launch",
        )?
        else {
            return Err(io::Error::other("missing initial app completion").into());
        };
        let UiOutcome::Launched { app } = &completion.outcome else {
            return Err(io::Error::other("missing acquired app identity").into());
        };
        crate::ios_receipt::verify_launch(directory, &ready.identity, &completion)?;
        save_atomic(&directory.join("app-identity.json"), app)?;
        let descriptor = self
            .xctest
            .app_container(&config.xctest.app.bundle, deadline, cancelled)?
            .join(&config.xctest.app.clock_descriptor);
        let call = ClockCall::Initialize {
            identity: app.clone(),
            source_sha256: source.clone(),
        };
        let reply: ClockReply = exchange(
            &config.python,
            &config.clock_script,
            &descriptor,
            &directory.join("clock-client"),
            &directory.join("clock-initialize"),
            &call,
            deadline,
            cancelled,
        )?;
        if let ClockReply::Unresolved { error } = &reply {
            return Err(Error::Bridge { message: error.clone(), cleanup_failure: None });
        }
        reply.validate(&call).map_err(io::Error::other)?;
        let owner = Owner {
            runner: ready.identity,
            app: app.clone(),
            controller_id: config.controller_id.clone(),
            source_sha256: source.clone(),
        };
        owner.validate().map_err(io::Error::other)?;
        save_atomic(&directory.join("request-owner.json"), &owner)?;
        self.executor = Some(Executor::create(
            directory,
            owner.clone(),
            Config {
                system_alerts: ready.system_alerts,
                surface: config.surface,
                python: config.python.clone(),
                ui_script: config.ui_script.clone(),
                clock_script: config.clock_script.clone(),
                ui_descriptor: ready.descriptor,
                clock_descriptor: descriptor,
            },
        )?);
        if config.surface == crate::ios_execution::UiSurface::Uikit {
            self.entries = Some(crate::external_entry::Dispatches::create(
                &directory.join("external-entry"),
                frontcap_core::native::external_entry::Target::Ios {
                    simulator: owner.app.simulator.to_ascii_lowercase(),
                    bundle: config.xctest.app.bundle.clone(),
                }, owner.app.generation.clone(),
            )?);
        }
        if config.surface == crate::ios_execution::UiSurface::Uikit && ready.app_restart {
            let installation = crate::ios_restarts::inspect_installation(&mut self.xctest, &config.xctest.app.bundle, deadline, cancelled)?;
            self.restarts = Some(crate::ios_restarts::History::create(directory, owner.clone(), installation)?);
        }
        self.owner = Some(owner);
        Ok(())
    }
    pub fn owner(&self) -> &Owner {
        self.owner.as_ref().expect("acquired iOS owner")
    }
    pub fn ensure_running(&mut self) -> io::Result<()> {
        self.xctest.ensure_running()
    }
    pub fn execute(
        &mut self,
        id: RequestId,
        operation: Operation,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Completion, Error> {
        if matches!(operation, Operation::SetAppState { .. }) {
            return Err(io::Error::other("use set_app_state to retain host process evidence").into());
        }
        self.validate_current_id(&id)?;
        if let Some(entries) = &self.entries {
            if entries.contains(&id) { return Err(crate::store::StoreError::Conflict.into()); }
            entries.ensure_resolved()?;
        }
        let result = self
            .executor
            .as_mut()
            .ok_or_else(|| io::Error::other("iOS target retired"))?
            .execute(id, operation, deadline, cancelled);
        if let Err(error) = &result {
            self.retain_error(error);
        }
        result
    }
    /// One ordinary OS dispatch under the acquired UIKit owner; an accepted ID is read-only.
    pub fn dispatch_url(&mut self, id: RequestId,
        request: frontcap_core::native::external_entry::Request,
        deadline: Instant, cancelled: &AtomicBool,
    ) -> io::Result<frontcap_core::native::external_entry::Accepted> {
        self.validate_current_id(&id)?;
        let entries = self.entries.as_mut()
            .ok_or_else(|| io::Error::other("external entry requires a UIKit owner"))?;
        if !entries.contains(&id) {
            self.executor.as_ref().ok_or_else(|| io::Error::other("iOS target retired"))?
                .validate_external_entry(&id).map_err(io::Error::other)?;
        }
        let admission = entries.directory().join(format!("admission-{id}.json"));
        entries.dispatch(&id, request, |request| {
            let frontcap_core::native::external_entry::Target::Ios { bundle, .. } = &request.target
                else { return Err(io::Error::other("external entry target is not iOS")); };
            self.xctest.ensure_running()?;
            let declarations = crate::ios_external_entry::admit(self.xctest.simulator_mut(),
                bundle, &request.url, deadline, cancelled)?;
            save_atomic(&admission, &declarations)?;
            self.executor.as_mut().ok_or_else(|| io::Error::other("iOS target retired"))?
                .invalidate_for_external_entry(&id).map_err(io::Error::other)?;
            self.xctest.ensure_running()?;
            crate::ios_external_entry::open(self.xctest.simulator_mut(), &request.url, deadline, cancelled)
        })
    }
    fn validate_current_id(&self, id: &RequestId) -> io::Result<()> {
        if let Some(history) = &self.restarts {
            history.ensure_resolved()?;
            if history.contains(id) { return Err(io::Error::other("request ID belongs to UIKit restart")); }
        }
        if self.retired.iter().any(|(executor, entries)| executor.contains(id) || entries.as_ref().is_some_and(|e|e.contains(id))) {
            return Err(io::Error::other("request ID belongs to an earlier UIKit generation"));
        }
        Ok(())
    }
    pub fn set_app_state(&mut self, id: RequestId, generation: &str,
        state: frontcap_core::native::app_state::State, deadline: Instant, cancelled: &AtomicBool)
        -> Result<frontcap_core::native::ios_lifecycle::Transition, Error> {
        let result = self.set_app_state_inner(id,generation,state,deadline,cancelled);
        if let Err(error) = &result { self.retain_error(error); }
        result
    }
    fn set_app_state_inner(&mut self, id: RequestId, generation: &str,
        state: frontcap_core::native::app_state::State, deadline: Instant, cancelled: &AtomicBool)
        -> Result<frontcap_core::native::ios_lifecycle::Transition, Error> {
        self.validate_current_id(&id)?;
        if self.owner().app.generation != generation { return Err(io::Error::other("UIKit host transition generation differs").into()); }
        let ready: crate::xctest::UiReady = frontcap_core::state::load(&self.directory.join("ui-ready.json"))?.ok_or_else(||io::Error::other("missing UIKit readiness"))?;
        if !ready.app_lifecycle || ready.identity != self.owner().runner {return Err(io::Error::other("UIKit host transitions unavailable").into());}
        let directory=self.executor.as_ref().ok_or_else(||io::Error::other("UIKit target retired"))?.directory().to_path_buf();
        if let Some(original)=crate::ios_app_state::read_completion(&directory,&id,self.owner(),state)? {return Ok(original);}
        if let Some(entries)=&self.entries {entries.ensure_resolved()?;if entries.contains(&id) {return Err(crate::store::StoreError::Conflict.into());}}
        crate::ios_app_state::prepare(&directory,&id,self.owner(),state)?;
        self.executor.as_mut().ok_or_else(||io::Error::other("UIKit target retired"))?
            .execute(id.clone(),Operation::SetAppState {generation:generation.into(),state},deadline,cancelled)?;
        Ok(crate::ios_app_state::complete(&directory,&id,self.owner(),state)?)
    }
    pub fn restart_available(&self) -> bool { self.restarts.is_some() }
    pub fn restart(&mut self, id: RequestId, generation: &str, url: Option<&str>, deadline: Instant, cancelled: &AtomicBool)
        -> Result<frontcap_core::native::ios_lifecycle::Replacement, Error> {
        let result = self.restart_inner(id, generation, url, deadline, cancelled);
        if let Err(error) = &result { self.retain_error(error); }
        result
    }
    fn restart_inner(&mut self, id: RequestId, generation: &str, url: Option<&str>, deadline: Instant, cancelled: &AtomicBool)
        -> Result<frontcap_core::native::ios_lifecycle::Replacement, Error> {
        self.validate_current_id(&id)?;
        if !self.restart_available() || self.owner().app.generation != generation {
            return Err(io::Error::other("UIKit restart generation or capability differs").into());
        }
        if let Some(entries) = &self.entries {
            entries.ensure_resolved()?;
            if entries.contains(&id) { return Err(io::Error::other("restart ID belongs to UIKit external entry").into()); }
        }
        let executor = self.executor.as_mut().ok_or_else(||io::Error::other("UIKit target retired"))?;
        let request = executor.restart_request(&id, url)?;
        let entry = frontcap_core::native::ios_lifecycle::restart_entry(self.owner.as_ref().unwrap(), &self.config.xctest.app.bundle, url).map_err(io::Error::other)?;
        let admission = if let Some(entry) = entry {
            let ready: crate::xctest::UiReady = frontcap_core::state::load(&self.directory.join("ui-ready.json"))?.ok_or_else(||io::Error::other("missing UIKit runner readiness"))?;
            if !ready.app_entry { return Err(io::Error::other("UIKit runner does not advertise URL launch").into()); }
            Some(crate::ios_external_entry::admit(self.xctest.simulator_mut(), &self.config.xctest.app.bundle, &entry.url, deadline, cancelled)?)
        } else { None };
        let before = crate::ios_restarts::inspect_installation(&mut self.xctest, &self.config.xctest.app.bundle, deadline, cancelled)?;
        self.restarts.as_mut().unwrap().dispatch(id.clone(), request.clone(), before, admission.as_ref())?;
        let completion = executor.restart(&id, request, deadline, cancelled)?;
        let after = crate::ios_restarts::inspect_installation(&mut self.xctest, &self.config.xctest.app.bundle, deadline, cancelled)?;
        let replacement = self.restarts.as_ref().unwrap().observe_completion(&id, completion, after)?;
        let directory = crate::ios_restarts::epoch_directory(&self.directory, &replacement.owner.app.generation);
        std::fs::create_dir_all(directory.parent().unwrap())?;
        std::fs::create_dir(&directory)?;
        // Transfer only the live runner cursor after its verified replacement. Historical receipts,
        // captures and app clocks remain in their original epochs; no prior client is reused.
        for path in ["ui-client/owner.json", "ui-client/app.json", "ui-client/ui/ui-client.json"] {
            let destination = directory.join(path);
            std::fs::create_dir_all(destination.parent().unwrap())?;
            std::fs::copy(executor.directory().join(path), &destination)?;
            std::fs::File::open(destination)?.sync_all()?;
        }
        let config = executor.config().clone();
        let call = ClockCall::Initialize { identity: replacement.owner.app.clone(), source_sha256: replacement.owner.source_sha256.clone() };
        let reply: ClockReply = exchange(&config.python, &config.clock_script, &config.clock_descriptor,
            &directory.join("clock-client"), &directory.join("clock-initialize"), &call, deadline, cancelled)?;
        reply.validate(&call).map_err(io::Error::other)?;
        if !matches!(reply, ClockReply::Ready { .. }) { return Err(io::Error::other("replacement app clock unavailable").into()); }
        let next = replacement.completion.request.request_id.checked_add(1).ok_or_else(||io::Error::other("UIKit runner sequence exhausted"))?;
        let next_executor = Executor::create_epoch(&directory, replacement.owner.clone(), config, next)?;
        save_atomic(&directory.join("request-owner.json"), &replacement.owner)?;
        let entries = Some(crate::external_entry::Dispatches::create(&directory.join("external-entry"),
            frontcap_core::native::external_entry::Target::Ios { simulator: replacement.owner.app.simulator.to_ascii_lowercase(),
                bundle: replacement.installation.bundle.clone() }, replacement.owner.app.generation.clone())?);
        self.restarts.as_mut().unwrap().complete(&id, replacement.clone())?;
        self.retired.push((self.executor.replace(next_executor).unwrap(), std::mem::replace(&mut self.entries, entries)));
        self.owner = Some(replacement.owner.clone());
        Ok(replacement)
    }
    fn retain_error(&mut self, error: &Error) {
        if let Error::Bridge {
            cleanup_failure: Some(failure),
            ..
        } = error
        {
            self.xctest.retain_cleanup_failure(failure.clone());
        }
    }
    pub fn stop(&mut self) -> Retirement {
        self.executor.take();
        self.xctest.stop()
    }
}
impl Drop for Target {
    fn drop(&mut self) {
        self.stop();
    }
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn exchange<C: Serialize, R: DeserializeOwned + Serialize>(
    python: &Path,
    script: &Path,
    descriptor: &Path,
    client: &Path,
    evidence: &Path,
    call: &C,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<R, Error> {
    exchange_with_group(python, script, descriptor, client, evidence, call, deadline, cancelled, None)
}
#[allow(clippy::too_many_arguments)]
pub(crate) fn exchange_with_group<C: Serialize, R: DeserializeOwned + Serialize>(
    python: &Path, script: &Path, descriptor: &Path, client: &Path, evidence: &Path,
    call: &C, deadline: Instant, cancelled: &AtomicBool,
    supervision: Option<&crate::resource_handle::Shared>,
) -> Result<R, Error> {
    let mut command = Command::new(python);
    command
        .arg(script)
        .arg("--descriptor")
        .arg(descriptor)
        .arg("--directory")
        .arg(client)
        .arg("--timeout")
        .arg(
            deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
                .clamp(0.001, 60.0)
                .to_string(),
        );
    let result = if let Some(shared) = supervision {
        crate::resource_handle::with_group(shared, |group|
            group.exchange(&command, call, evidence, deadline, cancelled, 1024 * 1024))?
    } else { exchange_typed_recorded(
        &mut command,
        call,
        deadline,
        cancelled,
        1024 * 1024,
        evidence,
    )? };
    if result.failure.is_some()
        || result.cleanup_failure.is_some()
        || result.evidence_failure.is_some()
    {
        return Err(Error::Bridge {
            message: format!(
                "operation: {:?}; evidence: {:?}",
                result.failure, result.evidence_failure
            ),
            cleanup_failure: result.cleanup_failure,
        });
    }
    result
        .reply
        .ok_or_else(|| io::Error::other("missing acquisition bridge reply").into())
}
