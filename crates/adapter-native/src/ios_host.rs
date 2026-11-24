//! Native simulator ownership and capture-bound input before any WebView exists.
use crate::{
    device::AcquisitionError,
    simulator::{Simulator, SimulatorLaunch},
    simulator_app::{App, Launch},
    simulator_input::Input,
};
use frontcap_core::{
    ids::RequestId,
    native::{contacts, ios_host::Owner, ownership::Retirement},
    state::load,
    write_budget::{Class, save_inherited},
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "crate::webview_session::default_lifetime_ms")]
    pub lifetime_ms: u64,
    pub simulator: SimulatorLaunch,
    pub app: Launch,
    pub contacts: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_ui: Option<crate::ios_host::system::Config>,
}
pub use frontcap_core::native::ios_host::Capture;
#[path = "ios_host_entry.rs"]
pub(crate) mod entry;
#[path = "ios_host_cold_entry.rs"]
pub(crate) mod cold_entry;
#[path = "ios_host_visibility.rs"]
pub(crate) mod visibility;
#[path = "ios_host_system.rs"]
pub mod system;

#[cfg(test)]
use frontcap_core::state::save_atomic;

fn save_evidence<T: serde::Serialize>(path: &Path, value: &T) -> io::Result<()> {
    save_inherited(Class::Ordinary, path, value)
}

