//! One owned simulator app, external page identity and durable clock request history.
//! Clock readiness does not imply application/content or all-producer readiness.
use crate::{
    device::AcquisitionError,
    request::ClockOperation,
    simulator_app::App,
    webview::{Endpoint, Reply, Status},
    webview_store::{Owner, RequestStore},
};
use frontcap_core::{
    ids::RequestId,
    native::{
        contacts::{Operation as ContactOperation, Receipt as ContactReceipt},
        ownership::Retirement,
    },
    state::{load, save_atomic},
};
use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub struct Target {
    app: App,
    requests: RequestStore,
    owner: Owner,
    directory: PathBuf,
    retired: Option<Retirement>,
    input: crate::simulator_input::Input,
    lifecycle: Option<Lifecycle>,
    lifecycle_config: Option<crate::xctest::RunnerLaunch>,
}
struct Lifecycle {
    requests: crate::webview_lifecycle_store::RequestStore,
    descriptor: PathBuf,
}
impl Target {
    pub fn connect(
        mut app: App,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Self, AcquisitionError> {
        if let Err(error) = std::fs::create_dir(directory) {
            return Err(AcquisitionError {
                cause: error.to_string(),
                retirement: Some(app.stop()),
            });
        }
        let work = Self::initialize(&mut app, directory, None, deadline, cancelled);
        match work {
            Ok((requests, owner)) => Ok(Self {
                app,
                requests,
                owner,
                directory: directory.to_path_buf(),
                retired: None,
                input: crate::simulator_input::Input::new(directory),
                lifecycle: None,
                lifecycle_config: None,
            }),
            Err(error) => {
                let _ = save_atomic(&directory.join("failure.json"), &error.to_string());
                Err(AcquisitionError {
                    cause: error.to_string(),
                    retirement: Some(app.stop()),
                })
            }
        }
    }
    fn initialize(
        app: &mut App,
        directory: &Path,
        previous: Option<&Owner>,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<(RequestStore, Owner)> {
        let descriptor = app.container().join("Documents/clock-control.json");
        let endpoint: Endpoint = loop {
            budget(deadline, cancelled)?;
            app.ensure_running(deadline, cancelled)?;
            if let Some(endpoint) = load::<Endpoint>(&descriptor)? {
                if previous.is_none_or(|old| endpoint.generation != old.endpoint.generation)
                    && endpoint.pid == app.process().pid { break endpoint; }
                if previous.is_none() { return Err(io::Error::other("WebView endpoint differs from launched process")); }
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        if endpoint.pid != app.process().pid {
            return Err(io::Error::other(
                "WebView endpoint differs from launched process",
            ));
        }
        let owner = Owner {
            simulator: app.simulator_id().into(),
            process: app.process().clone(),
            endpoint,
            attachment_sha256: app.attachment_sha256().into(),
        };
        let mut requests = RequestStore::create(&directory.join("requests"), owner.clone())
            .map_err(io::Error::other)?;
        let mut number = 0u64;
        loop {
            budget(deadline, cancelled)?;
            app.ensure_running(deadline, cancelled)?;
            number += 1;
            let reply = requests
                .execute(
                    RequestId::new(format!("mcp-webview-acquire-{number}"))
                        .map_err(io::Error::other)?,
                    ClockOperation::Read,
                    0,
                    deadline.min(Instant::now() + Duration::from_secs(10)),
                )
                .map_err(io::Error::other)?;
            match reply.status {
                Status::Ok => break,
                Status::NotReady if previous.is_some() => break,
                Status::NotReady => std::thread::sleep(Duration::from_millis(25)),
                _ => {
                    return Err(io::Error::other(
                        "WebView initialization unresolved or rejected",
                    ));
                }
            }
        }
        save_atomic(&directory.join("clock-ready.json"), &owner)?;
        Ok((requests, owner))
    }
    /// Replace the app and all producer bindings; old journals remain immutable on disk.
    /// New evidence lives in an unused directory. Failure retires ownership rather than exposing
    /// a mixture of old/new producers. The recorder must separately publish the returned owner.
    pub fn restart(&mut self, directory: &Path, deadline: Instant, cancelled: &AtomicBool) -> Result<Owner, AcquisitionError> {
        if let Err(error) = std::fs::create_dir(directory) {
            return Err(AcquisitionError { cause: error.to_string(), retirement: Some(self.stop()) });
        }
        let result = self.restart_inner(directory, deadline, cancelled);
        result.map_err(|error| {
            let _ = save_atomic(&directory.join("failure.json"), &error.to_string());
            AcquisitionError { cause: error.to_string(), retirement: Some(self.stop()) }
        })
    }
    fn restart_inner(&mut self, directory: &Path, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Owner> {
        budget(deadline, cancelled)?;
        self.app.ensure_running(deadline, cancelled)?;
        let previous = self.owner.clone();
        let contacts = self.input.contacts.is_some();
        let old_contact_generation = self.input.contacts.as_ref().map(|s| s.endpoint().generation.clone());
        let old_document = self.requests.inspect().last_completed.and_then(|r| r.reply.document);
        let lifecycle = self.lifecycle_config.clone();
        save_atomic(&directory.join("replacement-intent.json"), &serde_json::json!({
            "previous":previous,"previous_evidence":self.directory,"contacts":contacts,"lifecycle":lifecycle.is_some()
        }))?;
        // Retain original unknown outcomes before killing the producers. Never promote them.
        frontcap_core::state::save_atomic_bytes(&self.directory.join("last-native-receipt.json"),
            &crate::webview::read_receipt_bytes(&self.app.container().join("Documents/clock-control.json"))?)?;
        if let Some(session) = &self.input.contacts { session.retain_pending()?; }
        if let Some(lifecycle) = &self.lifecycle {
            frontcap_core::state::save_atomic_bytes(&self.directory.join("last-lifecycle-receipt.json"),
                &crate::webview::read_receipt_bytes(&lifecycle.descriptor)?)?;
        }
        // A runner-detach failure can retire the device before a replacement clock exists.
        // Keep the previous exact receipt with that attempted epoch as well as its original journal.
        frontcap_core::state::save_atomic_bytes(&directory.join("last-native-receipt.json"),
            &crate::webview::read_receipt_bytes(&self.directory.join("last-native-receipt.json"))?)?;
        self.input = crate::simulator_input::Input::new(directory);
        self.lifecycle = None;
        self.directory = directory.canonicalize()?;
        self.app.restart(&directory.join("app"), deadline, cancelled).map_err(|e| io::Error::other(e.cause))?;
        let (requests, owner) = Self::initialize(&mut self.app, directory, Some(&previous), deadline, cancelled)?;
        if old_document.is_some() && requests.inspect().last_completed.and_then(|r| r.reply.document) == old_document {
            return Err(io::Error::other("replacement reused the previous page document"));
        }
        frontcap_core::state::save_atomic_bytes(&directory.join("last-native-receipt.json"),
            &crate::webview::read_receipt_bytes(&self.app.container().join("Documents/clock-control.json"))?)?;
        self.requests = requests;
        self.owner = owner;
        self.directory = directory.canonicalize()?;
        if contacts {
            self.enable_contacts(deadline, cancelled)?;
            if self.input.contacts.as_ref().map(|s| s.endpoint().generation.clone()) == old_contact_generation {
                return Err(io::Error::other("replacement reused the previous contact generation"));
            }
        }
        if let Some(config) = lifecycle {
            self.enable_lifecycle(config, deadline, cancelled).map_err(|e| io::Error::other(e.cause))?;
        }
        save_atomic(&directory.join("replacement-ready.json"), &self.owner)?;
        Ok(self.owner.clone())
    }
    /// Attach only to the existing app. Any acquisition failure retires the whole owned target.
    pub fn enable_lifecycle(
        &mut self,
        config: crate::xctest::RunnerLaunch,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<(), AcquisitionError> {
        if self.lifecycle.is_some() {
            return Err(AcquisitionError {
                cause: "lifecycle runner already attached".into(),
                retirement: None,
            });
        }
        let saved_config = config.clone();
        let result = (|| -> io::Result<()> {
            budget(deadline, cancelled)?;
            let products_sha256 = frontcap_core::canonical::sha256_hex(
                &frontcap_core::canonical::canonical_of(&config.manifest),
            );
            // App attachment owns failure retirement, which may delete the live descriptor.
            // Preserve the exact current receipt before transferring control to that operation.
            frontcap_core::state::save_atomic_bytes(&self.directory.join("last-native-receipt.json"),
                &crate::webview::read_receipt_bytes(&self.app.container().join("Documents/clock-control.json"))?)?;
            let ready = self
                .app
                .attach_xctest(config, deadline, cancelled)
                .map_err(|e| io::Error::other(e.cause))?;
            let owner = frontcap_core::native::webview_lifecycle::Owner {
                app: self.owner.clone(),
                runner: ready.identity,
                runner_process: ready.process,
                port: ready.port,
                products_sha256,
            };
            let requests = crate::webview_lifecycle_store::RequestStore::create(
                &self.directory.join("lifecycle"),
                owner,
            )
            .map_err(io::Error::other)?;
            self.lifecycle = Some(Lifecycle {
                requests,
                descriptor: ready.descriptor,
            });
            let activated = self.lifecycle(
                RequestId::new("mcp-webview-lifecycle-acquire").unwrap(),
                frontcap_core::native::webview_lifecycle::Operation::Launch,
                deadline,
                cancelled,
            )?;
            if activated.status != frontcap_core::native::webview_lifecycle::Status::Ok {
                return Err(io::Error::other("lifecycle activation rejected"));
            }
            self.lifecycle_config = Some(saved_config);
            Ok(())
        })();
        result.map_err(|error| AcquisitionError {
            cause: error.to_string(),
            retirement: Some(self.stop()),
        })
    }
    pub fn lifecycle(
        &mut self,
        id: RequestId,
        operation: frontcap_core::native::webview_lifecycle::Operation,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<frontcap_core::native::webview_lifecycle::Reply> {
        budget(deadline, cancelled)?;
        self.app.ensure_running(deadline, cancelled)?;
        let lifecycle = self
            .lifecycle
            .as_mut()
            .ok_or_else(|| io::Error::other("lifecycle unavailable"))?;
        let admission = lifecycle
            .requests
            .prepare(id.clone(), operation)
            .map_err(io::Error::other)?;
        if matches!(
            admission,
            crate::webview_lifecycle_store::Recovery::Prepared { .. }
        ) {
            if let Some(contacts) = &mut self.input.contacts {
                contacts.interrupt()?;
            }
            self.input.capture_basis = None;
        }
        lifecycle
            .requests
            .execute(id, operation, deadline)
            .map_err(io::Error::other)
    }
    pub fn reconcile_lifecycle(
        &mut self,
        id: &RequestId,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<frontcap_core::native::webview_lifecycle::Reply> {
        budget(deadline, cancelled)?;
        let lifecycle = self
            .lifecycle
            .as_mut()
            .ok_or_else(|| io::Error::other("lifecycle unavailable"))?;
        let path = if self.retired.is_some() {
            self.directory.join("last-lifecycle-receipt.json")
        } else {
            lifecycle.descriptor.clone()
        };
        lifecycle
            .requests
            .reconcile(id, &path)
            .map_err(io::Error::other)
    }
    pub fn enable_contacts(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        self.input.enable_contacts(&mut self.app, self.owner.clone(), deadline, cancelled)
    }
    pub fn input(&mut self, id: RequestId, observation: RequestId, operation: ContactOperation,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<ContactReceipt> {
        self.input.input(&mut self.app,id,observation,operation,deadline,cancelled)
    }
    pub fn reconcile_input(&mut self, id: &RequestId, deadline: Instant, cancelled: &AtomicBool) -> io::Result<ContactReceipt> {
        self.input.reconcile_input(id,deadline,cancelled)
    }
    /// Idle monitoring checks the owned process identity without launching simctl inventory.
    /// Action admission still validates the simulator before observing or dispatching new effects.
    pub fn ensure_process_running(
        &self,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<()> {
        budget(deadline, cancelled)?;
        if self.retired.is_some() || self.owner.process.has_exited()? {
            return Err(io::Error::other("owned WebView app exited or retired"));
        }
        Ok(())
    }
    /// Local bounded evidence only, usable even while a producer request is unresolved.
    pub fn inspect_progress(&self) -> frontcap_core::native::webview_progress::Progress {
        use frontcap_core::native::webview_progress::{Basis, Progress, Unobserved};
        Progress {
            owner: self.owner.clone(), retired: self.retired.is_some(), basis: Basis::RetainedReceipts,
            clock: self.requests.inspect(),
            service: frontcap_core::native::webview_progress::Service::NotInspected,
            contact_endpoint: self.input.contacts.as_ref().map(|s| s.endpoint().clone()),
            contacts: self.input.contacts.as_ref().map(|s| s.inspect()),
            requires_cancel: self.input.contacts.as_ref().map(|s| s.requires_cancel()),
            lifecycle: self.lifecycle.as_ref().map(|l| l.requests.inspect()),
            unobserved: vec![Unobserved::ServiceDelivery, Unobserved::Media, Unobserved::Speech,
                Unobserved::NativePresentation, Unobserved::OtherAppWork],
        }
    }
    pub fn owner(&self) -> &Owner {
        &self.owner
    }
    pub fn execute(
        &mut self,
        id: RequestId,
        operation: ClockOperation,
        delta_ns: u64,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Reply> {
        if id.as_str().starts_with("mcp-webview-acquire-") {
            return Err(io::Error::other(
                "request ID reserved for WebView acquisition",
            ));
        }
        budget(deadline, cancelled)?;
        self.app.ensure_running(deadline, cancelled)?;
        if operation == ClockOperation::Advance {
            self.input.capture_basis = None;
        }
        self.requests
            .execute(
                id,
                operation,
                delta_ns,
                deadline.min(Instant::now() + Duration::from_secs(10)),
            )
            .map_err(io::Error::other)
    }
    /// Recover only the original dispatched request from this app's retained descriptor.
    /// Executing, stale, foreign or incomplete records leave dispatch unresolved.
    pub fn reconcile(
        &mut self,
        id: &RequestId,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Reply> {
        budget(deadline, cancelled)?;
        let descriptor = if self.retired.is_some() {
            self.directory.join("last-native-receipt.json")
        } else {
            self.app.container().join("Documents/clock-control.json")
        };
        self.requests
            .reconcile(id, &descriptor)
            .map_err(io::Error::other)
    }

    /// Native pixels, without claiming a settled presentation or advancing page time.
    pub fn input_basis(&self) -> Option<frontcap_core::native::contacts::CaptureInput> {
        self.input.input_basis()
    }
    pub fn capture(&mut self, id: RequestId, deadline: Instant, cancelled: &AtomicBool) -> io::Result<PathBuf> {
        let foreground=self.lifecycle.as_ref().is_none_or(|l| l.requests.is_foreground());
        self.input.capture(&mut self.app,foreground,id,deadline,cancelled)
    }
    pub fn stop(&mut self) -> Retirement {
        if let Some(retired) = &self.retired {
            return retired.clone();
        }
        // Device deletion removes the app container. Retain its last original receipt first,
        // including executing/unknown evidence; never promote it to completion automatically.
        let retained = crate::webview::read_receipt_bytes(
            &self.app.container().join("Documents/clock-control.json"),
        )
        .or_else(|error| {
            if error.kind() == io::ErrorKind::NotFound && self.app.retirement().is_some() {
                crate::webview::read_receipt_bytes(&self.directory.join("last-native-receipt.json"))
            } else { Err(error) }
        })
        .and_then(|bytes| {
            frontcap_core::state::save_atomic_bytes(
                &self.directory.join("last-native-receipt.json"),
                &bytes,
            )
        });
        let contact_retention = self
            .input.contacts
            .take()
            .map(|session| session.retain_pending())
            .transpose();
        // Pending evidence survives outside the device; the native lock must close before
        // app retirement can detach the Simulator data filesystem, even if retention failed.
        let lifecycle_retention = self
            .lifecycle
            .as_ref()
            .map(|lifecycle| {
                crate::webview::read_receipt_bytes(&lifecycle.descriptor).and_then(|bytes| {
                    frontcap_core::state::save_atomic_bytes(
                        &self.directory.join("last-lifecycle-receipt.json"),
                        &bytes,
                    )
                })
            })
            .transpose();
        let mut retirement = self.app.stop();
        if let Err(error) = lifecycle_retention {
            retirement.passed = false;
            retirement
                .errors
                .push(format!("lifecycle receipt retention failed: {error}"));
        }
        if let Err(error) = contact_retention {
            retirement.passed = false;
            retirement
                .errors
                .push(format!("contact receipt retention failed: {error}"));
        }
        if let Err(error) = retained {
            retirement.passed = false;
            retirement
                .errors
                .push(format!("WebView receipt retention failed: {error}"));
        }
        if let Err(error) = save_atomic(&self.directory.join("retirement.json"), &retirement) {
            retirement.passed = false;
            retirement
                .errors
                .push(format!("WebView retirement publication failed: {error}"));
        }
        self.retired = Some(retirement.clone());
        retirement
    }
}
impl Drop for Target {
    fn drop(&mut self) {
        self.stop();
    }
}
fn budget(deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "WebView target cancelled or deadline exceeded",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "webview_target_tests.rs"]
mod tests;
