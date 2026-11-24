use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("cannot create profile directory: {0}")]
    Profile(std::io::Error),
    #[error("cannot spawn {binary}: {source}")]
    Spawn { binary: String, source: std::io::Error },
    #[error("browser exited before writing DevToolsActivePort: {status}{detail}")]
    ExitedEarly { status: String, detail: String },
    #[error("this host restricts unprivileged user namespaces (AppArmor), so the engine's sandbox cannot start: run `sudo browser-pin engine allow-sandbox` once per host and engine build to install the engine where it may")]
    SandboxRestricted,
    #[error("DevToolsActivePort not written within {0:?}")]
    Timeout(Duration),
    #[error("malformed DevToolsActivePort: {0:?}")]
    BadActivePort(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Checkpoint(String),
}

#[derive(Debug)]
pub struct Launched {
    pub child: Child,
    pub port: u16,
    pub browser_ws_url: String,
    pub profile: tempfile::TempDir,
}

impl Drop for Launched {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn parse_active_port(text: &str) -> Result<(u16, String), LaunchError> {
    let mut lines = text.lines();
    let port = lines
        .next()
        .and_then(|l| l.trim().parse::<u16>().ok())
        .ok_or_else(|| LaunchError::BadActivePort(text.to_string()))?;
    let path = lines
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| l.starts_with("/devtools/browser/"))
        .ok_or_else(|| LaunchError::BadActivePort(text.to_string()))?;
    Ok((port, path))
}

/// Where the engine's diagnostics go: the profile's `stderr.log`, read when it exits before listening.
fn stderr_log(profile: &Path) -> Result<Stdio, LaunchError> {
    std::fs::File::create(profile.join("stderr.log")).map(Stdio::from).map_err(LaunchError::Profile)
}

/// Why `binary` exited before listening, from what it logged.
fn exited_early(profile: &Path, status: std::process::ExitStatus) -> LaunchError {
    let log = std::fs::read(profile.join("stderr.log")).unwrap_or_default();
    let log = String::from_utf8_lossy(&log);
    if log.contains(crate::sandbox::NO_SANDBOX) {
        return LaunchError::SandboxRestricted;
    }
    let line = log.lines().find(|l| l.contains(":FATAL:")).or_else(|| log.lines().rev().find(|l| !l.trim().is_empty()));
    let detail = line.map(|l| format!(": {}", l.trim().chars().take(300).collect::<String>())).unwrap_or_default();
    LaunchError::ExitedEarly { status: status.to_string(), detail }
}

/// Baseline flags for every launch. Determinism-profile flags are added by callers.
pub fn base_args(profile: &Path) -> Vec<String> {
    vec![
        "--remote-debugging-port=0".into(),
        format!("--user-data-dir={}", profile.display()),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "about:blank".into(),
    ]
}

pub fn launch(binary: &Path, extra_args: &[String], timeout: Duration) -> Result<Launched, LaunchError> {
    let profile = tempfile::tempdir().map_err(LaunchError::Profile)?;
    let mut args = extra_args.to_vec();
    args.extend(base_args(profile.path()));
    let mut child = Command::new(binary)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(stderr_log(profile.path())?)
        .spawn()
        .map_err(|source| LaunchError::Spawn { binary: binary.display().to_string(), source })?;
    let marker = profile.path().join("DevToolsActivePort");
    let start = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(exited_early(profile.path(), status));
        }
        if let Ok(text) = std::fs::read_to_string(&marker)
            && let Ok((port, path)) = parse_active_port(&text)
        {
            return Ok(Launched { child, port, browser_ws_url: format!("ws://127.0.0.1:{port}{path}"), profile });
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(LaunchError::Timeout(timeout));
        }
        // Polling a file written by another process; launch plumbing, not a determinism path.
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
#[path = "launch_tests.rs"]
mod tests;

#[derive(Debug)]
pub struct Detached {
    pub pid: u32,
    pub port: u16,
    pub browser_ws_url: String,
}

/// Starts the browser in its own process group with the given profile directory. The returned handle
/// does not own the process: it survives the caller, which terminates it by process identity.
pub fn launch_detached(binary: &Path, extra_args: &[String], profile: &Path, timeout: Duration) -> Result<Detached, LaunchError> {
    use std::os::unix::process::CommandExt;
    std::fs::create_dir_all(profile).map_err(LaunchError::Profile)?;
    let marker = profile.join("DevToolsActivePort");
    let _ = std::fs::remove_file(&marker);
    let mut args = extra_args.to_vec();
    args.extend(base_args(profile));
    let mut child = Command::new(binary)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr_log(profile)?)
        .process_group(0)
        .spawn()
        .map_err(|source| LaunchError::Spawn { binary: binary.display().to_string(), source })?;
    let start = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(exited_early(profile, status));
        }
        if let Ok(text) = std::fs::read_to_string(&marker)
            && let Ok((port, path)) = parse_active_port(&text)
        {
            let pid = child.id();
            // Reap it from a thread if it exits while this process lives; never kill it on drop.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            return Ok(Detached { pid, port, browser_ws_url: format!("ws://127.0.0.1:{port}{path}") });
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            return Err(LaunchError::Timeout(timeout));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