pub struct Target {
    app: App,
    owner: Owner,
    input: Input,
    directory: PathBuf,
    retired: Option<Retirement>,
    pending_entry: Option<cold_entry::Pending>,
    visibility: Option<frontcap_core::native::app_state::State>,
    pages: Option<crate::ios_host_pages::Pages>,
    pub(crate) system: Option<crate::ios_host::system::System>,
}
impl Target {
    pub fn acquire(
        config: Config,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Self, AcquisitionError> {
        Self::acquire_owned(config, directory, deadline, cancelled, None)
    }
    pub(crate) fn acquire_owned(config: Config, directory: &Path, deadline: Instant,
        cancelled: &AtomicBool, group: Option<crate::resource_group::Group>) -> Result<Self, AcquisitionError> {
        config.validate()?;
        if let Some(group) = &group {
            if directory.canonicalize()? != group.ready.target { return Err(io::Error::other("iOS group target differs").into()); }
        } else { std::fs::create_dir(directory)?; }
        save_evidence(&directory.join("launch.json"), &config)?;
        let source = config.simulator.source_sha256.clone();
        let simulator = if let Some(group) = group {
            Simulator::acquire_supervised(config.simulator, group, deadline, cancelled)?
        } else { Simulator::acquire(config.simulator, &directory.join("simulator"), deadline, cancelled)? };
        let app = App::launch(
            simulator,
            config.app,
            &directory.join("app"),
            deadline,
            cancelled,
        )?;
        let mut target = Self::connect(
            app,
            &directory.join("host"),
            config.contacts,
            deadline,
            cancelled,
        )?;
        if let Some(system) = config.system_ui {
            target.enable_system(system, &source, deadline, cancelled)?;
        }
        Ok(target)
    }
    pub fn connect(
        mut app: App,
        directory: &Path,
        contacts: bool,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Self, AcquisitionError> {
        let work = Self::initialize(&mut app, directory, contacts, deadline, cancelled);
        match work {
            Ok((owner, input, pages)) => Ok(Self {
                app,
                owner,
                input,
                directory: directory.canonicalize()?,
                retired: None,
                pending_entry: None,
                visibility: None,
                pages,
                system: None,
            }),
            Err(error) => Err(AcquisitionError {
                cause: error.to_string(),
                retirement: Some(app.stop()),
            }),
        }
    }
    fn initialize(
        app: &mut App,
        directory: &Path,
        contacts: bool,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<(Owner, Input, Option<crate::ios_host_pages::Pages>)> {
        std::fs::create_dir(directory)?;
        let owner = Owner {
            simulator: app.simulator_id().into(),
            process: app.process().clone(),
            attachment_sha256: app.attachment_sha256().into(),
        };
        owner.validate().map_err(io::Error::other)?;
        let mut input = Input::new(directory);
        if contacts {
            input.enable_contacts(app, owner.clone(), deadline, cancelled)?;
        }
        app.ensure_running(deadline, cancelled)?;
        save_evidence(&directory.join("ready.json"), &owner)?;
        let pages = if app.page_registry() {
            let mut pages =
                crate::ios_host_pages::Pages::new(&directory.join("pages"), owner.clone())?;
            loop {
                match pages.discover(app, deadline, cancelled) {
                    Ok(_) => break,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        std::thread::sleep(Duration::from_millis(25))
                    }
                    Err(error) => return Err(error),
                }
            }
            Some(pages)
        } else {
            None
        };
        Ok((owner, input, pages))
    }
    /// Replace the owned process in the same installed data container. Producer evidence stays
    /// in its original directory; fresh input and page authority are published together.
    pub fn restart(
        &mut self,
        previous: &Owner,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<frontcap_core::native::ios_host::Ready, AcquisitionError> {
        if previous != &self.owner || self.retired.is_some()
            || cancelled.load(Ordering::Acquire) || Instant::now() >= deadline
        {
            return Err(AcquisitionError {
                cause: "native host restart owner or budget differs".into(), retirement: None,
            });
        }
        // Reject a reused attempt directory before touching the current process.
        std::fs::create_dir(directory)?;
        let work = self.restart_inner(directory, deadline, cancelled);
        work.map_err(|error| {
            let _ = save_evidence(&directory.join("failure.json"), &error.to_string());
            AcquisitionError { cause: error.to_string(), retirement: Some(self.stop()) }
        })
    }
    fn restart_inner(
        &mut self, directory: &Path, deadline: Instant, cancelled: &AtomicBool,
    ) -> io::Result<frontcap_core::native::ios_host::Ready> {
        let directory = directory.canonicalize()?;
        self.app.ensure_running(deadline, cancelled)?;
        let previous = self.owner.clone();
        let system_launch = self.system_launch();
        let contacts = self.input.contacts.is_some();
        let old_generation = self.contact_endpoint().map(|e| e.generation.clone());
        save_evidence(&directory.join("replacement-intent.json"), &serde_json::json!({
            "previous":previous, "previous_evidence":self.directory,
            "contacts":contacts, "pages":self.pages.is_some()
        }))?;
        if let Some(input) = &self.input.contacts { input.retain_pending()?; }
        if let Some(pages) = &self.pages { pages.retain(&self.app)?; }
        // No reference from the old process can authorize input in the new one, even if
        // replacement fails after termination or before the new producers become ready.
        self.input = Input::new(&directory.join("host"));
        self.pages = None;
        self.system = None;
        self.directory = directory.clone();
        self.app.restart(&directory.join("app"), deadline, cancelled)
            .map_err(|e| io::Error::other(e.cause))?;
        let (owner, input, pages) = Self::initialize(
            &mut self.app, &directory.join("host"), contacts, deadline, cancelled)?;
        if owner.process == previous.process || owner.simulator != previous.simulator
            || owner.attachment_sha256 != previous.attachment_sha256
            || (contacts && input.contacts.as_ref().map(|s| &s.endpoint().generation) == old_generation.as_ref())
        {
            return Err(io::Error::other("native host replacement reused or changed producer identity"));
        }
        self.visibility = None;
        self.owner = owner;
        self.input = input;
        self.pages = pages;
        self.directory = directory.join("host");
        if let Some((config, source)) = system_launch {
            self.enable_system(config, &source, deadline, cancelled).map_err(|e| io::Error::other(e.cause))?;
        }
        let ready = frontcap_core::native::ios_host::Ready {
            owner: self.owner.clone(), contacts: self.contact_endpoint().cloned(),
            pages: self.pages_available(), app_lifecycle: self.app_lifecycle(), system_ui: self.system_identity().cloned(),
        };
        save_evidence(&directory.join("replacement-ready.json"), &ready)?;
        Ok(ready)
    }
    pub fn system_identity(&self) -> Option<&frontcap_core::native::ios_ui::UiIdentity> {
        self.system.as_ref().map(|system| &system.ready.identity)
    }
    pub(crate) fn evidence_directory(&self) -> &Path { &self.directory }
    pub fn pages_available(&self) -> bool {
        self.pages.is_some()
    }
    pub fn discover_pages(
        &mut self,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<frontcap_core::native::ios_host::Inventory> {
        self.pages
            .as_mut()
            .ok_or_else(|| io::Error::other("page registry unavailable"))?
            .discover(&mut self.app, deadline, cancelled)
    }
    pub fn attach_page(
        &mut self,
        id: &RequestId,
        page: &frontcap_core::native::ios_host::Page,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<frontcap_core::native::ios_host::Attachment> {
        self.input.capture_basis = None;
        self.pages
            .as_mut()
            .ok_or_else(|| io::Error::other("page registry unavailable"))?
            .attach(&mut self.app, id, page, deadline, cancelled)
    }
    pub fn page(
        &mut self,
        attachment: &RequestId,
        id: &RequestId,
        command: &frontcap_core::native::ios_host::PageOperation,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<crate::webview::Reply> {
        if matches!(
            command,
            frontcap_core::native::ios_host::PageOperation::Advance { .. }
        ) {
            self.input.capture_basis = None;
        }
        self.pages
            .as_mut()
            .ok_or_else(|| io::Error::other("page registry unavailable"))?
            .execute(&mut self.app, attachment, id, command, deadline, cancelled)
    }
    pub fn release_page(&mut self, attachment: &RequestId, evidence: &Path)
        -> io::Result<frontcap_core::native::ios_host::Attachment> {
        let released = self.pages.as_mut()
            .ok_or_else(|| io::Error::other("page registry unavailable"))?
            .release(&self.app, attachment, evidence)?;
        self.input.capture_basis = None;
        Ok(released)
    }
    pub fn retire_page(
        &mut self,
        attachment: &RequestId,
        evidence: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<frontcap_core::native::ios_host::Attachment> {
        let retired = self
            .pages
            .as_mut()
            .ok_or_else(|| io::Error::other("page registry unavailable"))?
            .retire(&mut self.app, attachment, evidence, deadline, cancelled)?;
        self.input.capture_basis = None;
        Ok(retired)
    }
    pub fn ensure_process_running(
        &mut self,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<()> {
        if self.pending_entry.is_some() { self.app.ensure_platform_running(deadline, cancelled) }
        else { self.app.ensure_running(deadline, cancelled) }
    }
    pub fn owner(&self) -> &Owner {
        &self.owner
    }
    pub fn contact_endpoint(&self) -> Option<&contacts::Endpoint> {
        self.input.contacts.as_ref().map(|s| s.endpoint())
    }
    pub fn capture(
        &mut self,
        id: RequestId,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Capture> {
        let path = self
            .input
            .capture(&mut self.app, true, id, deadline, cancelled)?;
        let bytes = std::fs::read(&path)?;
        Ok(Capture {
            owner: self.owner.clone(),
            path,
            sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes),
            bytes: bytes.len() as u64,
            input: self.input.input_basis(),
        })
    }
    pub fn input(
        &mut self,
        id: RequestId,
        observation: RequestId,
        operation: contacts::Operation,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<contacts::Receipt> {
        self.input.input(
            &mut self.app,
            id,
            observation,
            operation,
            deadline,
            cancelled,
        )
    }
    pub fn reconcile_input(
        &mut self,
        id: &RequestId,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<contacts::Receipt> {
        self.input.reconcile_input(id, deadline, cancelled)
    }
    pub fn stop(&mut self) -> Retirement {
        if let Some(retired) = &self.retired {
            return retired.clone();
        }
        let retained = self
            .input
            .contacts
            .take()
            .map(|s| s.retain_pending())
            .transpose();
        // Retain pending evidence, then drop the native requests.lock before deleting/detaching
        // Simulator data. Keeping this session alive would itself prevent volume retirement.
        let page_retention = self
            .pages
            .as_ref()
            .map(|pages| pages.retain_for_stop(&self.app))
            .transpose();
        let mut retirement = self.app.stop();
        if let Err(error) = page_retention {
            retirement.passed = false;
            retirement
                .errors
                .push(format!("native host page retention failed: {error}"));
        }
        if let Err(error) = retained {
            retirement.passed = false;
            retirement
                .errors
                .push(format!("native host input retention failed: {error}"));
        }
        if let Err(error) = save_inherited(Class::Retirement, &self.directory.join("retirement.json"), &retirement) {
            retirement.passed = false;
            retirement.errors.push(format!(
                "native host retirement publication failed: {error}"
            ));
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

#[cfg(test)]
#[path = "ios_host_tests.rs"]
mod tests;
