//! Synchronous ownership of one bridge process group. The caller owns the native target separately.
//! Spawn and filesystem calls remain subject to host OS scheduling; exchange and retirement have
//! separate deadlines. No other thread or SIGCHLD handler may reap this runner's child.
use crate::bridge::{BridgeCall, BridgeReply};
#[path = "process_artifact.rs"]
mod artifact;
pub(crate) use artifact::{ArtifactExchange, capture_artifact_recorded};
#[path = "process_evidence.rs"]
mod evidence;
pub use evidence::load_exchange;
pub(crate) use evidence::{save_exchange_with_budget, validate_exchange_files};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

thread_local! {
    static WRITE_CLASS: std::cell::Cell<frontcap_core::write_budget::Class> = const {
        std::cell::Cell::new(frontcap_core::write_budget::Class::Ordinary)
    };
}
fn write_class() -> frontcap_core::write_budget::Class { WRITE_CLASS.get() }

/// Synchronous cleanup scope. Never transfers cleanup authority to another thread or process.
pub(crate) struct RetirementWrites {
    previous: frontcap_core::write_budget::Class,
    same_thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl RetirementWrites {
    pub(crate) fn enter() -> Self {
        Self { previous: WRITE_CLASS.replace(frontcap_core::write_budget::Class::Retirement),
            same_thread: std::marker::PhantomData }
    }
}
impl Drop for RetirementWrites {
    fn drop(&mut self) { WRITE_CLASS.set(self.previous); }
}
fn save_budgeted<T: Serialize>(budget: Option<&frontcap_core::write_budget::WriteBudget>, path: &std::path::Path, value: &T) -> io::Result<()> {
    match budget {
        Some(budget) => budget.save_atomic(write_class(), path, value),
        None => frontcap_core::state::save_atomic(path, value),
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exchange<R = BridgeReply, S = Vec<u8>> {
    pub pid: Option<u32>,
    pub stdout: S,
    pub stderr: S,
    pub reply: Option<R>,
    pub failure: Option<String>,
    /// Kept separate so a retirement failure never replaces the operation's original failure.
    pub cleanup_failure: Option<String>,
    pub exit_code: Option<i32>,
    pub signal_error: Option<String>,
    pub evidence_failure: Option<String>,
}

/// Reserve a fresh evidence directory before execution and atomically save the complete exchange.
/// A publication failure is returned alongside the original outcome, never as a replacement for it.
/// Filesystem durability is not bounded by the operation deadline. This record is not a restart-safe
/// process ownership registry: recorder-death reconciliation still requires session ownership.
pub fn exchange_recorded(
    command: &mut Command,
    call: &BridgeCall,
    deadline: Instant,
    cancelled: &AtomicBool,
    output_limit: usize,
    evidence: &std::path::Path,
) -> io::Result<Exchange> {
    exchange_typed_recorded(command, call, deadline, cancelled, output_limit, evidence)
}

/// Reuse bounded process ownership for a target-specific typed bridge protocol.
pub fn exchange_typed_recorded<C: Serialize, R: serde::de::DeserializeOwned + Serialize>(
    command: &mut Command,
    call: &C,
    deadline: Instant,
    cancelled: &AtomicBool,
    output_limit: usize,
    evidence: &std::path::Path,
) -> io::Result<Exchange<R>> {
    let budget = frontcap_core::write_budget::WriteBudget::from_environment()?;
    exchange_typed_recorded_with_budget(command, call, deadline, cancelled, output_limit, evidence, budget.as_ref())
}

pub(crate) fn exchange_typed_recorded_with_budget<C: Serialize, R: serde::de::DeserializeOwned + Serialize>(
    command: &mut Command, call: &C, deadline: Instant, cancelled: &AtomicBool,
    output_limit: usize, evidence: &std::path::Path, budget: Option<&frontcap_core::write_budget::WriteBudget>,
) -> io::Result<Exchange<R>> {
    std::fs::create_dir(evidence)?;
    save_budgeted(budget, &evidence.join("call.json"), call)?;
    let request = serde_json::to_vec(call)?;
    let raw = capture_started(
        command,
        &request,
        deadline,
        cancelled,
        output_limit,
        |pid| {
            let identity = frontcap_core::process::ProcessIdentity::of(pid)?
                .ok_or_else(|| io::Error::other("bridge vanished before ownership publication"))?;
            save_budgeted(budget, &evidence.join("bridge-process.json"), &identity)
        },
    );
    let mut result = Exchange {
        pid: raw.pid,
        stdout: raw.stdout,
        stderr: raw.stderr,
        reply: None,
        failure: raw.failure,
        cleanup_failure: raw.cleanup_failure,
        exit_code: raw.exit_code,
        signal_error: raw.signal_error,
        evidence_failure: raw.evidence_failure,
    };
    if result.failure.is_none() && result.cleanup_failure.is_none() {
        match serde_json::from_slice(&result.stdout) {
            Ok(reply) => result.reply = Some(reply),
            Err(error) => result.failure = Some(format!("invalid bridge reply: {error}")),
        }
    }
    if let Err(error) = save_exchange_with_budget(&evidence.join("exchange.json"), &result, budget)
    {
        result.evidence_failure = Some(error.to_string());
        result.reply = None;
    }
    Ok(result)
}

/// The supplied command is a trusted local bridge configuration, never a remote command endpoint.
/// Always retain this result as evidence, including failed/partial exchanges. A reply alone must
/// not commit a request: the caller still validates it against the durable request and receipts.
pub fn exchange(
    command: &mut Command,
    call: &BridgeCall,
    deadline: Instant,
    cancelled: &AtomicBool,
    output_limit: usize,
) -> Exchange {
    exchange_started(command, call, deadline, cancelled, output_limit, |_| Ok(()))
}

fn exchange_started(
    command: &mut Command,
    call: &BridgeCall,
    deadline: Instant,
    cancelled: &AtomicBool,
    output_limit: usize,
    started: impl FnOnce(u32) -> io::Result<()>,
) -> Exchange {
    let request = match serde_json::to_vec(call) {
        Ok(request) => request,
        Err(error) => {
            let mut result = empty_exchange();
            result.failure = Some(error.to_string());
            return result;
        }
    };
    let mut result = capture_started(
        command,
        &request,
        deadline,
        cancelled,
        output_limit,
        started,
    );
    if result.failure.is_none() && result.cleanup_failure.is_none() {
        match serde_json::from_slice(&result.stdout) {
            Ok(reply) => result.reply = Some(reply),
            Err(error) => result.failure = Some(format!("invalid bridge reply: {error}")),
        }
    }
    result
}
fn empty_exchange() -> Exchange {
    Exchange {
        pid: None,
        stdout: Vec::new(),
        stderr: Vec::new(),
        reply: None,
        failure: None,
        cleanup_failure: None,
        exit_code: None,
        signal_error: None,
        evidence_failure: None,
    }
}

/// Capture a trusted target-tool command using the same bounded I/O and process ownership.
/// A raw command has no bridge reply, regardless of its stdout contents.
pub(crate) fn capture_recorded(
    command: &mut Command,
    deadline: Instant,
    cancelled: &AtomicBool,
    output_limit: usize,
    evidence: &std::path::Path,
) -> io::Result<Exchange> {
    let budget = frontcap_core::write_budget::WriteBudget::from_environment()?;
    std::fs::create_dir(evidence)?;
    let invocation = (
        command.get_program().to_string_lossy().into_owned(),
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    );
    save_budgeted(budget.as_ref(), &evidence.join("command.json"), &invocation)?;
    let mut result = capture_started(command, &[], deadline, cancelled, output_limit, |pid| {
        let identity = frontcap_core::process::ProcessIdentity::of(pid)?
            .ok_or_else(|| io::Error::other("command vanished before ownership publication"))?;
        save_budgeted(budget.as_ref(), &evidence.join("process.json"), &identity)
    });
    if let Err(error) = save_exchange_with_budget(&evidence.join("exchange.json"), &result, budget.as_ref())
    {
        result.evidence_failure = Some(error.to_string());
    }
    Ok(result)
}
fn capture_started(
    command: &mut Command,
    request: &[u8],
    deadline: Instant,
    cancelled: &AtomicBool,
    output_limit: usize,
    started: impl FnOnce(u32) -> io::Result<()>,
) -> Exchange {
    capture_started_limits(command, request, deadline, cancelled,
        CaptureLimits { stdout: output_limit, stderr: output_limit }, started)
}

#[derive(Clone, Copy)]
pub(crate) struct CaptureLimits {
    pub stdout: usize,
    pub stderr: usize,
}

fn capture_started_limits(
    command: &mut Command, request: &[u8], deadline: Instant, cancelled: &AtomicBool,
    limits: CaptureLimits, started: impl FnOnce(u32) -> io::Result<()>,
) -> Exchange {
    let mut result = empty_exchange();
    if let Some(reason) = interrupted(deadline, cancelled) {
        result.failure = Some(reason.into());
        return result;
    }
    let child = match command
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            result.failure = Some(format!("spawn: {error}"));
            return result;
        }
    };
    result.pid = Some(child.id());
    let mut owned = OwnedChild::new(child);
    let mut input = owned.child.stdin.take();
    let mut output = owned.child.stdout.take().unwrap();
    let mut errors = owned.child.stderr.take().unwrap();
    let run = (|| -> Result<(), String> {
        nonblocking(input.as_ref().unwrap()).map_err(|e| e.to_string())?;
        nonblocking(&output).map_err(|e| e.to_string())?;
        nonblocking(&errors).map_err(|e| e.to_string())?;
        started(owned.child.id()).map_err(|e| format!("bridge ownership publication: {e}"))?;
        let mut sent = 0;
        if request.is_empty() {
            input.take();
        }
        loop {
            if let Some(reason) = interrupted(deadline, cancelled) {
                return Err(reason.into());
            }
            if let Some(stdin) = input.as_mut() {
                match stdin.write(&request[sent..]) {
                    Ok(0) => return Err("bridge stdin closed before request delivery".into()),
                    Ok(n) => sent += n,
                    Err(e) if retryable(&e) => {}
                    Err(e) => return Err(format!("request write: {e}")),
                }
                if sent == request.len() {
                    input.take();
                }
            }
            drain(&mut output, &mut result.stdout, limits.stdout)?;
            drain(&mut errors, &mut result.stderr, limits.stderr)?;
            if owned.exited().map_err(|e| e.to_string())? {
                if let Some(reason) = interrupted(deadline, cancelled) {
                    return Err(reason.into());
                }
                if sent != request.len() {
                    return Err("bridge exited before request delivery".into());
                }
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    })();
    result.failure = run.err();
    input.take();
    // The leader stays waitable until all signals have been sent. WNOWAIT prevents PID reuse
    // between observing exit and retiring remaining members of its dedicated process group.
    result.cleanup_failure = owned
        .retire(&mut result.exit_code, &mut result.signal_error)
        .err()
        .map(|e| e.to_string());
    // Both pipes are nonblocking; retain buffered bytes even on cancellation/timeout.
    // If setup failed, do not risk a blocking read.
    if nonblocking(&output).is_ok() && nonblocking(&errors).is_ok() {
        for read in [
            drain_remaining(&mut output, &mut result.stdout, limits.stdout),
            drain_remaining(&mut errors, &mut result.stderr, limits.stderr),
        ] {
            if result.failure.is_none() {
                result.failure = read.err();
            }
        }
    }
    if result.failure.is_none() && result.exit_code.is_some_and(|code| code != 0) {
        result.failure = Some("bridge exited unsuccessfully".into());
    }
    if result.failure.is_none() && result.cleanup_failure.is_none() && result.exit_code.is_none() {
        result.failure = Some("bridge terminated by signal".into());
    }
    result
}

fn interrupted(deadline: Instant, cancelled: &AtomicBool) -> Option<&'static str> {
    if cancelled.load(Ordering::Acquire) {
        Some("bridge cancelled")
    } else if Instant::now() >= deadline {
        Some("bridge deadline exceeded")
    } else {
        None
    }
}
fn retryable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
fn nonblocking(file: &impl AsRawFd) -> io::Result<()> {
    // SAFETY: file owns a valid descriptor throughout both fcntl calls.
    unsafe {
        let flags = libc::fcntl(file.as_raw_fd(), libc::F_GETFL);
        if flags < 0 || libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
fn drain(file: &mut impl Read, bytes: &mut Vec<u8>, limit: usize) -> Result<(), String> {
    // Bound work per iteration too: a continuous producer must not starve cancellation.
    let mut buffer = [0; 8192];
    for _ in 0..8 {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                let retained = n.min(limit.saturating_sub(bytes.len()));
                bytes.extend_from_slice(&buffer[..retained]);
                if retained != n {
                    return Err("bridge output limit exceeded".into());
                }
            }
            Err(e) if retryable(&e) => return Ok(()),
            Err(e) => return Err(format!("bridge output read: {e}")),
        }
    }
    Ok(())
}
fn drain_remaining(file: &mut impl Read, bytes: &mut Vec<u8>, limit: usize) -> Result<(), String> {
    loop {
        let before = bytes.len();
        drain(file, bytes, limit)?;
        if bytes.len() == before {
            return Ok(());
        }
    }
}
pub(crate) struct OwnedChild {
    child: Child,
    reaped: bool,
    ownership_lost: bool,
    cleanup_attempted: bool,
}
impl OwnedChild {
    pub(crate) fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
            ownership_lost: false,
            cleanup_attempted: false,
        }
    }
    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }
    pub(crate) fn exited(&mut self) -> io::Result<bool> {
        // SAFETY: zeroed siginfo is valid output storage. WNOWAIT keeps our child waitable.
        unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            let status = libc::waitid(
                libc::P_PID,
                self.child.id(),
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            );
            if status < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ECHILD) {
                    self.ownership_lost = true;
                }
                return Err(error);
            }
            // Darwin can report CLD_STOPPED even when only WEXITED was requested.
            // A stopped leader remains live and must not trigger completed-command cleanup.
            Ok(info.si_pid() != 0
                && matches!(info.si_code, libc::CLD_EXITED | libc::CLD_KILLED | libc::CLD_DUMPED))
        }
    }
    fn signal(&mut self, signal: i32) -> io::Result<()> {
        // Confirm wait ownership before every signal, including a still-running leader.
        self.exited()?;
        // SAFETY: a direct child is reserved, unreaped, and was spawned in its own group.
        if unsafe { libc::kill(-(self.child.id() as i32), signal) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        Ok(())
    }
    pub(crate) fn retire(
        &mut self,
        exit_code: &mut Option<i32>,
        signal_error: &mut Option<String>,
    ) -> io::Result<()> {
        self.cleanup_attempted = true;
        if !self.exited()? {
            if let Err(error) = self.signal(libc::SIGTERM) {
                *signal_error = Some(error.to_string());
                if self.ownership_lost {
                    return Err(error);
                }
            }
            let grace = Instant::now() + Duration::from_millis(200);
            while !self.exited()? && Instant::now() < grace {
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        if let Err(error) = self.signal(libc::SIGKILL) {
            *signal_error = Some(error.to_string());
            // Darwin can return EPERM for a group containing only a zombie. Do not interpret
            // that as absence: reap only an exited owned leader, then independently check group
            // disappearance. Exit may become waitable slightly after the signal failure, so
            // retain the error and use the existing retirement deadline. A live/inaccessible
            // group still fails retirement below.
            if self.ownership_lost {
                return Err(error);
            }
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while !self.exited()? {
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "bridge did not become waitable after SIGKILL",
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        *exit_code = self.child.wait()?.code();
        self.reaped = true;
        loop {
            // Observation only after reaping; a reused group must never receive another signal.
            if unsafe { libc::kill(-(self.child.id() as i32), 0) } < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {
                    return Ok(());
                }
                if error.raw_os_error() != Some(libc::EPERM) {
                    return Err(error);
                }
                // Permission failure still means absence is unproven. Zombie-only groups can
                // disappear when their adopter reaps them; observe until the same cleanup limit.
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "bridge process group still present after retirement",
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.reaped && !self.ownership_lost && !self.cleanup_attempted {
            // Also protects unwinding. Never use Child::kill after losing wait ownership.
            let _ = self.retire(&mut None, &mut None);
        }
    }
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
