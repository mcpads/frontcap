//! Independent lossy stderr collection. EOF, not controller lifetime, ends collection.
//! Saved identities are inspection evidence and never grant signalling authority.
use frontcap_core::{process::ProcessIdentity, state::{load, save_atomic}, write_budget::{Class, WriteBudget}};
use serde::{Deserialize, Serialize};
use std::{fs::{File, OpenOptions}, io::{self, Read, Write}, os::{fd::OwnedFd,
    unix::{fs::OpenOptionsExt, net::UnixStream, process::CommandExt}}, path::{Path, PathBuf},
    process::{Command, Stdio}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};

/// Trusted launcher-selected executable, inherited by native owners and helpers.
pub const RECORDER_ENV: &str = "FRONTCAP_NATIVE_DIAGNOSTIC_RECORDER";

pub const LIMIT: u64 = 4 * 1024 * 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config { log: PathBuf }
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub limit_bytes: u64,
    pub observed_bytes: u64,
    pub retained_bytes: u64,
    pub eof: bool,
    pub error: Option<String>,
}

/// The supplied trusted executable calls `run` with the appended private directory.
/// The returned writer can outlive the controller; its collector is in a separate process group.
pub fn start(executable: &mut Command, log: &Path, root: &Path) -> io::Result<File> {
    let budget = WriteBudget::from_environment()?;
    start_with_budget(executable, log, root, budget.as_ref())
}

#[derive(Debug)]
pub enum OutputCleanup {
    Unproven,
    NoCollectorStarted,
    CollectorExited { pid: u32 },
}
pub struct OutputStartup {
    pub result: io::Result<File>,
    pub cleanup: OutputCleanup,
}

pub fn start_with_budget(executable: &mut Command, log: &Path, root: &Path, budget: Option<&WriteBudget>) -> io::Result<File> {
    start_with_budget_outcome(executable, log, root, budget).result
}
fn start_with_budget_outcome(executable: &mut Command, log: &Path, root: &Path, budget: Option<&WriteBudget>) -> OutputStartup {
    let mut cleanup = OutputCleanup::NoCollectorStarted;
    let result = start_collector(executable, log, root, budget, &mut cleanup);
    OutputStartup { result, cleanup }
}
fn start_collector(executable: &mut Command, log: &Path, root: &Path, budget: Option<&WriteBudget>, cleanup: &mut OutputCleanup) -> io::Result<File> {
    std::fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    let directory = root.join(format!("{}-{}", std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH).map_err(io::Error::other)?.as_nanos()));
    std::fs::create_dir(&directory)?;
    let log = log.parent().ok_or_else(|| io::Error::other("log parent missing"))?.canonicalize()?
        .join(log.file_name().ok_or_else(|| io::Error::other("log name missing"))?);
    save_control(budget, &directory.join("config.json"), &Config { log })?;
    WriteBudget::configure_child(executable, budget)?;
    let (reader, writer) = UnixStream::pair()?;
    let mut child = executable.arg(&directory).process_group(0)
        .stdin(Stdio::from(OwnedFd::from(reader))).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    *cleanup = OutputCleanup::Unproven;
    let result = (|| {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if child.try_wait()?.is_some() {
                return Err(io::Error::other(format!("diagnostic collector exited; inspect {}", directory.display())));
            }
            if load::<ProcessIdentity>(&directory.join("ready.json"))?.is_some() {
                return Ok(File::from(OwnedFd::from(writer)));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other("diagnostic collector readiness observation expired"));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    if result.is_err() {
        // No writer escaped the failed startup closure. Its dropped pipe signals EOF;
        // only this original Child handle can establish collector retirement.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => { *cleanup = OutputCleanup::CollectorExited { pid: child.id() }; break; }
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
                _ => break,
            }
        }
    }
    std::thread::spawn(move || { let _ = child.wait(); });
    result
}

/// Exclusively creates a diagnostic output. Recorder-managed owners inherit an independent
/// collector executable; standalone library callers without that capability keep direct output.
/// The returned handle may be cloned to merge stdout/stderr into the same bounded stream.
pub fn create_output(log: &Path) -> io::Result<File> {
    let budget = WriteBudget::from_environment()?;
    create_output_with_budget(log, budget.as_ref())
}

