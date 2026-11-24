//! Owned Android app/WebView worker. Control EOF revokes authority; records never authorize adoption.
use crate::{
    device::{AcquisitionError, AndroidLaunch, ContactDriverLimits, Device, WebViewLifecycle},
    worker::monotonic_ns,
};
pub use frontcap_core::native::android_webview::{Completion, Operation, Ready};
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
pub struct InputProducer {
    pub jar: PathBuf,
    pub sha256: String,
    pub request_capacity: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub device: AndroidLaunch,
    #[serde(default)]
    pub entry_url: String,
    pub lifetime_ms: u64,
    pub input: Option<InputProducer>,
}
impl Config {
    /// Bind retained readiness to frozen launch inputs. This does not establish live ownership.
    pub fn validate_ready(&self, ready: &Ready) -> io::Result<()> {
        self.validate()?;
        let profile = self.device.webview_app.as_ref().ok_or_else(|| io::Error::other("WebView readiness requires a WebView profile"))?;
        let owner = &ready.owner;
        if owner.package != profile.package
            || owner.activity != profile.activity
            || owner.serial != format!("emulator-{}", self.device.port)
            || owner.apk_sha256 != self.device.apk_sha256
            || owner.source_sha256 != self.device.source_sha256
            || owner.pid == 0
            || owner.process_start_ticks == 0
            || ready.page_id.is_empty()
            || !frontcap_core::native::request::canonical_uuid(&ready.document)
            || ready.prepared.url != self.entry_url
            || ready.prepared.random_seed.is_some()
        {
            return Err(io::Error::other(
                "Android WebView readiness differs from frozen launch inputs",
            ));
        }
        let input_matches = match (&self.input, &ready.contacts) {
            (None, None) => true,
            (Some(input), Some(driver)) => {
                driver.consumer == frontcap_core::native::android_webview::ContactConsumer::WebView(owner.clone())
                    && driver.jar_sha256 == input.sha256
                    && driver.request_capacity == input.request_capacity
                    && driver.lifetime_ms == self.lifetime_ms
            }
            _ => false,
        };
        if !input_matches {
            return Err(io::Error::other(
                "Android WebView input producer differs from frozen launch inputs",
            ));
        }
        Ok(())
    }
    fn validate(&self) -> io::Result<()> {
        let profile_matches = match (&self.device.host_app, &self.device.webview_app) {
            (Some(profile), None) => { profile.validate()?; self.device.view_app.is_none() && self.entry_url.is_empty() },
            (None, Some(_)) => (self.entry_url.starts_with("http://") || self.entry_url.starts_with("https://"))
                && !self.entry_url.chars().any(char::is_control),
            _ => false,
        };
        if !profile_matches
            || self.lifetime_ms == 0
            || self.lifetime_ms > 900_000
        {
            return Err(io::Error::other(
                "expected exclusive external app profile, matching page entry and bounded lifetime",
            ));
        }
        if self.input.as_ref().is_some_and(|input| {
            !input.jar.is_absolute()
                || !(1..=4096).contains(&input.request_capacity)
                || input.sha256.len() != 64
                || !input
                    .sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) {
            return Err(io::Error::other("invalid external input producer"));
        }
        Ok(())
    }
}
const MESSAGE_LIMIT: usize = 65536;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    config: Config,
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
    HomeReturnFailed {
        error: String,
        lifecycle: Option<Box<frontcap_core::native::android_progress::Lifecycle>>,
    },
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
    /// Worker is gone; its independently owned resource group is still retiring.
    ResourceCleanup { worker: ProcessIdentity },
    /// Only resource retirement is known. No worker or app-operation result is synthesized.
    WorkerLost { worker: ProcessIdentity, retirement: Retirement },
    /// No terminal proof. Never authorizes signalling a saved PID or replacing an unretired target.
    Unresolved,
}
impl Recovery {
    /// Cleanup only; does not establish any pending application's operation outcome.
    pub fn retirement(self) -> Option<Retirement> {
        match self {
            Self::Finished { result, .. } => result.retirement,
            Self::WorkerLost { retirement, .. } => Some(retirement),
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
        load(&directory.join("worker.json"))?,
        load(&directory.join("result.json"))?,
    ) {
        (Some(worker), Some(result)) => Ok(Recovery::Finished { worker, result }),
        (Some(worker), None) => {
            if !worker.has_exited()? { return Ok(Recovery::Unresolved); }
            let Some(launch) = load::<Launch>(&directory.join("launch.json"))? else { return Ok(Recovery::Unresolved); };
            launch.config.validate()?;
            let root = directory.canonicalize()?;
            let transport = launch.config.device.port.checked_add(1)
                .ok_or_else(|| io::Error::other("invalid original Android transport port"))?;
            match crate::resource_group::inspection::inspect(&root.join("target-resources"), &root.join("target"),
                &[launch.config.device.port, transport], launch.config.device.scratch_volume.as_ref())? {
                crate::resource_group::inspection::Inspection::Active => Ok(Recovery::ResourceCleanup { worker }),
                crate::resource_group::inspection::Inspection::Unresolved => Ok(Recovery::Unresolved),
                crate::resource_group::inspection::Inspection::Finished(group) => {
                    let signal_error = group.resources.values().find_map(|resource| resource.signal_error.clone());
                    Ok(Recovery::WorkerLost { worker, retirement: Retirement {
                        passed: group.passed, errors: group.errors, signal_error,
                    } })
                }
            }
        }
        _ => Ok(Recovery::Unresolved),
    }
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
    pub fn start_with_budget(
        executable: &Path, config: Config, directory: &Path, deadline: Instant,
        budget: Option<&frontcap_core::write_budget::WriteBudget>,
    ) -> io::Result<Self> {
        config.validate()?;
        let launch = Launch {
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
    pub fn wait_ready(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Ready> {
        self.wait_readiness("ready.json", deadline, cancelled)
    }
    pub fn wait_host_ready(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<frontcap_core::native::android_host::Ready> {
        self.wait_readiness("host-ready.json", deadline, cancelled)
    }
    fn wait_readiness<T: serde::de::DeserializeOwned>(&mut self, file: &str, deadline: Instant, cancelled: &AtomicBool) -> io::Result<T> {
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
            if let Some(ready) = load(&self.directory.join(file))? {
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
                id: id.clone(),
                operation: operation.clone(),
                deadline_ns: deadline_ns(deadline)?,
            },
            deadline,
            cancelled,
        )?;
        self.wait_reply(&id, &operation, deadline, cancelled)
    }
    fn wait_reply(&mut self, id: &RequestId, operation: &Operation, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Reply> {
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
                // Exit revokes further input, but cannot erase a verified original effect. This
                // lookup neither adopts descendants nor establishes their retirement.
                self.control.take();
                if let Some(reply) = read_result(&self.directory, id, operation)? {
                    return Ok(reply);
                }
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
pub fn run(directory: &Path) -> io::Result<()> {
    let launch: Launch = load(&directory.join("launch.json"))?
        .ok_or_else(|| io::Error::other("missing launch"))?;
    let executable = std::env::current_exe()?;
    let lifetime = launch.deadline_ns.checked_add(launch.config.lifetime_ms.checked_mul(1_000_000)
        .ok_or_else(|| io::Error::other("resource lifetime overflow"))?)
        .ok_or_else(|| io::Error::other("resource deadline overflow"))?;
    run_with(directory, move |config, target, deadline, cancelled| {
        Device::acquire_supervised(config, target, deadline, cancelled, crate::device::Supervision {
            executable, group_arguments: vec!["--resource-group".into()],
            resource_arguments: vec!["--resource-process".into()], deadline_ns: lifetime,
        })
    })
}
fn save_evidence<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    save_inherited(Class::Ordinary, path, value)
}

fn run_with(
    directory: &Path,
    acquire: impl FnOnce(AndroidLaunch, &Path, Instant, &AtomicBool) -> Result<Device, AcquisitionError>,
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
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let outcome = runtime.block_on(serve(directory, acquire, &commands, &cancelled, &stopping));
    let result = outcome.unwrap_or_else(|error| SessionResult {
        error: Some(error.to_string()),
        retirement: None,
    });
    save_inherited(Class::Retirement, &directory.join("result.json"), &result)
}
async fn serve(
    directory: &Path,
    acquire: impl FnOnce(AndroidLaunch, &Path, Instant, &AtomicBool) -> Result<Device, AcquisitionError>,
    commands: &mpsc::Receiver<Message>,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
) -> io::Result<SessionResult> {
    let launch: Launch =
        load(&directory.join("launch.json"))?.ok_or_else(|| io::Error::other("missing launch"))?;
    launch.config.validate()?;
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
        launch.config.device.clone(),
        &directory.join("target"),
        deadline,
        cancelled,
    ) {
        Ok(device) => device,
        Err(error) => {
            return Ok(SessionResult {
                error: Some(error.cause),
                retirement: error.retirement,
            });
        }
    };
    let work = serve_device(
        directory,
        &launch.config,
        &mut device,
        commands,
        cancelled,
        stopping,
    )
    .await;
    let retirement = device.stop();
    Ok(SessionResult {
        error: work.err().map(|e| e.to_string()),
        retirement: Some(retirement),
    })
}
fn child_id(id: &RequestId, part: &str) -> RequestId {
    let digest = frontcap_core::canonical::sha256_bytes_hex(id.as_str().as_bytes());
    RequestId::new(format!("request-{}-{part}", &digest[..32])).unwrap()
}
/// Frozen-config native-host lookup for recorder recovery; no worker or device is acquired.
pub fn read_host_result(directory: &Path, config: &Config, id: &RequestId,
    operation: &frontcap_core::native::android_host::Operation) -> io::Result<Option<Reply>> {
    let launch: Launch = load(&directory.join("launch.json"))?
        .ok_or_else(||io::Error::other("Android host frozen launch unavailable"))?;
    if serde_json::to_value(launch.config)? != serde_json::to_value(config)? {
        return Err(io::Error::other("Android host launch differs from frozen recorder config"));
    }
    read_result(directory,id,&Operation::Host {request:operation.clone()})
}

/// Offline original result lookup. An unfinished record grants no permission to retry its effect.
pub fn read_result(
    directory: &Path,
    id: &RequestId,
    operation: &Operation,
) -> io::Result<Option<Reply>> {
    let request = directory.join("requests").join(id.as_str());
    let Some(original) = load::<Operation>(&request.join("operation.json"))? else {
        return Ok(None);
    };
    if original != *operation {
        return Err(io::Error::other("Android WebView request ID conflict"));
    }
    if let Operation::Host { request: operation } = operation {
        return host::read_result(directory, id, operation);
    }
    let mut reply = load::<Reply>(&request.join("result.json"))?;
    if reply.is_none()
        && let Operation::Contacts { observation, input } = operation
        && let Some(ready) = load::<Ready>(&directory.join("ready.json"))?
        && let Some(owner) = ready.contacts
    {
        if owner.consumer != frontcap_core::native::android_webview::ContactConsumer::WebView(ready.owner) {
            return Err(io::Error::other("retained contact consumer differs from session owner"));
        }
        reply = Device::read_contact_result(
            &directory.join("target"), &owner, &child_id(id, "input"), observation, input,
        )?.map(|receipt| Reply::Completed { completion: Box::new(Completion::Contacts { receipt }) });
    }
    if matches!(&reply, Some(Reply::HomeReturnFailed { .. })) && !matches!(operation, Operation::HomeReturn) {
        return Err(io::Error::other("Android lifecycle failure differs from original operation"));
    }
    if let Some(Reply::HomeReturnFailed { lifecycle: Some(progress), .. }) = &reply {
        let ready = load::<Ready>(&directory.join("ready.json"))?
            .ok_or_else(|| io::Error::other("lifecycle failure owner unavailable"))?;
        for transition in progress.last_completed.iter().chain(progress.pending.iter()) {
            if transition.owner != ready.owner || transition.receipt.as_ref().is_some_and(|receipt|
                receipt.owner != transition.owner || receipt.operation != transition.operation) {
                return Err(io::Error::other("lifecycle failure evidence differs from session owner or operation"));
            }
        }
    }
    if let Some(Reply::Completed { completion }) = &reply {
        let snapshots: &[&frontcap_core::native::android_webview::Snapshot] = match completion.as_ref() {
            Completion::Read { snapshot } | Completion::Observed { snapshot, .. } => &[snapshot],
            Completion::HomeReturned { before, after, .. } => &[before, after],
            _ => &[],
        };
        for snapshot in snapshots {
            let ready = if snapshot.navigation.is_some() || snapshot.timer_activity.is_some() {
                Some(load::<Ready>(&directory.join("ready.json"))?
                    .ok_or_else(|| io::Error::other("page observation document owner unavailable"))?)
            } else { None };
            snapshot.validate(ready.as_ref().map(|r| r.document.as_str()))?;
        }
        let matches = match (operation, completion.as_ref()) {
            (Operation::Read, Completion::Read { .. })

            | (Operation::InspectFrames, Completion::Frames { .. })
            | (Operation::InspectPointers, Completion::Pointers { .. })
            | (Operation::Contacts { .. } | Operation::ReconcileContacts { .. }, Completion::Contacts { .. })
            | (Operation::HomeReturn, Completion::HomeReturned { .. }) => true,
            (Operation::InspectProgress, Completion::Progress { observation }) => observation.connection_navigation.as_ref().is_none_or(|history|history.valid()),
            (Operation::Observe { input_basis }, Completion::Observed { basis, .. }) => {
                *input_basis == basis.is_some()
            }
            (Operation::Timers { action }, Completion::Timers { completion }) => {
                completion.action == *action
            }
            (Operation::Frame { request }, Completion::Frame { receipt }) => {
                receipt.validate()?;
                receipt.last.as_ref().is_some_and(|last| last.request == *request)
            }
            _ => false,
        };
        if !matches {
            return Err(io::Error::other("Android WebView completion differs from original operation"));
        }
    }
    Ok(reply)
}
async fn serve_device(
    directory: &Path,
    config: &Config,
    device: &mut Device,
    commands: &mpsc::Receiver<Message>,
    cancelled: &AtomicBool,
    stopping: &AtomicBool,
) -> io::Result<()> {
    if config.device.host_app.is_some() {
        return host::serve(directory, config, device, commands, cancelled, stopping).await;
    }
    use crate::{android_frames, android_timers::Action};
    let end = Instant::now() + Duration::from_millis(config.lifetime_ms);
    let input = if let Some(input) = &config.input {
        Some(
            device
                .start_contact_driver_bounded(
                    &input.jar,
                    &input.sha256,
                    ContactDriverLimits {
                        lifetime: Duration::from_millis(config.lifetime_ms),
                        request_capacity: input.request_capacity,
                    },
                    end,
                    cancelled,
                )
                .map_err(io::Error::other)?,
        )
    } else {
        None
    };
    device
        .connect_webview_debugger(end, cancelled)
        .map_err(io::Error::other)?;
    let pages = device.inspect_webview_pages(
        Path::new("/usr/bin/curl"),
        &directory.join("pages"),
        end,
        cancelled,
    )?;
    if pages.len() != 1 {
        return Err(io::Error::other(
            "expected one WebView page; ambiguous selection refused",
        ));
    }
    let owner = device.webview_debugger().unwrap().clone();
    let journal = directory.join("page");
    let mut page = device
        .connect_webview_page(&pages[0].id, &journal, end, cancelled)
        .await?;
    let (prepared, document) = prepare_page(&mut page, directory, &config.entry_url, None, end, cancelled).await?;
    std::fs::create_dir(directory.join("commands"))?;
    std::fs::create_dir(directory.join("requests"))?;
    save_evidence(
        &directory.join("ready.json"),
        &Ready {
            owner,
            page_id: pages[0].id.clone(),
            document: document.clone(),
            prepared,
            contacts: input,
        },
    )?;
    let mut expected = 1;
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
        check(end, cancelled)?;
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
            return Err(io::Error::other("unexpected session start"));
        };
        if *number != expected {
            return Err(io::Error::other("out-of-order Android WebView command"));
        }
        expected += 1;
        let attempt = directory.join("commands").join(number.to_string());
        std::fs::create_dir(&attempt)?;
        save_evidence(&attempt.join("command.json"), &message)?;
        let request = directory.join("requests").join(id.as_str());
        if request.exists() {
            let reply = match read_result(directory, id, operation) {
                Ok(Some(reply)) => reply,
                Ok(None) => Reply::Failed {
                    error: "original request unresolved; never replay".into(),
                },
                Err(error) => Reply::Failed {
                    error: error.to_string(),
                },
            };
            save_evidence(&attempt.join("result.json"), &reply)?;
            continue;
        }
        std::fs::create_dir(&request)?;
        save_evidence(&request.join("operation.json"), operation)?;
        let deadline = instant(*deadline_ns)?.min(end);
        let validation = operation
            .validate()
            .and_then(|()| check(deadline, cancelled));
        let work = if let Err(error) = validation {
            Err(error)
        } else if matches!(operation, Operation::HomeReturn) {
            let before = async {
                let receipt = page
                    .inspect_frames(&child_id(id, "before-producer"), deadline, cancelled)
                    .await?
                    .ok_or_else(|| io::Error::other("missing producer before Home"))?;
                if receipt
                    .last
                    .is_some_and(|r| r.state != android_frames::State::Completed)
                {
                    return Err(io::Error::other("frame effect unresolved before Home"));
                }
                page.control_timers(
                    &child_id(id, "before-pause"),
                    &Action::Pause,
                    deadline,
                    cancelled,
                )
                .await?;
                page.read(&child_id(id, "before-clock"), deadline, cancelled)
                    .await
            }
            .await;
            match before {
                Err(error) => Err(error),
                Ok(before) => {
                    page.close();
                    drop(page);
                    #[cfg(feature = "fault-injection")]
                    let interrupt_after_home = std::env::var("FRONTCAP_ANDROID_WEBVIEW_FAULT").ok().as_deref()
                        == Some(format!("{id}@after_home").as_str());
                    #[cfg(not(feature = "fault-injection"))]
                    let interrupt_after_home = false;
                    let transitions = (|| {
                        let home = device.transition_webview(
                            &child_id(id, "home"),
                            WebViewLifecycle::Home,
                            deadline,
                            cancelled,
                        )?;
                        if interrupt_after_home {
                            return Err(io::Error::other("injected interruption after verified Home"));
                        }
                        let returned = device.transition_webview(
                            &child_id(id, "activate"),
                            WebViewLifecycle::Activate,
                            deadline,
                            cancelled,
                        )?;
                        Ok::<_, io::Error>((home, returned))
                    })();
                    let lifecycle = device.inspect_lifecycle_progress();
                    let resumed = if interrupt_after_home {
                        Err(io::Error::other("injected page reconnect failure after Home"))
                    } else {
                        device.resume_webview_page(&journal, &child_id(id, "return"), deadline, cancelled).await
                    };
                    page = match resumed {
                        Ok(page) => page,
                        Err(error) => {
                            let reply = Reply::HomeReturnFailed {
                                lifecycle: lifecycle.map(Box::new),
                                error: format!(
                                    "page resume failed: {error}; transition: {:?}",
                                    transitions.err()
                                ),
                            };
                            save_evidence(&request.join("result.json"), &reply)?;
                            save_evidence(&attempt.join("result.json"), &reply)?;
                            return Err(error);
                        }
                    };
                    async {
                        let (home, returned) = transitions?;
                        let receipt = page
                            .inspect_frames(&child_id(id, "returned-producer"), deadline, cancelled)
                            .await?
                            .ok_or_else(|| io::Error::other("missing returned producer"))?;
                        if receipt.document != document {
                            return Err(io::Error::other("return replaced page document"));
                        }
                        page.control_timers(
                            &child_id(id, "returned-pause"),
                            &Action::Pause,
                            deadline,
                            cancelled,
                        )
                        .await?;
                        let after = page
                            .read(&child_id(id, "after-clock"), deadline, cancelled)
                            .await?;
                        Ok(Completion::HomeReturned {
                            home: Box::new(home),
                            returned: Box::new(returned),
                            before,
                            after,
                            receipt,
                        })
                    }
                    .await
                }
            }
        } else {
            execute(&mut page, id, operation, deadline, cancelled).await
        };
        let reply = match work {
            Ok(completion) => Reply::Completed {
                completion: Box::new(completion),
            },
            Err(error) if matches!(operation, Operation::HomeReturn) => Reply::HomeReturnFailed {
                error: error.to_string(), lifecycle: page.inspect_lifecycle_progress().map(Box::new),
            },
            Err(error) => Reply::Failed {
                error: error.to_string(),
            },
        };
        // Retain the caller-ID result before publishing transport acknowledgement. Storage failure
        // ends this owner instead of permitting another mutation with uncertain journal state.
        #[cfg(feature = "fault-injection")]
        if std::env::var("FRONTCAP_ANDROID_WEBVIEW_FAULT").ok().as_deref()
            == Some(format!("{id}@before_result").as_str())
        {
            return Err(io::Error::other("injected Android WebView result publication failure"));
        }
        save_evidence(&request.join("result.json"), &reply)?;
        save_evidence(&attempt.join("result.json"), &reply)?;
    }
}
async fn prepare_page(page: &mut crate::android_webview_page::Connection<'_>, directory: &Path,
    url: &str, random_seed: Option<u32>, end: Instant, cancelled: &AtomicBool) -> io::Result<(frontcap_core::native::android_webview::Prepared, String)> {
    let mut poll = 0;
    loop {
        let snapshot = page
            .read(
                &RequestId::new(format!("initial-host-{poll}")).unwrap(),
                end,
                cancelled,
            )
            .await?;
        save_evidence(&directory.join("preparation-progress.json"), &serde_json::json!({
            "phase": "initial_navigation", "poll": poll,
            "requested_url": url, "snapshot": snapshot,
        }))?;
        if snapshot.initial_navigation_finished() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        poll += 1;
    }
    let prepared = page
        .prepare_frames(
            &RequestId::new("prepare").unwrap(),
            url,
            random_seed,
            end,
            cancelled,
        )
        .await?;
    let mut poll = 0;
    let document = loop {
        let snapshot = page
            .read(
                &RequestId::new(format!("ready-{poll}")).unwrap(),
                end,
                cancelled,
            )
            .await?;
        let receipt = page
            .inspect_frames(
                &RequestId::new(format!("producer-{poll}")).unwrap(),
                end,
                cancelled,
            )
            .await?;
        // Retain the last observed state even when the caller's preparation deadline cancels us.
        // This is diagnostic evidence only, never a readiness record or authority to navigate again.
        save_evidence(&directory.join("preparation-progress.json"), &serde_json::json!({
            "phase": "entry_navigation", "poll": poll,
            "requested_url": url, "snapshot": snapshot, "producer": receipt,
        }))?;
        // Async scripts may deliberately remain held by the owned resource service. A parsed
        // document with the installed frame producer is attachable, not application-ready.
        if snapshot.entry_document_parsed(url)
            && let Some(receipt) = receipt
        {
            if let Some(seed) = random_seed
                && snapshot.starting_randomness.as_ref().is_none_or(|r| r.seed != seed || r.document != receipt.document) {
                return Err(io::Error::other("entry document lacks the declared starting randomness"));
            }
            break receipt.document;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        poll += 1;
    };
    page.control_timers(
        &RequestId::new("initial-pause").unwrap(),
        &crate::android_timers::Action::Pause,
        end,
        cancelled,
    )
    .await?;
    Ok((prepared, document))
}
async fn execute(
    page: &mut crate::android_webview_page::Connection<'_>,
    id: &RequestId,
    operation: &Operation,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> io::Result<Completion> {
    match operation {
        Operation::InspectProgress => Ok(Completion::Progress { observation: Box::new(page.inspect_progress()?) }),
        Operation::Read => Ok(Completion::Read {
            snapshot: page
                .read(&child_id(id, "read"), deadline, cancelled)
                .await?,
        }),
        Operation::Observe { input_basis } => {
            let basis = if *input_basis {
                Some(page.capture_contacts(&child_id(id, "basis"), deadline, cancelled)?)
            } else {
                None
            };
            let screen = if let Some(basis) = &basis {
                basis.path.clone()
            } else {
                page.capture(&child_id(id, "screen"), deadline, cancelled)?
            };
            let snapshot = page
                .read(&child_id(id, "read"), deadline, cancelled)
                .await?;
            let pointers = page
                .inspect_pointers(&child_id(id, "pointers"), deadline, cancelled)
                .await?;
            let bytes = std::fs::read(&screen)?;
            Ok(Completion::Observed {
                sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes),
                bytes: bytes.len() as u64,
                snapshot,
                screen,
                pointers,
                basis,
            })
        }
        Operation::InspectFrames => Ok(Completion::Frames {
            receipt: page
                .inspect_frames(&child_id(id, "frames"), deadline, cancelled)
                .await?,
        }),
        Operation::InspectPointers => Ok(Completion::Pointers {
            snapshot: page
                .inspect_pointers(&child_id(id, "pointers"), deadline, cancelled)
                .await?,
        }),
        Operation::Frame { request } => Ok(Completion::Frame {
            receipt: page
                .dispatch_frame(&child_id(id, "frame"), request, deadline, cancelled)
                .await?,
        }),
        Operation::Timers { action } => Ok(Completion::Timers {
            completion: page
                .control_timers(&child_id(id, "timer"), action, deadline, cancelled)
                .await?,
        }),
        Operation::Contacts { observation, input } => Ok(Completion::Contacts {
            receipt: page.input_contacts(
                &child_id(id, "input"),
                observation,
                input,
                deadline,
                cancelled,
            )?,
        }),
        Operation::ReconcileContacts { request_id } => Ok(Completion::Contacts {
            receipt: page.reconcile_contacts(
                &child_id(request_id, "input"),
                deadline,
                cancelled,
            )?,
        }),
        Operation::HomeReturn => unreachable!(),
        Operation::Host { .. } => Err(io::Error::other("native host operation requires a host session")),
    }
}

#[cfg(test)]
#[path = "android_webview_session_tests.rs"]
mod tests;

#[path = "android_host_session.rs"]
mod host;
