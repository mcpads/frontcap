//! Long-lived iOS target owner. Controller EOF revokes authority; evidence never grants adoption.
use crate::{
    device::AcquisitionError,
    ios_store::{Completion, Operation, Owner},
    ios_target::{IosLaunch, Target},
    worker::monotonic_ns,
};
use frontcap_core::{
    ids::RequestId,
    native::ownership::Retirement,
    process::ProcessIdentity,
    state::{load, save_atomic},
    write_budget::{Class, save_inherited},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, BufRead, Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
const MESSAGE_LIMIT: usize = 65536;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    config: IosLaunch,
    deadline_ns: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    Start,
    Execute {
        number: u64,
        id: RequestId,
        operation: Operation,
        deadline_ns: u64,
    },
    SetAppState { number: u64, id: RequestId, generation: String, state: frontcap_core::native::app_state::State, deadline_ns: u64 },
    Restart { number: u64, id: RequestId, generation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")] url: Option<String>, deadline_ns: u64 },
    DispatchUrl {
        number: u64,
        id: RequestId,
        request: frontcap_core::native::external_entry::Request,
        deadline_ns: u64,
    },
    Stop,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Completed { completion: Completion },
    Failed { error: String },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResult {
    pub error: Option<String>,
    pub retirement: Option<Retirement>,
    /// Set only by the worker before it invokes target acquisition. Older results lack proof.
    #[serde(default)]
    pub acquisition_not_started: bool,
}
#[derive(Debug)]
pub enum Recovery {
    Active,
    Finished {
        worker: ProcessIdentity,
        result: SessionResult,
    },
    ResourceCleanup { worker: ProcessIdentity },
    WorkerLost { worker: ProcessIdentity, retirement: Retirement },
    RetirementRecovered { worker: ProcessIdentity, result: SessionResult, retirement: Retirement },
    /// No terminal proof. Never authorizes signalling a saved PID or replacing an unretired target.
    Unresolved,
}
impl Recovery {
    pub fn retirement(self) -> Option<Retirement> {
        match self {
            Self::Finished { result, .. } => result.retirement,
            Self::WorkerLost { retirement, .. } | Self::RetirementRecovered { retirement, .. } => Some(retirement),
            _ => None,
        }
    }
}
pub fn inspect(directory: &Path) -> io::Result<Recovery> {
    let lock = File::options()
        .read(true)
        .write(true)
        .open(directory.join("session.lock"))?;
    let _lock = match crate::store::RequestLease::try_acquire(lock) {
        Ok(lease) => lease,
        Err(std::fs::TryLockError::WouldBlock) => return Ok(Recovery::Active),
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    };
    match (
        load::<ProcessIdentity>(&directory.join("worker.json"))?,
        load::<SessionResult>(&directory.join("result.json"))?,
    ) {
        (Some(worker), result) => {
            if result.as_ref().is_none_or(|r| !r.acquisition_not_started && r.retirement.as_ref().is_none_or(|r| !r.passed))
                && worker.has_exited()? {
                use crate::resource_group::inspection::Inspection;
                match inspect_resources(directory)? {
                    Inspection::Active => return Ok(Recovery::ResourceCleanup { worker }),
                    Inspection::Finished(group) => {
                        let retirement = Retirement { passed: group.passed, errors: group.errors,
                            signal_error: group.resources.values().find_map(|r| r.signal_error.clone()) };
                        match result {
                            None => return Ok(Recovery::WorkerLost { worker, retirement }),
                            Some(result) if retirement.passed => return Ok(Recovery::RetirementRecovered { worker, result, retirement }),
                            Some(result) => return Ok(Recovery::Finished { worker, result }),
                        }
                    }
                    Inspection::Unresolved => (),
                }
            }
            Ok(match result { Some(result) => Recovery::Finished { worker, result }, None => Recovery::Unresolved })
        }
        _ => Ok(Recovery::Unresolved),
    }
}
fn inspect_resources(directory: &Path) -> io::Result<crate::resource_group::inspection::Inspection> {
    use crate::resource_group::inspection::{self, Inspection};
    let Some(launch) = load::<Launch>(&directory.join("launch.json"))? else { return Ok(Inspection::Unresolved); };
    if launch.config.resource_lifetime_ms.is_none() { return Ok(Inspection::Unresolved); }
    launch.config.validate_resources()?;
    let root = directory.canonicalize()?;
    if let Some(acquired) = load::<crate::xctest::XctestLaunch>(&root.join("target/launch.json"))?
        && serde_json::to_value(acquired)? != serde_json::to_value(&launch.config.xctest)? {
        return Err(io::Error::other("UIKit resource target differs from original launch"));
    }
    if let Some(acquired) = load::<crate::simulator::SimulatorLaunch>(&root.join("target/simulator/launch.json"))?
        && serde_json::to_value(acquired)? != serde_json::to_value(&launch.config.xctest.simulator)? {
        return Err(io::Error::other("UIKit resource Simulator differs from original launch"));
    }
    inspection::inspect(&root.join("target-resources"), &root.join("target"), &[], None)
}
fn deadline_ns(deadline: Instant) -> io::Result<u64> {
    monotonic_ns()?
        .checked_add(
            u64::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_nanos(),
            )
            .map_err(io::Error::other)?,
        )
        .ok_or_else(|| io::Error::other("session deadline exceeds monotonic clock"))
}
fn instant(deadline_ns: u64) -> io::Result<Instant> {
    Instant::now()
        .checked_add(Duration::from_nanos(
            deadline_ns.saturating_sub(monotonic_ns()?),
        ))
        .ok_or_else(|| io::Error::other("session deadline exceeds host range"))
}
fn check(deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) {
        return Err(io::Error::other("native session cancelled"));
    }
    if Instant::now() >= deadline {
        return Err(io::Error::other("native session deadline exceeded"));
    }
    Ok(())
}

