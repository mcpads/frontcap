//! A live resource owner independent of its controller. Disk records grant no signalling authority.
use crate::{process::OwnedChild, worker::monotonic_ns};
use frontcap_core::{process::ProcessIdentity, state::{load, save_atomic}};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs::File, io::{self, Read, Write},
    os::{fd::AsRawFd, unix::process::CommandExt}, path::{Path, PathBuf}, process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub remove_environment: Vec<String>,
    pub directory: PathBuf,
    /// Absolute host monotonic deadline, including time waiting for authorization/readiness.
    pub deadline_ns: u64,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputSummary {
    pub retained_bytes: usize,
    pub total_bytes: u64,
    pub eof: bool,
    pub failure: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultRecord {
    pub process: Option<ProcessIdentity>,
    pub reason: String,
    pub retirement_error: Option<String>,
    pub exit_code: Option<i32>,
    pub signal_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdout: Option<OutputSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr: Option<OutputSummary>,
}

/// The caller owns the supervisor, not the resource. Dropping authority never signals saved PIDs.
pub struct ResourceProcess {
    supervisor: Option<Child>,
    authority: Option<ChildStdin>,
    directory: PathBuf,
    pub process: ProcessIdentity,
}
impl ResourceProcess {
    /// `supervisor` is a trusted executable configured to call `run` on the appended directory.
    pub fn start(supervisor: &mut Command, launch: &Launch, directory: &Path, deadline: Instant) -> io::Result<Self> {
        if !launch.program.is_absolute() || !launch.directory.is_absolute() || launch.deadline_ns <= monotonic_ns()? {
            return Err(io::Error::other("invalid resource process launch"));
        }
        std::fs::create_dir(directory)?;
        let directory = directory.canonicalize()?;
        File::create_new(directory.join("owner.lock"))?;
        save_atomic(&directory.join("launch.json"), launch)?;
        let log = crate::diagnostic_log::create_output(&directory.join("supervisor.log"))?;
        let mut child = supervisor.arg(&directory).process_group(0).stdin(Stdio::piped())
            .stdout(Stdio::null()).stderr(log).spawn()?;
        let mut authority = child.stdin.take().unwrap();
        // One bounded authorization byte; EOF thereafter revokes the capability.
        let authorized = authority.write_all(b"1");
        let ready = (|| {
            authorized?;
            loop {
                if let Some(result) = load::<ResultRecord>(&directory.join("result.json"))? {
                    return Err(io::Error::other(format!("resource ended before readiness: {result:?}")));
                }
                if child.try_wait()?.is_some() {
                    return Err(io::Error::other("resource supervisor exited before readiness"));
                }
                if let Some(process) = load(&directory.join("ready.json"))? { return Ok(process); }
                if Instant::now() >= deadline { return Err(io::Error::other("resource readiness observation expired")); }
                std::thread::sleep(Duration::from_millis(5));
            }
        })();
        match ready {
            Ok(process) => Ok(Self { supervisor: Some(child), authority: Some(authority), directory, process }),
            Err(error) => {
                drop(authority);
                std::thread::spawn(move || { let _ = child.wait(); });
                Err(error)
            }
        }
    }
    pub fn revoke(&mut self) { self.authority.take(); }
    pub fn stop(mut self, deadline: Instant) -> io::Result<ResultRecord> {
        self.revoke();
        loop {
            if self.supervisor.as_mut().unwrap().try_wait()?.is_some() {
                self.supervisor.take();
                return load(&self.directory.join("result.json"))?
                    .ok_or_else(|| io::Error::other("resource retirement unresolved"));
            }
            if Instant::now() >= deadline { return Err(io::Error::other("resource retirement observation expired")); }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for ResourceProcess {
    fn drop(&mut self) {
        self.authority.take();
        if let Some(mut child) = self.supervisor.take() {
            std::thread::spawn(move || { let _ = child.wait(); });
        }
    }
}

/// Private executable entry. Resource creation and all signals/reaping stay in this process.
/// Both output streams are drained independently with bounded retained prefixes and explicit loss.
pub fn run(directory: &Path) -> io::Result<()> {
    let lock = File::options().read(true).write(true).open(directory.join("owner.lock"))?;
    lock.try_lock().map_err(io::Error::other)?;
    if directory.join("supervisor.json").exists() || directory.join("result.json").exists() {
        return Err(io::Error::other("resource owner already started; inspect original evidence"));
    }
    let owner = ProcessIdentity::of(std::process::id())?
        .ok_or_else(|| io::Error::other("resource supervisor identity unavailable"))?;
    save_atomic(&directory.join("supervisor.json"), &owner)?;
    let launch: Launch = load(&directory.join("launch.json"))?
        .ok_or_else(|| io::Error::other("resource launch absent"))?;
    seal_inherited_descriptors()?;
    let revoked = Arc::new(AtomicBool::new(false));
    let signal = revoked.clone();
    let (send, authorized) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let input = io::stdin();
        let mut input = input.lock();
        let mut byte = [0];
        let valid = input.read_exact(&mut byte).is_ok() && byte == *b"1";
        let _ = send.send(valid);
        if valid { let _ = input.read(&mut byte); }
        signal.store(true, Ordering::Release);
    });
    let remaining = Duration::from_nanos(launch.deadline_ns.saturating_sub(monotonic_ns()?));
    if authorized.recv_timeout(remaining) != Ok(true) || revoked.load(Ordering::Acquire) {
        return save_atomic(&directory.join("result.json"), &ResultRecord { process: None,
            reason: "not authorized".into(), retirement_error: None, exit_code: None, signal_error: None, stdout: None, stderr: None });
    }
    let mut process = None;
    let mut child = None;
    let mut stdout = None;
    let mut stderr = None;
    let work = (|| -> io::Result<String> {
        if monotonic_ns()? >= launch.deadline_ns { return Ok("lifetime expired before spawn".into()); }
        let mut command = Command::new(&launch.program);
        command.args(&launch.arguments);
        for name in &launch.remove_environment { command.env_remove(name); }
        let mut spawned = command.envs(&launch.environment)
            .current_dir(&launch.directory).process_group(0).stdin(Stdio::null())
            .stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
        let output = spawned.stdout.take().unwrap();
        let errors = spawned.stderr.take().unwrap();
        child = Some(OwnedChild::new(spawned));
        stdout = Some(Output::new(output)?);
        stderr = Some(Output::new(errors)?);
        process = Some(ProcessIdentity::of(child.as_ref().unwrap().id())?
            .ok_or_else(|| io::Error::other("resource disappeared before identity publication"))?);
        save_atomic(&directory.join("ready.json"), process.as_ref().unwrap())?;
        loop {
            stdout.as_mut().unwrap().drain()?;
            stderr.as_mut().unwrap().drain()?;
            if revoked.load(Ordering::Acquire) { return Ok("authority lost".into()); }
            if monotonic_ns()? >= launch.deadline_ns { return Ok("lifetime expired".into()); }
            if child.as_mut().unwrap().exited()? { return Ok("resource exited".into()); }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    let mut result = ResultRecord { process, reason: work.unwrap_or_else(|e| e.to_string()),
        retirement_error: None, exit_code: None, signal_error: None, stdout: None, stderr: None };
    if let Some(child) = child.as_mut() {
        result.retirement_error = child.retire(&mut result.exit_code, &mut result.signal_error).err().map(|e| e.to_string());
    }
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    result.stdout = stdout.map(|output| output.finish(&directory.join("stdout.bin"), drain_deadline));
    result.stderr = stderr.map(|output| output.finish(&directory.join("stderr.bin"), drain_deadline));
    save_atomic(&directory.join("result.json"), &result)
}

// The private supervisor entry accepts only stdin authority and its standard output streams.
// Mark inherited descriptors before starting its threads; Rust-created owner/collector handles
// are already close-on-exec. Keep them usable by the supervisor while removing child authority.
fn seal_inherited_descriptors() -> io::Result<()> {
    for entry in std::fs::read_dir("/dev/fd")? {
        let entry = entry?;
        let Some(fd) = entry.file_name().to_str().and_then(|name| name.parse::<i32>().ok()) else { continue; };
        if fd <= 2 { continue; }
        // SAFETY: fcntl changes only this private supervisor's descriptor flags. A descriptor
        // closed by directory enumeration is ignored; failures on live descriptors refuse spawn.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EBADF) { continue; }
                return Err(error);
            }
            if libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
    }
    Ok(())
}

// Drain output without letting a continuously writing child starve authority/lifetime checks.
// Retain only a prefix; total_bytes and eof distinguish truncation from incomplete collection.
const OUTPUT_LIMIT: usize = 256 * 1024;
struct Output<R> {
    reader: R,
    bytes: Vec<u8>,
    total: u64,
    eof: bool,
}
impl<R: Read + AsRawFd> Output<R> {
    fn new(reader: R) -> io::Result<Self> {
        // SAFETY: this live pipe descriptor belongs to reader for the collector's lifetime.
        unsafe {
            let flags = libc::fcntl(reader.as_raw_fd(), libc::F_GETFL);
            if flags < 0 || libc::fcntl(reader.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Self { reader, bytes: Vec::new(), total: 0, eof: false })
    }
    fn drain(&mut self) -> io::Result<()> {
        if self.eof { return Ok(()); }
        let mut buffer = [0; 8192];
        for _ in 0..16 {
            match self.reader.read(&mut buffer) {
                Ok(0) => { self.eof = true; break; }
                Ok(count) => {
                    self.total = self.total.checked_add(count as u64)
                        .ok_or_else(|| io::Error::other("resource output byte count overflow"))?;
                    let keep = count.min(OUTPUT_LIMIT - self.bytes.len());
                    self.bytes.extend_from_slice(&buffer[..keep]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    fn finish(mut self, path: &Path, deadline: Instant) -> OutputSummary {
        let mut failure = None;
        while !self.eof {
            if let Err(error) = self.drain() { failure = Some(error.to_string()); break; }
            if !self.eof && Instant::now() >= deadline {
                failure = Some("resource output did not close after retirement".into()); break;
            }
            if !self.eof { std::thread::sleep(Duration::from_millis(2)); }
        }
        if let Err(error) = frontcap_core::state::save_atomic_bytes(path, &self.bytes) {
            failure = Some(match failure { Some(prior) => format!("{prior}; output publication: {error}"), None => error.to_string() });
        }
        OutputSummary { retained_bytes: self.bytes.len(), total_bytes: self.total, eof: self.eof, failure }
    }
}

#[cfg(test)]
#[path = "resource_process_tests.rs"]
mod tests;
