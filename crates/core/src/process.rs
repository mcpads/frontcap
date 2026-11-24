//! Process identity: a pid together with its start time, so a reused pid is never mistaken for the
//! process that was recorded. Termination happens only by identity (runtime-lifecycle.md).
use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ProcessIdentity {
    pub pid: u32,
    /// Start time as printed by `ps -o lstart=` in the C locale and UTC; compared verbatim.
    pub started: String,
}

/// Start time of `pid` and whether it has already exited (a zombie), or `None` if no such process.
fn probe(pid: u32) -> std::io::Result<Option<(String, bool)>> {
    // `lstart` is printed in the caller's locale and time zone; a fixed environment makes the recorded
    // identity comparable between processes started from different environments.
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-o", "stat=", "-p", &pid.to_string()])
        .env("TZ", "UTC")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .output()?;
    if !out.status.success() {
        // `ps` exits 1 with no output when the pid is absent. Other failures are not exit evidence.
        if out.status.code() == Some(1) && out.stdout.is_empty() && out.stderr.is_empty() {
            return Ok(None);
        }
        return Err(std::io::Error::other(format!("cannot inspect process {pid}: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let fields: Vec<&str> = text.split_whitespace().collect();
    // `lstart` prints five fields; `stat` follows.
    if fields.len() < 6 {
        return Err(std::io::Error::other(format!("invalid process identity response for {pid}")));
    }
    let (started, state) = fields.split_at(fields.len() - 1);
    Ok(Some((started.join(" "), state[0].starts_with('Z'))))
}

impl ProcessIdentity {
    pub fn of(pid: u32) -> std::io::Result<Option<ProcessIdentity>> {
        Ok(probe(pid)?.map(|(started, _)| ProcessIdentity { pid, started }))
    }

    pub fn current() -> ProcessIdentity {
        ProcessIdentity::of(std::process::id()).ok().flatten().expect("own process identity")
    }

    pub fn is_alive(&self) -> bool {
        self.probe_alive().unwrap_or(false)
    }

    /// Proves the recorded identity has exited; process inspection failures remain errors.
    pub fn has_exited(&self) -> std::io::Result<bool> {
        self.probe_alive().map(|alive| !alive)
    }

    fn probe_alive(&self) -> std::io::Result<bool> {
        Ok(matches!(probe(self.pid)?, Some((started, false)) if started == self.started))
    }

    /// Kills the process only if it is still this identity, and waits until it has exited; returns
    /// whether it killed it.
    pub fn kill(&self) -> std::io::Result<bool> {
        self.kill_with(|| {
            let output = Command::new("kill").args(["-9", &self.pid.to_string()]).output()?;
            if output.status.success() { Ok(()) }
            else { Err(std::io::Error::other(format!("SIGKILL of process {} failed: {}", self.pid, String::from_utf8_lossy(&output.stderr).trim()))) }
        })
    }

    fn kill_with(&self, signal: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<bool> {
        if !self.probe_alive()? {
            return Ok(false);
        }
        if let Err(error) = signal() {
            // A failed signal can race a real exit. Only a fresh probe proves that case; a live
            // target or a failed probe must never be returned as successful termination.
            return if self.probe_alive()? { Err(error) } else { Ok(false) };
        }
        for _ in 0..250 {
            if !self.probe_alive()? {
                return Ok(true);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        Err(std::io::Error::other(format!("process {} did not exit after SIGKILL", self.pid)))
    }
}

#[cfg(test)]
#[path = "process_tests.rs"]
mod tests;