/// A serial command client. Dropping this handle revokes authority; it never kills the target owner.
pub struct Session {
    surface: crate::ios_execution::UiSurface,
    child: Option<Child>,
    control: Option<ChildStdin>,
    directory: PathBuf,
    number: u64,
    retirement_reader: Option<PathBuf>,
}
/// Cleanup obtained through the original startup handle, never reconstructed from saved files.
#[derive(Debug)]
pub enum StartupCleanup {
    Unproven,
    /// Fresh session metadata failed before any session or diagnostic worker could start.
    NoWritersStarted,
    /// Diagnostic setup failed without a collector, or its original child has been reaped.
    DiagnosticFailed { collector_pid: Option<u32> },
    /// A fresh directory and diagnostic output exist, but no session worker was spawned.
    WorkerNotStarted,
    /// The original worker exited with explicit proof that target acquisition never began.
    AcquisitionNotStarted { worker: ProcessIdentity },
    Retired(Retirement),
}
pub type BeforeStart<'a> = dyn FnMut(&Session) -> io::Result<()> + 'a;

pub struct Startup {
    pub result: io::Result<Session>,
    pub cleanup: StartupCleanup,
}
impl Session {
    /// Fixed read-only entrypoint from this original worker invocation, never a recovered executable.
    pub fn retirement_reader(&self) -> Option<&Path> { self.retirement_reader.as_deref() }
    pub fn start(
        executable: &Path,
        config: IosLaunch,
        directory: &Path,
        deadline: Instant,
    ) -> io::Result<Self> {
        Self::start_with_budget(executable, config, directory, deadline, None)
    }
    pub fn start_with_budget(
        executable: &Path, config: IosLaunch, directory: &Path, deadline: Instant,
        budget: Option<&frontcap_core::write_budget::WriteBudget>,
    ) -> io::Result<Self> {
        Self::start_with_budget_outcome(executable, config, directory, deadline, budget).result
    }
    pub fn start_with_budget_outcome(
        executable: &Path, config: IosLaunch, directory: &Path, deadline: Instant,
        budget: Option<&frontcap_core::write_budget::WriteBudget>,
    ) -> Startup {
        Self::start_with_observer(executable, config, directory, deadline, budget, None)
    }
    /// Register independent supervision before the first target acquisition command is sent.
    pub fn start_with_observer(executable: &Path, config: IosLaunch, directory: &Path, deadline: Instant,
        budget: Option<&frontcap_core::write_budget::WriteBudget>, before_start: Option<&mut BeforeStart<'_>>) -> Startup {
        let mut cleanup = StartupCleanup::Unproven;
        let result = Self::start_worker(executable, config, directory, deadline, budget, &mut cleanup, before_start);
        Startup { result, cleanup }
    }
    fn start_worker(
        executable: &Path, config: IosLaunch, directory: &Path, deadline: Instant,
        budget: Option<&frontcap_core::write_budget::WriteBudget>, cleanup: &mut StartupCleanup,
        before_start: Option<&mut BeforeStart<'_>>,
    ) -> io::Result<Self> {
        let retirement_reader = config.resource_lifetime_ms.map(|_| executable.canonicalize()).transpose()?;
        let launch = Launch {
            config,
            deadline_ns: deadline_ns(deadline)?,
        };
        std::fs::create_dir(directory)?;
        *cleanup = StartupCleanup::NoWritersStarted;
        File::create_new(directory.join("session.lock"))?;
        match budget {
            Some(budget) => budget.save_atomic(frontcap_core::write_budget::Class::Ordinary, &directory.join("launch.json"), &launch)?,
            None => save_atomic(&directory.join("launch.json"), &launch)?,
        }
        // Diagnostic initialization can spawn its own writer before returning an error.
        *cleanup = StartupCleanup::Unproven;
        let output = crate::diagnostic_log::create_output_with_budget_outcome(&directory.join("worker.log"), budget);
        let log = match output.result {
            Ok(log) => log,
            Err(error) => {
                *cleanup = match output.cleanup {
                    crate::diagnostic_log::OutputCleanup::NoCollectorStarted => StartupCleanup::DiagnosticFailed { collector_pid: None },
                    crate::diagnostic_log::OutputCleanup::CollectorExited { pid } => StartupCleanup::DiagnosticFailed { collector_pid: Some(pid) },
                    crate::diagnostic_log::OutputCleanup::Unproven => StartupCleanup::Unproven,
                };
                return Err(error);
            }
        };
        *cleanup = StartupCleanup::WorkerNotStarted;
        let mut command = Command::new(executable);
        frontcap_core::write_budget::WriteBudget::configure_child(&mut command, budget)?;
        let mut child = command
            .arg(directory.canonicalize()?)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()?;
        *cleanup = StartupCleanup::Unproven;
        let control = child.stdin.take().unwrap();
        let mut session = Self {
            surface: launch.config.surface,
            child: Some(child),
            control: Some(control),
            directory: directory.to_path_buf(),
            number: 0,
            retirement_reader,
        };
        let startup = (|| {
            let fd = session.control.as_ref().unwrap().as_raw_fd();
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFL);
                if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if let Some(before_start) = before_start {
                loop {
                    if session.child.as_mut().unwrap().try_wait()?.is_some() {
                        return Err(io::Error::other("iOS worker exited before acquisition registration"));
                    }
                    if directory.join("worker.json").exists() { session.worker_identity()?; break; }
                    check(deadline, &AtomicBool::new(false))?;
                    std::thread::sleep(Duration::from_millis(5));
                }
                before_start(&session)?;
            }
            session.send(&Message::Start, deadline, &AtomicBool::new(false))
        })();
        if let Err(error) = startup {
            // Keep the original handle until its worker has retired. Saved evidence alone
            // cannot establish which child still owns partially dispatched acquisition.
            match session.stop_startup(Instant::now() + Duration::from_secs(60)) {
                Ok(stopped) => { *cleanup = stopped; return Err(error); }
                Err(stopped) => return Err(io::Error::other(format!("iOS startup: {error}; target retirement: {stopped}"))),
            }
        }
        Ok(session)
    }
    pub fn worker_identity(&self) -> io::Result<ProcessIdentity> {
        let child = self.child.as_ref().ok_or_else(|| io::Error::other("iOS session worker handle absent"))?;
        let process = ProcessIdentity::of(child.id())?.ok_or_else(|| io::Error::other("iOS session worker exited"))?;
        if load::<ProcessIdentity>(&self.directory.join("worker.json"))?.as_ref() != Some(&process) {
            return Err(io::Error::other("iOS session worker record differs from original child"));
        }
        Ok(process)
    }
    pub fn surface(&self) -> crate::ios_execution::UiSurface {
        self.surface
    }
    pub fn system_alerts(&self) -> io::Result<bool> {
        let ready: Option<crate::xctest::UiReady> = load(&self.directory.join("target/ui-ready.json"))?;
        let owner: Option<Owner> = load(&self.directory.join("ready.json"))?;
        Ok(self.surface == crate::ios_execution::UiSurface::Uikit
            && ready.zip(owner).is_some_and(|(ready, owner)| ready.identity == owner.runner && ready.system_alerts))
    }
    pub fn restart_available(&self) -> io::Result<bool> {
        let ready: Option<crate::xctest::UiReady> = load(&self.directory.join("target/ui-ready.json"))?;
        let initial: Option<Owner> = load(&self.directory.join("ready.json"))?;
        Ok(self.surface == crate::ios_execution::UiSurface::Uikit
            && ready.zip(initial).is_some_and(|(r,o)|r.app_restart && r.identity==o.runner)
            && self.directory.join("target/restarts/history.json").exists())
    }
    pub fn app_lifecycle(&self) -> io::Result<bool> {
        let ready: Option<crate::xctest::UiReady> = load(&self.directory.join("target/ui-ready.json"))?;
        let initial: Option<Owner> = load(&self.directory.join("ready.json"))?;
        Ok(self.surface == crate::ios_execution::UiSurface::Uikit
            && ready.zip(initial).is_some_and(|(r,o)|r.app_lifecycle && r.identity==o.runner))
    }
    pub fn validate_foreground(&self) -> io::Result<()> {
        let (owner,epoch)=self.current_epoch()?;
        crate::ios_store::RequestStore::validate_foreground(&epoch.join("requests"),&owner).map_err(io::Error::other)
    }
    pub fn entry_available(&self) -> io::Result<bool> {
        let ready: Option<crate::xctest::UiReady> = load(&self.directory.join("target/ui-ready.json"))?;
        Ok(self.restart_available()? && ready.is_some_and(|r| r.app_entry))
    }
    pub fn current_epoch(&self) -> io::Result<(Owner,PathBuf)> {
        if !self.directory.join("target/restarts/history.json").exists() {
            let owner=load(&self.directory.join("ready.json"))?.ok_or_else(||io::Error::other("iOS session is not ready"))?;
            return Ok((owner,self.directory.join("target")));
        }
        crate::ios_restarts::epochs(&self.directory.join("target"))?.pop().ok_or_else(||io::Error::other("missing UIKit owner"))
    }
    pub fn validate_restart(&self,id:&RequestId,generation:&str,url:Option<&str>)->io::Result<()> {
        if !self.restart_available()? {return Err(io::Error::other("UIKit restart unavailable"));}
        if url.is_some() && !self.entry_available()? {return Err(io::Error::other("UIKit URL launch unavailable"));}
        let (owner,epoch)=self.current_epoch()?;
        let launch: Launch = load(&self.directory.join("launch.json"))?.ok_or_else(||io::Error::other("missing iOS launch"))?;
        frontcap_core::native::ios_lifecycle::restart_entry(&owner, &launch.config.xctest.app.bundle, url).map_err(io::Error::other)?;
        if owner.app.generation!=generation {return Err(io::Error::other("UIKit restart generation differs"));}
        crate::ios_store::RequestStore::validate_replacement(&epoch.join("requests"),&owner,id).map_err(io::Error::other)
    }
    pub fn validate_operation(&self, id: &RequestId, operation: &Operation) -> io::Result<()> {
        self.surface.validate_operation(operation)?;
        if matches!(operation, Operation::SetAppState {..}) && !self.app_lifecycle()? {
            return Err(io::Error::other("UIKit host transitions unavailable"));
        }
        let (owner,epoch)=self.current_epoch()?;
        frontcap_core::native::ios_request::Request::build_pair(&owner,operation,1,1).map_err(io::Error::other)?;
        crate::ios_store::RequestStore::validate_operation(&epoch.join("requests"),&owner,id,operation).map_err(io::Error::other)?;
        if matches!(operation, Operation::ObserveSystemAlerts | Operation::TapSystemButton { .. }) && !self.system_alerts()? {
            return Err(io::Error::other("XCTest runner does not advertise system alerts"));
        }
        if matches!(operation, Operation::TapSystemButton { .. } | Operation::TapTarget { .. } | Operation::PressTarget { .. } | Operation::ReplaceText { .. } | Operation::ScrollTarget { .. }) {
            let (owner, epoch) = self.current_epoch()?;
            crate::ios_store::RequestStore::validate_ui_input(
                &epoch.join("requests"), &owner, id, operation).map_err(io::Error::other)?;
        }
        Ok(())
    }
    fn send(
        &mut self,
        message: &Message,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        if bytes.len() > MESSAGE_LIMIT {
            return Err(io::Error::other("native session message too large"));
        }
        let mut offset = 0;
        while offset < bytes.len() {
            if let Err(error) = check(deadline, cancelled) {
                self.control.take();
                return Err(error);
            }
            let result = self
                .control
                .as_mut()
                .ok_or_else(|| io::Error::other("session control closed"))?
                .write(&bytes[offset..]);
            match result {
                Ok(0) => {
                    self.control.take();
                    return Err(io::Error::other("session write returned zero"));
                }
                Ok(n) => offset += n,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(error) => {
                    self.control.take();
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    pub fn wait_ready(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Owner> {
        loop {
            if let Err(error) = check(deadline, cancelled) {
                self.control.take();
                return Err(error);
            }
            if let Some(result) = load::<SessionResult>(&self.directory.join("result.json"))? {
                return Err(io::Error::other(format!(
                    "native session ended: {result:?}"
                )));
            }
            if self.child.as_mut().unwrap().try_wait()?.is_some() {
                return Err(io::Error::other("native session exited before readiness"));
            }
            if let Some(ready) = load(&self.directory.join("ready.json"))? {
                return Ok(ready);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    /// Caller ID retries return the saved completion. Dispatched uncertainty is never re-executed.
    pub fn execute(
        &mut self,
        id: RequestId,
        operation: Operation,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Reply> {
        if matches!(operation, Operation::SetAppState { .. }) {
            return Err(io::Error::other("use set_app_state to retain host process evidence"));
        }
        self.number = self
            .number
            .checked_add(1)
            .ok_or_else(|| io::Error::other("session sequence exhausted"))?;
        self.send(
            &Message::Execute {
                number: self.number,
                id,
                operation,
                deadline_ns: deadline_ns(deadline)?,
            },
            deadline,
            cancelled,
        )?;
        self.wait_reply(deadline, cancelled)
    }
    pub fn set_app_state(&mut self, id: RequestId, generation: String, state: frontcap_core::native::app_state::State,
        deadline: Instant,cancelled:&AtomicBool) -> io::Result<crate::ios_app_state::Reply> {
        self.validate_operation(&id,&Operation::SetAppState {generation:generation.clone(),state})?;
        self.number=self.number.checked_add(1).ok_or_else(||io::Error::other("session sequence exhausted"))?;
        self.send(&Message::SetAppState {number:self.number,id,generation,state,deadline_ns:deadline_ns(deadline)?},deadline,cancelled)?;
        self.wait_reply(deadline,cancelled)
    }
    pub fn restart(&mut self,id:RequestId,generation:String,url:Option<String>,deadline:Instant,cancelled:&AtomicBool)->io::Result<crate::ios_restarts::Reply> {
        self.validate_restart(&id,&generation,url.as_deref())?;
        self.number=self.number.checked_add(1).ok_or_else(||io::Error::other("session sequence exhausted"))?;
        self.send(&Message::Restart {number:self.number,id,generation,url,deadline_ns:deadline_ns(deadline)?},deadline,cancelled)?;
        self.wait_reply(deadline,cancelled)
    }
    pub fn dispatch_url(&mut self, id: RequestId,
        request: frontcap_core::native::external_entry::Request,
        deadline: Instant, cancelled: &AtomicBool,
    ) -> io::Result<crate::external_entry::Reply> {
        if self.surface != crate::ios_execution::UiSurface::Uikit {
            return Err(io::Error::other("external entry requires a UIKit owner"));
        }
        request.validate().map_err(io::Error::other)?;
        self.number = self.number.checked_add(1)
            .ok_or_else(|| io::Error::other("session sequence exhausted"))?;
        self.send(&Message::DispatchUrl { number: self.number, id, request,
            deadline_ns: deadline_ns(deadline)? }, deadline, cancelled)?;
        self.wait_reply(deadline, cancelled)
    }
    fn wait_reply<R: serde::de::DeserializeOwned>(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<R> {
        let reply = self
            .directory
            .join("commands")
            .join(self.number.to_string())
            .join("result.json");
        // Keep the operation budget; allow the bridge supervisor to publish cancellation evidence.
        let mut end = deadline + Duration::from_secs(5);
        loop {
            if cancelled.load(Ordering::Acquire) && self.control.is_some() {
                self.control.take();
                end = end.min(Instant::now() + Duration::from_secs(5));
            }
            if let Some(reply) = load(&reply)? {
                return Ok(reply);
            }
            if self.child.as_mut().unwrap().try_wait()?.is_some() {
                return Err(io::Error::other(
                    "session exited without command result; inspect saved request",
                ));
            }
            if Instant::now() >= end {
                self.control.take();
                return Err(io::Error::other(
                    "session command observation expired; inspect existing session",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    /// Stop the original startup owner and classify its terminal writer disposition.
    pub fn stop_startup(self, deadline: Instant) -> io::Result<StartupCleanup> {
        match self.stop(deadline)? {
            Recovery::Finished { worker, result } if result.acquisition_not_started && result.retirement.is_none() && result.error.is_some() =>
                Ok(StartupCleanup::AcquisitionNotStarted { worker }),
            Recovery::Finished { result, .. } if !result.acquisition_not_started && result.retirement.as_ref().is_some_and(|r| r.passed) =>
                Ok(StartupCleanup::Retired(result.retirement.unwrap())),
            Recovery::WorkerLost { retirement, .. } | Recovery::RetirementRecovered { retirement, .. } if retirement.passed =>
                Ok(StartupCleanup::Retired(retirement)),
            recovery => Err(io::Error::other(format!("iOS startup retirement unproven: {recovery:?}"))),
        }
    }
    pub fn stop(mut self, deadline: Instant) -> io::Result<Recovery> {
        if self.control.is_some() && self.child.as_mut().unwrap().try_wait()?.is_none() {
            // A failed send still closes authority and leaves the owner responsible for retirement.
            let _ = self.send(&Message::Stop, deadline, &AtomicBool::new(false));
        }
        self.control.take();
        loop {
            if self.child.as_mut().unwrap().try_wait()?.is_some() {
                let recovery = inspect(&self.directory)?;
                if !matches!(recovery, Recovery::ResourceCleanup { .. }) {
                    self.child.take();
                    return Ok(recovery);
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "session retirement unconfirmed; inspect existing session",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.control.take();
        if let Some(mut child) = self.child.take() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

/// Private worker entry point; restarting an existing directory never resumes mutations.
pub fn run(directory: &Path) -> io::Result<()> {
    run_with(directory, |config, target, deadline, cancelled| {
        config.validate_app_environment()?;
        let Some(lifetime_ms) = config.resource_lifetime_ms else {
            return Target::acquire(config, target, deadline, cancelled);
        };
        let parent = target.parent().ok_or_else(|| io::Error::other("UIKit session parent missing"))?.canonicalize()?;
        let executable = std::env::current_exe()?;
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME unavailable"))?;
        let group_config = crate::resource_group::Config {
            host: PathBuf::from(home).join(".cache/frontcap"), target: parent.join("target"),
            evidence_root: Some(parent.clone()), artifact_budget_bytes: None,
            temporary_storage_root: None, scratch_volume: None, scratch_volume_root: None,
            resource_executable: executable.clone(), resource_arguments: vec!["--resource-process".into()],
            ports: vec![], deadline_ns: monotonic_ns()?.checked_add(lifetime_ms * 1_000_000)
                .ok_or_else(|| io::Error::other("UIKit resource lifetime overflow"))?,
        };
        let mut command = Command::new(executable); command.arg("--resource-group");
        let group = crate::resource_group::Group::start(&mut command, &group_config,
            &parent.join("target-resources"), deadline)?;
        Target::acquire_with(config, target, deadline, cancelled,
            move |config, directory, deadline, cancelled| crate::xctest::Xctest::start_in_group(
                config, directory, deadline, cancelled, group))
    })
}
fn run_with(
    directory: &Path,
    acquire: impl FnOnce(IosLaunch, &Path, Instant, &AtomicBool) -> Result<Target, AcquisitionError>,
) -> io::Result<()> {
    let lock = File::options()
        .read(true)
        .write(true)
        .open(directory.join("session.lock"))?;
    lock.try_lock().map_err(io::Error::other)?;
    if directory.join("worker.json").exists() || directory.join("result.json").exists() {
        return Err(io::Error::other(
            "native session already started; inspect existing evidence",
        ));
    }
    // This single owner identity is required to interpret terminal cleanup even if ordinary
    // admission is exhausted before Start. It shares the bounded retirement allowance.
    save_inherited(Class::Retirement,
        &directory.join("worker.json"),
        &ProcessIdentity::of(std::process::id())?
            .ok_or_else(|| io::Error::other("cannot identify session owner"))?,
    )?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let stopping = Arc::new(AtomicBool::new(false));
    let (sender, commands) = mpsc::sync_channel(1);
    let signal = cancelled.clone();
    let stop = stopping.clone();
    std::thread::spawn(move || {
        let input = io::stdin();
        let mut input = input.lock();
        loop {
            let mut bytes = Vec::new();
            let read = input
                .by_ref()
                .take((MESSAGE_LIMIT + 1) as u64)
                .read_until(b'\n', &mut bytes);
            if read.is_err()
                || bytes.is_empty()
                || bytes.len() > MESSAGE_LIMIT
                || bytes.last() != Some(&b'\n')
            {
                break;
            }
            let Ok(message) = serde_json::from_slice::<Message>(&bytes) else {
                break;
            };
            if matches!(message, Message::Stop) {
                stop.store(true, Ordering::Release);
                break;
            }
            // The serial API never queues another request while one is waiting for dispatch.
            if sender.try_send(message).is_err() {
                break;
            }
        }
        signal.store(true, Ordering::Release);
    });
    let outcome = serve(directory, acquire, &commands, &cancelled, &stopping);
    let result = outcome.unwrap_or_else(|error| SessionResult {
        error: Some(error.to_string()),
        retirement: None,
        acquisition_not_started: false,
    });
    save_inherited(Class::Retirement, &directory.join("result.json"), &result)
}
fn serve(
    directory: &Path,
    acquire: impl FnOnce(IosLaunch, &Path, Instant, &AtomicBool) -> Result<Target, AcquisitionError>,
    commands: &mpsc::Receiver<Message>,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
) -> io::Result<SessionResult> {
    let launch: Launch = load(&directory.join("launch.json"))?
        .ok_or_else(|| io::Error::other("missing session launch"))?;
    let deadline = instant(launch.deadline_ns)?;
    if !matches!(
        commands.recv_timeout(deadline.saturating_duration_since(Instant::now())),
        Ok(Message::Start)
    ) {
        return Ok(SessionResult { error: Some("session not authorized before acquisition deadline".into()),
            retirement: None, acquisition_not_started: true });
    }
    if let Err(error) = check(deadline, cancelled) {
        return Ok(SessionResult { error: Some(error.to_string()), retirement: None, acquisition_not_started: true });
    }
    let mut device = match acquire(
        launch.config,
        &directory.join("target"),
        deadline,
        cancelled,
    ) {
        Ok(device) => device,
        Err(error) => {
            return Ok(SessionResult {
                error: Some(error.cause),
                retirement: error.retirement,
                acquisition_not_started: false,
            });
        }
    };
    // Every exit after acquisition, including storage failure, goes through explicit retirement.
    let work = serve_target(directory, &mut device, commands, cancelled, stopping);
    let retirement = device.stop();
    Ok(SessionResult {
        error: work.err().map(|e| e.to_string()),
        retirement: Some(retirement),
        acquisition_not_started: false,
    })
}
fn serve_target(
    directory: &Path,
    target: &mut Target,
    commands: &mpsc::Receiver<Message>,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
) -> io::Result<()> {
    std::fs::create_dir(directory.join("commands"))?;
    save_inherited(Class::Ordinary, &directory.join("ready.json"), target.owner())?;
    let mut expected = 1u64;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return if stopping.load(Ordering::Acquire) {
                Ok(())
            } else {
                Err(io::Error::other(
                    "controller disconnected or sent invalid commands",
                ))
            };
        }
        target.ensure_running()?;
        let message = match commands.recv_timeout(Duration::from_millis(20)) {
            Ok(message) => message,
            Err(_) => continue,
        };
        let number = match &message {
            Message::Execute { number, .. } | Message::DispatchUrl { number, .. } | Message::Restart { number, .. } | Message::SetAppState { number, .. } => number,
            _ => return Err(io::Error::other("unexpected iOS session start")),
        };
        if *number != expected {
            return Err(io::Error::other("out-of-order iOS session command"));
        }
        expected = expected
            .checked_add(1)
            .ok_or_else(|| io::Error::other("iOS sequence exhausted"))?;
        let attempt = directory.join("commands").join(number.to_string());
        std::fs::create_dir(&attempt)?;
        let class = if matches!(&message, Message::Execute { operation: Operation::Finish, .. }) {
            Class::Retirement
        } else { Class::Ordinary };
        // Admission precedes effects. Finish and terminal owner evidence retain a separate allowance.
        save_inherited(class, &attempt.join("command.json"), &message)?;
        if let Message::SetAppState {id,generation,state,deadline_ns,..}=message {
            let work=target.set_app_state(id,&generation,state,instant(deadline_ns)?,cancelled);
            let primary_error=work.as_ref().err().map(ToString::to_string);
            let reply=match work {
                Ok(transition)=>crate::ios_app_state::Reply::Completed {transition: Box::new(transition)},
                Err(error)=>crate::ios_app_state::Reply::Failed {error:error.to_string()},
            };
            let publication=save_inherited(class, &attempt.join("result.json"),&reply);
            if let Some(error)=primary_error {return Err(io::Error::other(error));}
            publication?;continue;
        }
        if let Message::Restart {id,generation,url,deadline_ns,..}=message {
            let work=target.restart(id,&generation,url.as_deref(),instant(deadline_ns)?,cancelled);
            let primary_error=work.as_ref().err().map(ToString::to_string);
            let reply=match work {
                Ok(replacement)=>crate::ios_restarts::Reply::Completed {replacement: Box::new(replacement)},
                Err(error)=>crate::ios_restarts::Reply::Failed {error:error.to_string()},
            };
            let publication=save_inherited(class, &attempt.join("result.json"),&reply);
            if let Some(error)=primary_error {return Err(io::Error::other(error));}
            publication?;
            save_inherited(Class::Ordinary, &directory.join("current-ready.json"),target.owner())?;
            continue;
        }
        if let Message::DispatchUrl { id, request, deadline_ns, .. } = message {
            let work = target.dispatch_url(id, request, instant(deadline_ns)?, cancelled);
            let primary_error = work.as_ref().err().map(ToString::to_string);
            let reply = match work {
                Ok(acceptance) => crate::external_entry::Reply::Accepted { acceptance },
                Err(error) => crate::external_entry::Reply::Failed { error: error.to_string() },
            };
            let publication = save_inherited(class, &attempt.join("result.json"), &reply);
            if let Some(error) = primary_error { return Err(io::Error::other(error)); }
            publication?;
            continue;
        }
        let Message::Execute { id, operation, deadline_ns, .. } = &message else { unreachable!() };
        let work = target.execute(
            id.clone(),
            operation.clone(),
            instant(*deadline_ns)?,
            cancelled,
        );
        let primary_error = work.as_ref().err().map(ToString::to_string);
        let reply = match work {
            Ok(completion) => Reply::Completed { completion },
            Err(error) => Reply::Failed {
                error: error.to_string(),
            },
        };
        let publication = save_inherited(class, &attempt.join("result.json"), &reply);
        if let Err(error) = &publication {
            eprintln!("iOS result publication failed: {error}");
        }
        if let Some(error) = primary_error {
            return Err(io::Error::other(error));
        }
        publication?;
        // Finish ends the owned target, not merely its app. Do not wait for another command.
        if matches!(operation, Operation::Finish) {
            return Ok(());
        }
    }
}
#[cfg(test)]
#[path = "ios_session_tests.rs"]
mod tests;
