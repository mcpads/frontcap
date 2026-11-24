//! Shared capture-bound native input for an owned simulator app, independent of page readiness.
use crate::simulator_app::App;
use frontcap_core::{
    ids::RequestId,
    native::contacts::{
        Endpoint, Operation as ContactOperation, Receipt as ContactReceipt, Window,
    },
    state::{load, save_atomic},
};
use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WindowState {
    Available(Window),
    Unavailable(EmptyWindow),
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyWindow {}

pub(crate) struct Input {
    pub(crate) contacts: Option<crate::contact_session::Session>,
    pub(crate) capture_basis: Option<CaptureBasis>,
    pub(crate) input_bindings:
        std::collections::BTreeMap<RequestId, (RequestId, ContactOperation, Window)>,
    directory: PathBuf,
}
#[derive(Clone, serde::Serialize)]
pub(crate) struct CaptureBasis {
    id: RequestId,
    path: PathBuf,
    sha256: String,
    window: Window,
}
impl Input {
    pub(crate) fn new(directory: &Path) -> Self {
        Self {
            contacts: None,
            capture_basis: None,
            input_bindings: Default::default(),
            directory: directory.into(),
        }
    }
    pub fn enable_contacts(
        &mut self,
        app: &mut App,
        owner: impl Into<crate::contact_session::Consumer>,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<()> {
        if self.contacts.is_some() {
            return Ok(());
        }
        let native = app.container().join("Documents/frontcap-hid");
        // A page endpoint can precede a visible native window. Both host routes must wait
        // within the original acquisition budget before advertising capture-bound input.
        loop {
            budget(deadline, cancelled)?;
            app.ensure_running(deadline, cancelled)?;
            if let Some(endpoint) = load::<Endpoint>(&native.join("endpoint.json"))? {
                endpoint
                    .validate(app.process().pid)
                    .map_err(io::Error::other)?;
                if let Some(WindowState::Available(window)) =
                    load::<WindowState>(&native.join("window.json"))?
                    && window.validate(&endpoint).is_ok()
                {
                    break;
                }
            }
            std::thread::sleep(
                Duration::from_millis(25).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        let session = crate::contact_session::Session::create(
            &native,
            &self.directory.join("contacts"),
            owner,
        )?;
        session.window()?; // Refuse old attachments or unsupported geometry before advertising input.
        self.contacts = Some(session);
        Ok(())
    }
    pub fn input(
        &mut self,
        app: &mut App,
        id: RequestId,
        observation: RequestId,
        operation: ContactOperation,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<ContactReceipt> {
        operation.validate().map_err(io::Error::other)?;
        if let Some((saved_observation, saved_operation, _window)) = self.input_bindings.get(&id) {
            if saved_observation != &observation || saved_operation != &operation {
                return Err(io::Error::other(
                    "observed contact request identity conflict",
                ));
            }
            return self
                .contacts
                .as_mut()
                .ok_or_else(|| io::Error::other("contact input unavailable"))?
                .reconcile(&id, deadline, cancelled);
        }
        budget(deadline, cancelled)?;
        app.ensure_running(deadline, cancelled)?;
        let basis = self
            .capture_basis
            .as_ref()
            .filter(|basis| basis.id == observation)
            .ok_or_else(|| io::Error::other("native input requires the latest unconsumed capture"))?
            .clone();
        if !std::fs::symlink_metadata(&basis.path)?.is_file()
            || frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(&basis.path)?)
                != basis.sha256
        {
            return Err(io::Error::other("input capture bytes changed"));
        }
        let contacts = self
            .contacts
            .as_mut()
            .ok_or_else(|| io::Error::other("contact input unavailable"))?;
        contacts.validate_new_input(&operation)?;
        if contacts.window()? != basis.window {
            return Err(io::Error::other("input capture window changed"));
        }
        save_atomic(
            &self.directory.join(format!("input-{}.json", id.as_str())),
            &(&observation, &operation, &basis),
        )?;
        self.input_bindings.insert(
            id.clone(),
            (observation, operation.clone(), basis.window.clone()),
        );
        self.capture_basis = None;
        contacts.execute_observed(id, operation, basis.window, deadline, cancelled)
    }
    pub fn reconcile_input(
        &mut self,
        id: &RequestId,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<ContactReceipt> {
        self.contacts
            .as_mut()
            .ok_or_else(|| io::Error::other("contact input unavailable"))?
            .reconcile(id, deadline, cancelled)
    }
    pub fn input_basis(&self) -> Option<frontcap_core::native::contacts::CaptureInput> {
        let basis = self.capture_basis.as_ref()?;
        let (expected_revision, remaining_requests) = self.contacts.as_ref()?.admission()?;
        Some(frontcap_core::native::contacts::CaptureInput {
            requires_cancel: self.contacts.as_ref()?.requires_cancel(),
            observation: basis.id.clone(),
            expected_revision,
            remaining_requests,
        })
    }
    pub fn capture(
        &mut self,
        app: &mut App,
        foreground: bool,
        id: RequestId,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<PathBuf> {
        budget(deadline, cancelled)?;
        self.capture_basis = None;
        let window = self
            .contacts
            .as_ref()
            .and_then(|session| session.admission().and_then(|_| session.window().ok()));
        let directory = self.directory.join(format!("capture-{}", id.as_str()));
        std::fs::create_dir(&directory)?;
        let path = directory.canonicalize()?.join("screen.png");
        app.capture_png(&path, deadline, cancelled)?;
        if let Some(window) = window.filter(|_| foreground) {
            let bytes = std::fs::read(&path)?;
            if bytes.len() < 24
                || !bytes.starts_with(b"\x89PNG\r\n\x1a\n")
                || u32::from_be_bytes(bytes[16..20].try_into().unwrap()) as f64
                    != window.width * window.scale
                || u32::from_be_bytes(bytes[20..24].try_into().unwrap()) as f64
                    != window.height * window.scale
                || self.contacts.as_ref().unwrap().window().ok().as_ref() != Some(&window)
            {
                save_atomic(
                    &directory.join("input-unavailable.json"),
                    &"capture does not match the current full-screen input window",
                )?;
                return Ok(path);
            }
            let basis = CaptureBasis {
                id,
                path: path.clone(),
                sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes),
                window,
            };
            save_atomic(&directory.join("input-basis.json"), &basis)?;
            self.capture_basis = Some(basis);
        }
        Ok(path)
    }
}
fn budget(deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "native input cancelled or deadline exceeded",
        ))
    } else {
        Ok(())
    }
}
