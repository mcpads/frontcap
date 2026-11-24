//! Execute stored iOS intents under an already-owned live XCTest/app session.
//! This is not target acquisition, process adoption or a controller reconnect protocol.
use crate::{
    ios::{ClockCall, ClockReply, UiCall, UiReply},
    ios_store::{Completion, Operation, Owner, Recovery, Request, RequestStore},
    process::exchange_typed_recorded,
    store::StoreError,
};
use frontcap_core::{
    ids::{LaunchId, RequestId},
    state::{load, save_atomic},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("iOS bridge failed: {message}; cleanup: {cleanup_failure:?}")]
    Bridge {
        message: String,
        cleanup_failure: Option<String>,
    },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub system_alerts: bool,
    #[serde(default)]
    pub surface: UiSurface,
    pub python: PathBuf,
    pub ui_script: PathBuf,
    pub clock_script: PathBuf,
    pub ui_descriptor: PathBuf,
    pub clock_descriptor: PathBuf,
}
/// Selected by the frozen host profile and verified against the bridge's saved owner.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UiSurface {
    #[default]
    Uikit,
}
impl UiSurface {
    pub fn observation(self) -> Operation {
        match self {
            Self::Uikit => Operation::ObserveTargets,
        }
    }
    pub fn validate_operation(self, operation: &Operation) -> io::Result<()> {
        let supported = match operation {
            Operation::ObserveSystemAlerts | Operation::TapSystemButton { .. } | Operation::SetAppState { .. }
                | Operation::ObserveTargets | Operation::TapTarget { .. } | Operation::PressTarget { .. }
                | Operation::ReplaceText { .. } | Operation::ScrollTarget { .. } => self == Self::Uikit,
            Operation::ReadClock | Operation::AdvanceClock { .. } | Operation::Finish => true,
        };
        if supported { Ok(()) } else {
            Err(io::Error::other("operation differs from configured iOS UI surface"))
        }
    }
}
#[derive(Deserialize)]
struct ClientOwner<T> {
    #[serde(default)]
    surface: Option<UiSurface>,
    identity: T,
    source_sha256: String,
    descriptor: PathBuf,
}
#[derive(Deserialize)]
struct Cursor {
    generation: String,
    next_id: u64,
    pending: Option<serde::de::IgnoredAny>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    owner: Owner,
    config: Config,
}
pub struct Executor {
    directory: PathBuf,
    owner: Owner,
    config: Config,
    store: RequestStore,
}
impl Executor {
    pub fn create(directory: &Path, owner: Owner, config: Config) -> Result<Self, Error> {
        Self::create_epoch(directory, owner, config, 2)
    }
    pub(crate) fn create_epoch(directory: &Path, owner: Owner, config: Config, first_ui: u64) -> Result<Self, Error> {
        owner.validate().map_err(io::Error::other)?;
        if [
            &config.python,
            &config.ui_script,
            &config.clock_script,
            &config.ui_descriptor,
            &config.clock_descriptor,
        ]
        .iter()
        .any(|p| !p.is_absolute())
        {
            return Err(io::Error::other("iOS execution paths must be absolute").into());
        }
        let ui: ClientOwner<frontcap_core::native::ios_ui::UiIdentity> =
            required(&directory.join("ui-client/owner.json"))?;
        let clock: ClientOwner<frontcap_core::native::ios::ClockIdentity> =
            required(&directory.join("clock-client/owner.json"))?;
        let app: frontcap_core::native::ios::ClockIdentity =
            required(&directory.join("ui-client/app.json"))?;
        let ui_cursor: Cursor = required(&directory.join("ui-client/ui/ui-client.json"))?;
        let clock_cursor: Cursor =
            required(&directory.join("clock-client/clock/clock-client.json"))?;
        if ui.surface != Some(config.surface)
            || clock.surface.is_some()
            || ui.identity != owner.runner
            || clock.identity != owner.app
            || app != owner.app
            || ui.source_sha256 != owner.source_sha256
            || clock.source_sha256 != owner.source_sha256
            || ui.descriptor != config.ui_descriptor.canonicalize()?
            || clock.descriptor != config.clock_descriptor.canonicalize()?
            || ui_cursor.generation != owner.runner.generation
            || ui_cursor.next_id != first_ui || first_ui < 2
            || ui_cursor.pending.is_some()
            || clock_cursor.generation != owner.app.generation
            || clock_cursor.next_id != 1
            || clock_cursor.pending.is_some()
        {
            return Err(io::Error::other(
                "iOS executor requires matching initialized clients and initial Launch",
            )
            .into());
        }
        if directory.join("execution.json").symlink_metadata().is_ok() {
            return Err(io::Error::other("existing iOS execution cannot be reset").into());
        }
        let store = RequestStore::create_epoch(&directory.join("requests"), owner.clone(), first_ui)?;
        save_atomic(
            &directory.join("execution.json"),
            &Binding {
                owner: owner.clone(),
                config: config.clone(),
            },
        )?;
        Ok(Self {
            directory: directory.into(),
            owner,
            config,
            store,
        })
    }
    /// Requires the caller to still own the original target. This only reopens storage.
    pub fn open(directory: &Path, owner: Owner, config: Config) -> Result<Self, Error> {
        let binding: Binding = required(&directory.join("execution.json"))?;
        if binding.owner != owner || binding.config != config {
            return Err(StoreError::IdentityChanged.into());
        }
        let store = RequestStore::open(&directory.join("requests"), &owner)?;
        Ok(Self {
            directory: directory.into(),
            owner,
            config,
            store,
        })
    }
    /// Pure saved-result lookup: no subprocess, client cursor changes or target contact.
    pub fn read_completion(
        directory: &Path,
        owner: &Owner,
        id: &RequestId,
        operation: &Operation,
    ) -> Result<Option<Completion>, Error> {
        let completion =
            RequestStore::read_completion(&directory.join("requests"), owner, id, operation)?;
        if let Some(c) = &completion {
            let request = c.request();
            crate::ios_receipt::verify(directory, owner, &request, c)?;
        }
        Ok(completion)
    }
    pub(crate) fn directory(&self) -> &Path { &self.directory }
    pub(crate) fn config(&self) -> &Config { &self.config }
    pub(crate) fn contains(&self, id: &RequestId) -> bool { self.store.contains(id) }
    pub(crate) fn restart_request(&self, id: &RequestId, url: Option<&str>) -> Result<frontcap_core::native::ios_ui::UiRequest, Error> {
        self.validate_external_entry(id)?;
        Ok(frontcap_core::native::ios_ui::UiRequest { generation: self.owner.runner.generation.clone(),
            request_id: self.store.next_ui(), operation: frontcap_core::native::ios_ui::UiOperation::Restart { app: self.owner.app.clone(), url: url.map(str::to_owned) } })
    }
    pub(crate) fn restart(&mut self, id: &RequestId, request: frontcap_core::native::ios_ui::UiRequest,
        deadline: Instant, cancelled: &AtomicBool) -> Result<frontcap_core::native::ios_ui::UiCompletion, Error> {
        let frontcap_core::native::ios_ui::UiOperation::Restart { url, .. } = &request.operation else { return Err(io::Error::other("expected UIKit restart request").into()); };
        if self.restart_request(id, url.as_deref())? != request { return Err(io::Error::other("UIKit restart request differs").into()); }
        self.invalidate_for_external_entry(id)?;
        let call = UiCall::Execute { identity: self.owner.runner.clone(), app: Some(self.owner.app.clone()), request };
        let exchanges = self.directory.join("restart-exchanges");
        std::fs::create_dir_all(&exchanges)?;
        let reply: UiReply = self.exchange(false, &call, &exchanges.join(id.as_str()), deadline, cancelled)?;
        reply.validate(&call).map_err(io::Error::other)?;
        let UiReply::Completed { completion } = reply else { return Err(io::Error::other("UIKit restart unresolved").into()); };
        crate::ios_receipt::verify_restart(&self.directory, &self.owner, &completion)?;
        Ok(*completion)
    }
    pub(crate) fn validate_external_entry(&self, id: &RequestId) -> Result<(), Error> {
        if self.config.surface != UiSurface::Uikit {
            return Err(io::Error::other("external entry requires a UIKit owner").into());
        }
        self.store.validate_external_entry(id)?;
        Ok(())
    }
    pub(crate) fn invalidate_for_external_entry(&mut self, id: &RequestId) -> Result<(), Error> {
        self.store.invalidate_for_external_entry(id)?;
        Ok(())
    }
    pub fn execute(
        &mut self,
        id: RequestId,
        operation: Operation,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Completion, Error> {
        self.config.surface.validate_operation(&operation)?;
        if matches!(operation, Operation::ObserveSystemAlerts | Operation::TapSystemButton { .. }) && !self.config.system_alerts {
            return Err(io::Error::other("XCTest runner does not advertise system alerts").into());
        }
        RequestStore::validate_ui_input(&self.directory.join("requests"), &self.owner, &id, &operation)?;
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(io::Error::other("iOS execution cancelled or expired").into());
        }
        let (request, recover) = match self.store.prepare(id.clone(), operation)? {
            Recovery::Prepared(request) => {
                if !matches!(request,Request::Press { .. }) { self.store.dispatch(&id)?; }
                (request, false)
            }
            Recovery::Reconcile(request) => (request, true),
            Recovery::Completed(completion) => {
                let request = completion.request();
                crate::ios_receipt::verify(&self.directory, &self.owner, &request, &completion)?;
                return Ok(completion);
            }
        };
        let folder = self.directory.join("execution-commands").join(id.as_str());
        std::fs::create_dir_all(&folder)?;
        let evidence = folder.join(LaunchId::generate().to_string());
        let completion = match &request {
            Request::Press { ui, clock } => {
                use frontcap_core::native::{ios_request::PressClock, ios_ui::UiOutcome};
                std::fs::create_dir_all(&evidence)?;
                if !recover {
                    let reserve=ClockCall::ReserveUi { identity:self.owner.app.clone(), request:clock.clone(),
                        ui_identity:self.owner.runner.clone(), ui_request:ui.clone() };
                    let reply: ClockReply=self.exchange(true,&reserve,&evidence.join("reserve"),deadline,cancelled)?;
                    reply.validate(&reserve).map_err(io::Error::other)?;
                    // Reservation is safe to repeat while Prepared. Once dispatched, only saved UI
                    // receipts may resolve this request, even if the first bridge never started.
                    self.store.dispatch(&id)?;
                }
                let call=if recover {
                    UiCall::Recover { identity:self.owner.runner.clone(),app:Some(self.owner.app.clone()),request:ui.clone() }
                } else {
                    UiCall::Execute { identity:self.owner.runner.clone(),app:Some(self.owner.app.clone()),request:ui.clone() }
                };
                let reply: UiReply=self.exchange(false,&call,&evidence.join("ui"),deadline,cancelled)?;
                reply.validate(&call).map_err(io::Error::other)?;
                let UiReply::Completed { completion: result }=reply else {
                    return Err(io::Error::other("missing timed press UI result").into());
                };
                crate::ios_receipt::verify(&self.directory,&self.owner,&Request::Ui(ui.clone()),&Completion::Ui(result.clone()))?;
                let clock=match &result.outcome {
                    UiOutcome::TargetPressed { .. } => {
                        let call=ClockCall::Recover { identity:self.owner.app.clone(),request:clock.clone() };
                        let reply: ClockReply=self.exchange(true,&call,&evidence.join("clock"),deadline,cancelled)?;
                        reply.validate(&call).map_err(io::Error::other)?;
                        let ClockReply::Completed { completion }=reply else {
                            return Err(io::Error::other("missing timed press clock result").into());
                        };
                        PressClock::Delivered { completion }
                    }
                    UiOutcome::TargetPressNotApplied { .. } => {
                        let call=ClockCall::ReleaseUi { identity:self.owner.app.clone(),request:clock.clone(),
                            ui_identity:self.owner.runner.clone(),ui_request:ui.clone(),receipt_sha256:result.receipt_sha256.clone() };
                        let reply: ClockReply=self.exchange(true,&call,&evidence.join("release"),deadline,cancelled)?;
                        reply.validate(&call).map_err(io::Error::other)?;
                        PressClock::Released { request:clock.clone(),ui_receipt_sha256:result.receipt_sha256.clone() }
                    }
                    _ => return Err(io::Error::other("unexpected timed press UI outcome").into()),
                };
                Completion::Press { ui:result, clock }
            }
            Request::Clock(request) => {
                let call = if recover {
                    ClockCall::Recover {
                        identity: self.owner.app.clone(),
                        request: request.clone(),
                    }
                } else {
                    ClockCall::Execute {
                        identity: self.owner.app.clone(),
                        request: request.clone(),
                    }
                };
                let reply: ClockReply =
                    self.exchange(true, &call, &evidence, deadline, cancelled)?;
                if let ClockReply::Unresolved { error } = &reply {
                    return Err(Error::Bridge {
                        message: error.clone(),
                        cleanup_failure: None,
                    });
                }
                reply.validate(&call).map_err(io::Error::other)?;
                let ClockReply::Completed { completion } = reply else {
                    return Err(io::Error::other("missing iOS clock completion").into());
                };
                Completion::Clock(completion)
            }
            Request::Ui(request) => {
                let call = if recover {
                    UiCall::Recover {
                        identity: self.owner.runner.clone(),
                        app: Some(self.owner.app.clone()),
                        request: request.clone(),
                    }
                } else {
                    UiCall::Execute {
                        identity: self.owner.runner.clone(),
                        app: Some(self.owner.app.clone()),
                        request: request.clone(),
                    }
                };
                let reply: UiReply = self.exchange(false, &call, &evidence, deadline, cancelled)?;
                if let UiReply::Unresolved { error } = &reply {
                    return Err(Error::Bridge {
                        message: error.clone(),
                        cleanup_failure: None,
                    });
                }
                reply.validate(&call).map_err(io::Error::other)?;
                let UiReply::Completed { completion } = reply else {
                    return Err(io::Error::other("missing XCTest completion").into());
                };
                Completion::Ui(completion)
            }
        };
        crate::ios_receipt::verify(&self.directory, &self.owner, &request, &completion)?;
        self.store.complete(&id, completion.clone())?;
        Ok(completion)
    }
    fn exchange<C: Serialize, R: DeserializeOwned + Serialize>(
        &self,
        clock: bool,
        call: &C,
        evidence: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<R, Error> {
        let (script, descriptor, client) = if clock {
            (
                &self.config.clock_script,
                &self.config.clock_descriptor,
                "clock-client",
            )
        } else {
            (
                &self.config.ui_script,
                &self.config.ui_descriptor,
                "ui-client",
            )
        };
        let mut command = Command::new(&self.config.python);
        command
            .arg(script)
            .arg("--descriptor")
            .arg(descriptor)
            .arg("--directory")
            .arg(self.directory.join(client))
            .arg("--timeout")
            .arg(deadline.saturating_duration_since(Instant::now()).as_secs_f64().clamp(0.001, 60.0).to_string());
        let result = exchange_typed_recorded(
            &mut command,
            call,
            deadline,
            cancelled,
            1024 * 1024,
            evidence,
        )?;
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
            .ok_or_else(|| io::Error::other("missing iOS bridge reply").into())
    }
}
fn required<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    load(path)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing initialized iOS evidence"))
}

#[cfg(test)]
#[path = "ios_execution_tests.rs"]
mod tests;