pub fn create_output_with_budget(log: &Path, budget: Option<&WriteBudget>) -> io::Result<File> {
    create_output_with_budget_outcome(log, budget).result
}
pub fn create_output_with_budget_outcome(log: &Path, budget: Option<&WriteBudget>) -> OutputStartup {
    let mut cleanup = OutputCleanup::Unproven;
    let result = (|| {
        let file = File::create_new(log)?;
        cleanup = OutputCleanup::NoCollectorStarted;
        let Some(recorder) = std::env::var_os(RECORDER_ENV) else { return Ok(file); };
        let recorder = PathBuf::from(recorder);
        if !recorder.is_absolute() { return Err(io::Error::other("absolute diagnostic recorder path required")); }
        drop(file);
        let mut name = log.file_name().ok_or_else(|| io::Error::other("diagnostic log name missing"))?.to_os_string();
        name.push(".diagnostics");
        let mut command = Command::new(recorder);
        command.arg("--collect-native-diagnostics");
        let startup = start_with_budget_outcome(&mut command, log, &log.with_file_name(name), budget);
        cleanup = startup.cleanup;
        startup.result
    })();
    OutputStartup { result, cleanup }
}

fn append(file: &mut File, bytes: &[u8], budget: Option<&WriteBudget>) -> io::Result<usize> {
    // Concurrent recovery attempts share the same retained-prefix allowance. Never truncate it.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline =>
                std::thread::sleep(Duration::from_millis(5)),
            Err(error) => return Err(io::Error::other(error)),
        }
    }
    let result = (|| {
        let keep = (LIMIT.saturating_sub(file.metadata()?.len()) as usize).min(bytes.len());
        if keep > 0 && let Some(budget) = budget { budget.charge(Class::Ordinary, keep as u64)?; }
        file.write_all(&bytes[..keep])?;
        Ok(keep)
    })();
    let unlocked = file.unlock();
    result.and_then(|kept| unlocked.map(|()| kept))
}

fn save_control<T: Serialize>(budget: Option<&WriteBudget>, path: &Path, value: &T) -> io::Result<()> {
    match budget {
        Some(budget) => budget.save_atomic(Class::Retirement, path, value),
        None => save_atomic(path, value),
    }
}

pub fn run(directory: &Path) -> io::Result<()> {
    let budget = WriteBudget::from_environment()?;
    let lock = OpenOptions::new().write(true).create_new(true).open(directory.join("owner.lock"))?;
    lock.try_lock().map_err(io::Error::other)?;
    let mut summary = Summary { limit_bytes: LIMIT, observed_bytes: 0, retained_bytes: 0, eof: false, error: None };
    let result = (|| {
        let config: Config = load(&directory.join("config.json"))?
            .ok_or_else(|| io::Error::other("diagnostic collector configuration missing"))?;
        if !config.log.is_absolute() { return Err(io::Error::other("absolute log path required")); }
        let mut file = OpenOptions::new().append(true).create(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&config.log)?;
        if !file.metadata()?.is_file() || file.metadata()?.len() > LIMIT {
            return Err(io::Error::other("diagnostic log is not a regular file within its retained bound"));
        }
        let process = ProcessIdentity::of(std::process::id())?
            .ok_or_else(|| io::Error::other("diagnostic collector identity unavailable"))?;
        save_control(budget.as_ref(), &directory.join("ready.json"), &process)?;
        let input = io::stdin();
        let mut input = input.lock();
        let mut bytes = [0; 8192];
        loop {
            let count = match input.read(&mut bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                other => other?,
            };
            if count == 0 { summary.eof = true; break; }
            summary.observed_bytes = summary.observed_bytes.checked_add(count as u64)
                .ok_or_else(|| io::Error::other("diagnostic byte count overflow"))?;
            if summary.error.is_none() {
                match append(&mut file, &bytes[..count], budget.as_ref()) {
                    Ok(kept) => summary.retained_bytes += kept as u64,
                    Err(error) => summary.error = Some(error.to_string()),
                }
            }
            // Keep draining after saturation or file errors; diagnostic failure must not break
            // an independently retiring writer's stderr pipe. Final evidence discloses the loss.
        }
        file.sync_all()
    })();
    if let Err(error) = result {
        summary.error = Some(match summary.error { Some(prior) => format!("{prior}; {error}"), None => error.to_string() });
    }
    save_control(budget.as_ref(), &directory.join("result.json"), &summary)
}

#[cfg(test)]
#[path = "diagnostic_log_tests.rs"]
mod tests;
