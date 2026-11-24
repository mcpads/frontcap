//! Long-lived external WebView target owner. Controller EOF revokes authority; evidence never grants adoption.
use crate::{
    device::AcquisitionError,
    simulator::{Simulator, SimulatorLaunch},
    simulator_app::{App, Launch as AppLaunch},
    webview_store::Owner,
    webview_target::Target,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_lifetime_ms")]
    pub lifetime_ms: u64,
    #[serde(default)]
    pub lifecycle: Option<crate::xctest::RunnerLaunch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_ui: Option<crate::ios_host::system::Config>,
    #[serde(default)]
    pub contacts: bool,
    #[serde(default)]
    pub service_url_environment: Option<String>,
    pub simulator: SimulatorLaunch,
    pub app: AppLaunch,
}
pub(crate) fn default_lifetime_ms() -> u64 { 900_000 }
pub use frontcap_core::native::webview::{Completion, Operation};
fn acquire(
    config: Config,
    directory: &Path,
    deadline: Instant,
    cancelled: &AtomicBool,
    group: crate::resource_group::Group,
) -> Result<Target, AcquisitionError> {
    if config.system_ui.is_some() { return Err(io::Error::other("system runner requires native host runtime").into()); }
    if config.app.script.is_none() || config.app.page_registry { return Err(io::Error::other("WebView session requires its single-page document-start script").into()); }
    if directory.canonicalize()? != group.ready.target { return Err(io::Error::other("iOS group target differs").into()); }
    let simulator = Simulator::acquire_supervised(config.simulator, group, deadline, cancelled)?;
    let app = App::launch(
        simulator,
        config.app,
        &directory.join("app"),
        deadline,
        cancelled,
    )?;
    let mut target = Target::connect(app, &directory.join("webview"), deadline, cancelled)?;
    if config.contacts && let Err(error) = target.enable_contacts(deadline, cancelled) {
        let retirement = target.stop();
        return Err(AcquisitionError { cause: error.to_string(), retirement: Some(retirement) });
    }
    if let Some(runner) = config.lifecycle {
        target.enable_lifecycle(runner, deadline, cancelled)?;
    }
    Ok(target)
}
const MESSAGE_LIMIT: usize = 65536;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Launch {
    #[serde(default)]
    pub(crate) host: bool,
    pub(crate) config: Config,
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
    Stop,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Completed { completion: Box<Completion> },
    Failed { error: String },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionResult {
    pub error: Option<String>,
    pub retirement: Option<Retirement>,
}
#[derive(Debug)]
pub enum Recovery {
    Active,
    Finished {
        worker: ProcessIdentity,
        result: SessionResult,
    },
    RetirementRecovered { worker: ProcessIdentity, result: SessionResult, retirement: Retirement },
    ResourceCleanup { worker: ProcessIdentity },
    WorkerLost { worker: ProcessIdentity, retirement: Retirement },
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
fn inspection_lock(directory: &Path) -> io::Result<Option<crate::store::RequestLease>> {
    let file = File::options().read(true).write(true).open(directory.join("session.lock"))?;
    match crate::store::RequestLease::try_acquire(file) {
        Ok(lease) => Ok(Some(lease)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error),
    }
}

pub fn inspect(directory: &Path) -> io::Result<Recovery> {
    let Some(_lock) = inspection_lock(directory)? else { return Ok(Recovery::Active); };
    match (
        load::<ProcessIdentity>(&directory.join("worker.json"))?,
        load(&directory.join("result.json"))?,
    ) {
        (Some(worker), Some(result)) => {
            let result: SessionResult = result;
            if result.retirement.as_ref().is_some_and(|r| r.passed) && !bundle_retirement_complete(directory)? {
                return Ok(Recovery::Unresolved);
            }
            if result.retirement.as_ref().is_none_or(|r| !r.passed) && worker.has_exited()? {
                match inspect_resources(directory)? {
                    crate::resource_group::inspection::Inspection::Active => return Ok(Recovery::ResourceCleanup { worker }),
                    crate::resource_group::inspection::Inspection::Finished(group) if group.passed => {
                        return Ok(Recovery::RetirementRecovered { worker, result, retirement: group_retirement(*group) });
                    }
                    _ => (),
                }
            }
            Ok(Recovery::Finished { worker, result })
        }
        (Some(worker), None) => {
            if !worker.has_exited()? { return Ok(Recovery::Unresolved); }
            match inspect_resources(directory)? {
                crate::resource_group::inspection::Inspection::Active => Ok(Recovery::ResourceCleanup { worker }),
                crate::resource_group::inspection::Inspection::Unresolved => Ok(Recovery::Unresolved),
                crate::resource_group::inspection::Inspection::Finished(group) => {
                    Ok(Recovery::WorkerLost { worker, retirement: group_retirement(*group) })
                }
            }
        }
        _ => Ok(Recovery::Unresolved),
    }
}


fn inspect_resources(directory: &Path) -> io::Result<crate::resource_group::inspection::Inspection> {
    use crate::resource_group::inspection::{self, Inspection};
    let Some(launch) = load::<Launch>(&directory.join("launch.json"))? else { return Ok(Inspection::Unresolved); };
    let root = directory.canonicalize()?;
    if let Some(config) = load::<SimulatorLaunch>(&root.join("target/simulator/launch.json"))?
        && serde_json::to_value(config)? != serde_json::to_value(launch.config.simulator)? {
        return Err(io::Error::other("resource group Simulator differs from original launch"));
    }
    let outcome = inspection::inspect(&root.join("target-resources"), &root.join("target"), &[], None)?;
    if matches!(&outcome, Inspection::Finished(group) if group.passed) && !bundle_retirement_complete(directory)? {
        return Ok(Inspection::Unresolved);
    }
    Ok(outcome)
}
fn bundle_retirement_complete(directory: &Path) -> io::Result<bool> {
    let Some(launch) = load::<Launch>(&directory.join("launch.json"))? else { return Ok(true); };
    let runner = if let Some(lifecycle) = launch.config.lifecycle {
        lifecycle
    } else if let Some(system) = launch.config.system_ui {
        system.runner_launch(&launch.config.app.bundle)
    } else { return Ok(true); };
    if runner.result_bundle_volume.is_none() { return Ok(true); }
    // A dead session and a retired resource group do not authorize detaching a separate volume.
    // Compare every acquired runner with the original session configuration, never its own plan alone.
    if !std::fs::symlink_metadata(directory)?.is_dir() { return Ok(false); }
    let directory = directory.canonicalize()?;
    Ok(crate::xctest::bundle_volume::inspect_target(&directory.join("target"), &runner).is_ok())
}
fn group_retirement(group: crate::resource_group::Retirement) -> Retirement {
    let signal_error = group.resources.values().find_map(|resource| resource.signal_error.clone());
    Retirement { passed: group.passed, errors: group.errors, signal_error }
}

#[path = "webview_session_recovery.rs"]
mod recovery;
pub use recovery::read_clock_result;

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
    child: Option<Child>,
    control: Option<ChildStdin>,
    directory: PathBuf,
    number: u64,
}
impl Session {
    pub fn start(
        executable: &Path,
        config: Config,
        directory: &Path,
        deadline: Instant,
    ) -> io::Result<Self> {
        Self::start_with_budget(executable, config, directory, deadline, None)
    }
    pub fn start_with_budget(executable: &Path, config: Config, directory: &Path, deadline: Instant,
        budget: Option<&frontcap_core::write_budget::WriteBudget>) -> io::Result<Self> {
        if config.app.page_registry {return Err(io::Error::other("page registry requires native host runtime"));}
        Self::start_mode(executable, config, directory, deadline, false, budget)
    }
    pub fn start_host(executable: &Path, config: crate::ios_host::Config, directory: &Path, deadline: Instant) -> io::Result<Self> {
        Self::start_host_with_budget(executable, config, directory, deadline, None)
    }
    pub fn start_host_with_budget(executable: &Path, config: crate::ios_host::Config, directory: &Path, deadline: Instant,
        budget: Option<&frontcap_core::write_budget::WriteBudget>) -> io::Result<Self> {
        config.validate()?;
        Self::start_mode(executable, Config { lifetime_ms: config.lifetime_ms, simulator: config.simulator, app: config.app, contacts: config.contacts, system_ui: config.system_ui, lifecycle: None, service_url_environment: None }, directory, deadline, true, budget)
    }
    fn start_mode(executable: &Path, config: Config, directory: &Path, deadline: Instant, host: bool, budget: Option<&frontcap_core::write_budget::WriteBudget>) -> io::Result<Self> {
        if config.lifetime_ms == 0 || config.lifetime_ms > 900_000 {
            return Err(io::Error::other("iOS resource lifetime must be 1..900000 milliseconds"));
        }
        let launch = Launch {
            host,
            config,
            deadline_ns: deadline_ns(deadline)?,
        };
        std::fs::create_dir(directory)?;
        File::create_new(directory.join("session.lock"))?;
        match budget {
            Some(budget) => budget.save_atomic(frontcap_core::write_budget::Class::Ordinary, &directory.join("launch.json"), &launch)?,
            None => save_atomic(&directory.join("launch.json"), &launch)?,
        }
        let log = crate::diagnostic_log::create_output_with_budget(&directory.join("worker.log"), budget)?;
        let mut command = Command::new(executable);
        frontcap_core::write_budget::WriteBudget::configure_child(&mut command, budget)?;
        let mut child = command
            .arg(directory.canonicalize()?)
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()?;
        let control = child.stdin.take().unwrap();
        let mut session = Self {
            child: Some(child),
            control: Some(control),
            directory: directory.to_path_buf(),
            number: 0,
        };
        let fd = session.control.as_ref().unwrap().as_raw_fd();
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        session.send(&Message::Start, deadline, &AtomicBool::new(false))?;
        Ok(session)
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
        self.wait_record("ready.json", deadline, cancelled)
    }
    pub fn wait_host_ready(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<frontcap_core::native::ios_host::Ready> {
        self.wait_record("host-ready.json", deadline, cancelled)
    }
    fn wait_record<T: serde::de::DeserializeOwned>(&mut self, name: &str, deadline: Instant, cancelled: &AtomicBool) -> io::Result<T> {
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
            if let Some(ready) = load(&self.directory.join(name))? {
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
    fn wait_reply(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Reply> {
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
    pub fn stop(mut self, deadline: Instant) -> io::Result<Recovery> {
        if self.control.is_some() && self.child.as_mut().unwrap().try_wait()?.is_none() {
            // A failed send still closes authority and leaves the owner responsible for retirement.
            let _ = self.send(&Message::Stop, deadline, &AtomicBool::new(false));
        }
        self.control.take();
        loop {
            if let Some(child) = self.child.as_mut() && child.try_wait()?.is_some() { self.child.take(); }
            if self.child.is_none() {
                let outcome = inspect(&self.directory)?;
                if !matches!(outcome, Recovery::ResourceCleanup { .. }) { return Ok(outcome); }
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
enum Owned { Page(Box<Target>), Host(Box<crate::ios_host::Target>) }
impl Owned {
    fn stop(&mut self) -> Retirement { match self { Self::Page(t) => t.stop(), Self::Host(t) => t.stop() } }
    fn ensure_process_running(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        match self { Self::Page(t) => t.ensure_process_running(deadline,cancelled), Self::Host(t) => t.ensure_process_running(deadline,cancelled) }
    }
}
pub fn run(directory: &Path) -> io::Result<()> {
    let launch: Launch = load(&directory.join("launch.json"))?.ok_or_else(|| io::Error::other("missing session launch"))?;
    if launch.config.lifetime_ms == 0 || launch.config.lifetime_ms > 900_000 {
        return Err(io::Error::other("iOS resource lifetime must be 1..900000 milliseconds"));
    }
    let lifetime = launch.deadline_ns.checked_add(launch.config.lifetime_ms * 1_000_000)
        .ok_or_else(|| io::Error::other("iOS resource lifetime overflow"))?;
    run_with(directory, move |config, directory, deadline, cancelled, host| {
        let parent = directory.parent().ok_or_else(|| io::Error::other("session directory absent"))?.canonicalize()?;
        let executable = std::env::current_exe()?;
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME unavailable"))?;
        let group_config = crate::resource_group::Config { host: PathBuf::from(home).join(".cache/frontcap"),
            target: parent.join("target"), evidence_root: Some(parent.clone()), artifact_budget_bytes: None, temporary_storage_root: None, scratch_volume: None, scratch_volume_root: None,
            resource_executable: executable.clone(), resource_arguments: vec!["--resource-process".into()],
            ports: vec![], deadline_ns: lifetime };
        let mut command = Command::new(executable); command.arg("--resource-group");
        let group = crate::resource_group::Group::start(&mut command, &group_config, &parent.join("target-resources"), deadline)?;
        if host {
            if config.lifecycle.is_some() || config.service_url_environment.is_some() { return Err(io::Error::other("native host launch contains page configuration").into()); }
            crate::ios_host::Target::acquire_owned(crate::ios_host::Config { lifetime_ms: config.lifetime_ms, simulator: config.simulator, app: config.app, contacts: config.contacts, system_ui: config.system_ui },directory,deadline,cancelled,Some(group)).map(|t|Owned::Host(Box::new(t)))
        } else { acquire(config,directory,deadline,cancelled,group).map(|t|Owned::Page(Box::new(t))) }
    })
}
fn save_evidence<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    save_inherited(Class::Ordinary, path, value)
}

fn run_with(
    directory: &Path,
    acquire: impl FnOnce(Config, &Path, Instant, &AtomicBool, bool) -> Result<Owned, AcquisitionError>,
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
    save_evidence(
        &directory.join("worker.json"),
        &ProcessIdentity::of(std::process::id())?
            .ok_or_else(|| io::Error::other("cannot identify session owner"))?,
    )?;
    let cancelled = Arc::new(AtomicBool::new(false));
    let stopping = Arc::new(AtomicBool::new(false));
    let disconnected = Arc::new(AtomicBool::new(false));
    let (sender, commands) = mpsc::sync_channel(1);
    let signal = cancelled.clone();
    let stop = stopping.clone();
    let disconnect = disconnected.clone();
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
        if !stop.load(Ordering::Acquire) {
            disconnect.store(true, Ordering::Release);
        }
        signal.store(true, Ordering::Release);
    });
    let outcome = serve(directory, acquire, &commands, &cancelled, &stopping, &disconnected);
    let result = outcome.unwrap_or_else(|error| SessionResult {
        error: Some(error.to_string()),
        retirement: None,
    });
    save_inherited(Class::Retirement, &directory.join("result.json"), &result)
}
fn serve(
    directory: &Path,
    acquire: impl FnOnce(Config, &Path, Instant, &AtomicBool, bool) -> Result<Owned, AcquisitionError>,
    commands: &mpsc::Receiver<Message>,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
    disconnected: &AtomicBool,
) -> io::Result<SessionResult> {
    let launch: Launch = load(&directory.join("launch.json"))?
        .ok_or_else(|| io::Error::other("missing session launch"))?;
    let deadline = instant(launch.deadline_ns)?;
    if !matches!(
        commands.recv_timeout(deadline.saturating_duration_since(Instant::now())),
        Ok(Message::Start)
    ) {
        return Err(io::Error::other(
            "session not authorized before acquisition deadline",
        ));
    }
    check(deadline, cancelled)?;
    let mut device = match acquire(
        launch.config,
        &directory.join("target"),
        deadline,
        cancelled,
        launch.host,
    ) {
        Ok(device) => device,
        Err(error) => {
            return Ok(SessionResult {
                error: Some(error.cause),
                retirement: error.retirement,
            });
        }
    };
    // Every exit after acquisition, including storage failure, goes through explicit retirement.
    let work = serve_target(directory, &mut device, commands, cancelled, stopping, disconnected);
    let retirement = device.stop();
    Ok(SessionResult {
        error: work.err().map(|e| e.to_string()),
        retirement: Some(retirement),
    })
}
fn serve_target(
    directory: &Path,
    owned: &mut Owned,
    commands: &mpsc::Receiver<Message>,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
    disconnected: &AtomicBool,
) -> io::Result<()> {
    std::fs::create_dir(directory.join("commands"))?;
    match owned {
        Owned::Page(t) => save_evidence(&directory.join("ready.json"), t.owner())?,
        Owned::Host(t) => save_evidence(&directory.join("host-ready.json"), &frontcap_core::native::ios_host::Ready { owner:t.owner().clone(), contacts:t.contact_endpoint().cloned(), pages:t.pages_available(), app_lifecycle:t.app_lifecycle(), system_ui:t.system_identity().cloned() })?,
    }
    let mut expected = 1u64;
    let mut last_check = Instant::now();
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
        if last_check.elapsed() >= Duration::from_secs(1) {
            // A graceful Stop must not cancel the idle probe and revoke the resource
            // group's authority before explicit retirement. EOF still cancels it;
            // the existing deadline bounds a probe that does not return.
            owned.ensure_process_running(Instant::now() + Duration::from_secs(10), disconnected)?;
            last_check = Instant::now();
            continue;
        }
        let message = match commands.recv_timeout(Duration::from_millis(20)) {
            Ok(message) => message,
            Err(_) => continue,
        };
        let Message::Execute {
            number,
            id,
            operation,
            deadline_ns,
        } = &message
        else {
            return Err(io::Error::other("unexpected WebView session start"));
        };
        if *number != expected {
            return Err(io::Error::other("out-of-order WebView session command"));
        }
        expected = expected
            .checked_add(1)
            .ok_or_else(|| io::Error::other("WebView sequence exhausted"))?;
        let attempt = directory.join("commands").join(number.to_string());
        std::fs::create_dir(&attempt)?;
        save_evidence(&attempt.join("command.json"), &message)?;
        let deadline = instant(*deadline_ns)?;
        let work = match owned {
          Owned::Host(target) => match operation {
            Operation::Host { request } => crate::ios_host_session::execute(target, directory, id, request, deadline, cancelled).map(|completion|Completion::Host { completion }),
            _ => Err(io::Error::other("page operation requires a page session")),
          },
          Owned::Page(target) => match operation {
            Operation::Host { .. } => Err(io::Error::other("native host operation requires a host session")),
            Operation::Restart { generation } => {
                if generation != &target.owner().endpoint.generation {
                    Err(io::Error::other("restart generation differs from current owner"))
                } else {
                    let previous = target.owner().clone();
                    let epochs = directory.join("epochs");
                    std::fs::create_dir_all(&epochs)?;
                    // A failed replacement retires the target and ends this worker. No retry.
                    let owner = target.restart(&epochs.join(id.as_str()), deadline, cancelled)
                        .map_err(|e| io::Error::other(e.cause))?;
                    let page_ready = target.inspect_progress().clock.last_completed
                        .is_some_and(|r| r.reply.status == crate::webview::Status::Ok);
                    save_evidence(&directory.join("ready.json"), &owner)?;
                    Ok(Completion::Restarted { previous, owner, page_ready })
                }
            }

            Operation::InspectProgress => Ok(Completion::Progress { observation: Box::new(target.inspect_progress()) }),
            Operation::Background | Operation::Foreground => {
                let kind = if matches!(operation, Operation::Background) {
                    frontcap_core::native::webview_lifecycle::Operation::Background
                } else { frontcap_core::native::webview_lifecycle::Operation::Foreground };
                if id.as_str().starts_with("mcp-webview-lifecycle-") {
                    Err(io::Error::other("request ID reserved for lifecycle acquisition"))
                } else {
                    target.lifecycle(id.clone(), kind, deadline, cancelled)
                        .map(|reply| Completion::Lifecycle { reply })
                }
            }
            Operation::ReconcileLifecycle { request_id } => target
                .reconcile_lifecycle(request_id, deadline, cancelled)
                .map(|reply| Completion::LifecycleReconciled { reply }),
            Operation::Contacts { observation, input } => target
                .input(id.clone(), observation.clone(), input.clone(), deadline, cancelled)
                .map(|receipt|Completion::Contacts {receipt}),
            Operation::ReconcileContacts { request_id } => target
                .reconcile_input(request_id, deadline, cancelled)
                .map(|receipt|Completion::ContactsReconciled {receipt}),
            Operation::Clock { kind, delta_ns } => target
                .execute(id.clone(), *kind, *delta_ns, deadline, cancelled)
                .map(|reply| Completion::Clock { reply }),
            Operation::Reconcile { request_id } => target
                .reconcile(request_id, deadline, cancelled)
                .map(|reply| Completion::Reconciled { reply }),
            Operation::Capture => {
                target
                    .capture(id.clone(), deadline, cancelled)
                    .and_then(|path| {
                        let meta = std::fs::symlink_metadata(&path)?;
                        if !meta.is_file() || meta.len() > 16 * 1024 * 1024 {
                            return Err(io::Error::other("invalid WebView capture file"));
                        }
                        let data = std::fs::read(&path)?;
                        if data.len() as u64 != meta.len()
                            || !data.starts_with(b"\x89PNG\r\n\x1a\n")
                        {
                            return Err(io::Error::other("invalid WebView PNG"));
                        }
                        Ok(Completion::Captured {
                            input_basis: target.input_basis(),
                            path,
                            bytes: data.len() as u64,
                            sha256: frontcap_core::canonical::sha256_bytes_hex(&data),
                        })
                    })
            }
        },
        };
        // A test-injected publication failure must not become a saved Failed operation: the
        // host/native evidence decides completion or uncertainty after owned retirement.
        let work = match work {
            Err(error) if crate::ios_host_fault::is_injected(&error) => return Err(error),
            other => other,
        };
        let reply = match work {
            Ok(completion) => Reply::Completed {
                completion: Box::new(completion),
            },
            Err(error) => Reply::Failed {
                error: error.to_string(),
            },
        };
        let publication = save_evidence(&attempt.join("result.json"), &reply);
        if let Err(error) = &publication {
            eprintln!("WebView result publication failed: {error}");
        }
        publication?;
        // An operation failure remains inspectable; a reconcile command may resolve its receipt.
        // EOF/Stop still revokes further authority and retires the owned target.
    }
}
#[cfg(test)]
#[path = "webview_session_tests.rs"]
mod tests;
