//! Long-lived target owner. The private controller pipe is an authority lease, not a reconnect API.
//! Only this process owns/reaps the emulator and bridges; terminal files support read-only recovery.
use crate::{
    device::{AcquisitionError, AndroidLaunch, Device, Retirement},
    request::{Completion, Operation},
    store::{Recovery as RequestRecovery, RequestStore},
    worker::monotonic_ns,
};
use frontcap_core::{
    ids::RequestId,
    process::ProcessIdentity,
    state::{load, save_atomic},
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
mod views;
pub use views::{ViewReply, read_view_observation, read_view_result, view_observation_directory};

const MESSAGE_LIMIT: usize = 262144;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    config: AndroidLaunch,
    deadline_ns: u64,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
enum Message {
    Start,
    ExecuteViewOperation { number: u64, id: RequestId, operation: Operation, deadline_ns: u64 },
    DispatchViewUrl { number: u64, id: RequestId, request: frontcap_core::native::external_entry::Request, deadline_ns: u64 },
    RestartViews { number: u64, id: RequestId, generation: String,
        #[serde(default, skip_serializing_if = "Option::is_none")] url: Option<String>, deadline_ns: u64 },
    SetViewState { number: u64, id: RequestId, generation: String,
        state: frontcap_core::native::app_state::State, deadline_ns: u64 },
    ObserveViews {
        number: u64,
        id: RequestId,
        deadline_ns: u64,
    },
    Stop,
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
    /// No terminal proof. Never authorizes signalling a saved PID or replacing an unretired target.
    Unresolved,
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
    view: bool,
}
impl Session {
    pub fn start(
        executable: &Path,
        config: AndroidLaunch,
        directory: &Path,
        deadline: Instant,
    ) -> io::Result<Self> {
        Self::start_with_budget(executable, config, directory, deadline, None)
    }
    pub fn start_with_budget(
        executable: &Path, config: AndroidLaunch, directory: &Path, deadline: Instant,
        budget: Option<&frontcap_core::write_budget::WriteBudget>,
    ) -> io::Result<Self> {
        let view = config.view_app.is_some();
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
            view,
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
    pub fn wait_view_ready(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<crate::android_app::Ready> {
        if !self.view { return Err(io::Error::other("expected a View session")); }
        self.wait_ready_value("view-ready.json", deadline, cancelled)
    }
    fn wait_ready_value<T: serde::de::DeserializeOwned>(&mut self, name: &str, deadline: Instant, cancelled: &AtomicBool) -> io::Result<T> {
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
    pub fn validate_view_operation(&self, id: &RequestId, operation: &Operation) -> io::Result<()> {
        if !self.view { return Err(io::Error::other("expected a View session")); }
        if let Some((owner, directory)) = crate::android_restarts::request_epoch(&self.directory,id)? {
            return RequestStore::read_completion(&directory.join("requests"), &owner.identity(), &owner.generation,
                &owner.source_sha256, id, operation).map_err(io::Error::other)?
                .map(|_| ()).ok_or_else(|| io::Error::other("original View operation unresolved"));
        }
        self.validate_view_foreground()?;
        if crate::android_app_state::has_request(&self.directory, id)? {
            return Err(io::Error::other("View operation ID names a host transition"));
        }
        let (_, directory) = crate::android_restarts::current(&self.directory)?;
        views::validate_operation(&directory,id,operation)
    }
    pub fn execute_view_operation(&mut self, id: RequestId, operation: Operation, deadline: Instant, cancelled: &AtomicBool) -> io::Result<ViewReply> {
        if !self.view || !matches!(operation,Operation::ReadClock {} | Operation::Advance { .. } | Operation::ReplaceViewText { .. } | Operation::TapView { .. } | Operation::ScrollView { .. }) {
            return Err(io::Error::other("expected a View operation"));
        }
        self.number=self.number.checked_add(1).ok_or_else(||io::Error::other("session sequence exhausted"))?;
        self.send(&Message::ExecuteViewOperation {number:self.number,id,operation,deadline_ns:deadline_ns(deadline)?},deadline,cancelled)?;
        self.wait_reply(deadline,cancelled)
    }
    pub fn observe_views(&mut self, id: RequestId, deadline: Instant, cancelled: &AtomicBool) -> io::Result<ViewReply> {
        if !self.view { return Err(io::Error::other("expected a View session")); }
        self.validate_view_foreground()?;
        self.number = self.number.checked_add(1).ok_or_else(|| io::Error::other("session sequence exhausted"))?;
        self.send(&Message::ObserveViews { number: self.number, id, deadline_ns: deadline_ns(deadline)? }, deadline, cancelled)?;
        self.wait_reply(deadline, cancelled)
    }
    pub fn dispatch_view_url(&mut self, id: RequestId, request: frontcap_core::native::external_entry::Request,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<crate::external_entry::Reply> {
        if !self.view { return Err(io::Error::other("expected a View session")); }
        let (_, directory) = crate::android_restarts::epochs(&self.directory)?.into_iter().find(|(o,_)| o.generation == request.generation)
            .ok_or_else(|| io::Error::other("URL generation absent from Android lineage"))?;
        views::validate_entry(&directory, &request)?;
        self.number = self.number.checked_add(1).ok_or_else(|| io::Error::other("session sequence exhausted"))?;
        self.send(&Message::DispatchViewUrl { number: self.number, id, request, deadline_ns: deadline_ns(deadline)? }, deadline, cancelled)?;
        self.wait_reply(deadline, cancelled)
    }
    pub fn view_restart_available(&self) -> bool {
        self.view && self.directory.join("restarts/history.json").is_file()
    }
    pub fn view_lifecycle_available(&self) -> io::Result<bool> {
        Ok(self.view && crate::android_app_state::available(&self.directory)?)
    }
    pub fn validate_view_foreground(&self) -> io::Result<()> {
        if !self.view { return Err(io::Error::other("expected a View session")); }
        crate::android_app_state::require_foreground(&self.directory)
    }
    pub fn validate_view_state(&self, id: &RequestId, generation: &str, state: frontcap_core::native::app_state::State) -> io::Result<()> {
        if !self.view { return Err(io::Error::other("expected a View session")); }
        crate::android_app_state::validate(&self.directory, id, generation, state)
    }
    pub fn set_view_state(&mut self, id: RequestId, generation: String, state: frontcap_core::native::app_state::State,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<crate::android_app_state::Reply> {
        self.validate_view_state(&id, &generation, state)?;
        self.number = self.number.checked_add(1).ok_or_else(|| io::Error::other("session sequence exhausted"))?;
        self.send(&Message::SetViewState { number: self.number, id, generation, state, deadline_ns: deadline_ns(deadline)? }, deadline, cancelled)?;
        self.wait_reply(deadline, cancelled)
    }
    pub fn validate_view_restart(&self, id: &RequestId, generation: &str, url: Option<&str>) -> io::Result<()> {
        if !self.view {return Err(io::Error::other("restart requires Android Views"));}
        crate::android_restarts::validate_restart(&self.directory, id, generation, url)
    }
    pub fn restart_views(&mut self, id: RequestId, generation: String, url: Option<String>, deadline: Instant, cancelled: &AtomicBool) -> io::Result<crate::android_restarts::Reply> {
        self.validate_view_restart(&id, &generation, url.as_deref())?;
        self.number = self.number.checked_add(1).ok_or_else(|| io::Error::other("session sequence exhausted"))?;
        self.send(&Message::RestartViews {number: self.number, id, generation, url, deadline_ns: deadline_ns(deadline)?}, deadline, cancelled)?;
        self.wait_reply(deadline, cancelled)
    }
    fn wait_reply<T: serde::de::DeserializeOwned>(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<T> {
        let reply = self
            .directory
            .join("commands")
            .join(self.number.to_string())
            .join("result.json");
        // Operation time is unchanged; allow separate diagnostics (10 s) and owned retirement.
        let mut end = deadline + Duration::from_secs(15);
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
            if self.child.as_mut().unwrap().try_wait()?.is_some() {
                self.child.take();
                return inspect(&self.directory);
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
    run_with(directory, Device::acquire)
}
fn run_with(
    directory: &Path,
    acquire: impl FnOnce(AndroidLaunch, &Path, Instant, &AtomicBool) -> Result<Device, AcquisitionError>,
) -> io::Result<()> {
    let lock = File::options()
        .read(true)
        .write(true)
        .open(directory.join("session.lock"))?;
    lock.lock()?;
    if directory.join("worker.json").exists() || directory.join("result.json").exists() {
        return Err(io::Error::other(
            "native session already started; inspect existing evidence",
        ));
    }
    save_atomic(
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
    });
    save_atomic(&directory.join("result.json"), &result)
}
fn serve(
    directory: &Path,
    acquire: impl FnOnce(AndroidLaunch, &Path, Instant, &AtomicBool) -> Result<Device, AcquisitionError>,
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
        return Err(io::Error::other(
            "session not authorized before acquisition deadline",
        ));
    }
    check(deadline, cancelled)?;
    let mut device = match acquire(
        launch.config,
        &directory.join("device"),
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
    // Every exit after acquisition, including storage failure, goes through explicit retirement.
    let work = views::serve(directory, &mut device, commands, cancelled, stopping);
    let retirement = device.stop();
    Ok(SessionResult {
        error: work.err().map(|e| e.to_string()),
        retirement: Some(retirement),
    })
}


#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
