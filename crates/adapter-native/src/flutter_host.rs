//! Bounded commands through the SDK worker's existing live authority pipe.
use super::*;
use frontcap_core::native::external_entry::Request;
use std::io::Read;

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum Operation<'a> {
    InstalledApps,
    SetAppState { generation: &'a str, state: frontcap_core::native::app_state::State,
        #[serde(skip_serializing_if = "Option::is_none")] call: Option<&'a crate::ios::UiCall> },
    OpenUrl { request: &'a Request },
    SystemUi { call: &'a crate::ios::UiCall },
    RestartApp { generation: &'a str, #[serde(skip_serializing_if = "Option::is_none")] url: Option<&'a str> },
    BeginEntry { generation: &'a str, url: &'a str },
    PollEntry { generation: &'a str, begin_command: u64 },
}

#[derive(Serialize)]
struct Command<'a> { number: u64, timeout_ms: u64, #[serde(flatten)] operation: Operation<'a> }
#[derive(Serialize)]
struct Notify { command: &'static str, number: u64 }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply { number: u64, error: Option<String> }

/// In-memory admission for the next command of this live session. Saved files cannot construct it.
pub struct AdmittedEntry {
    request: Request,
    command_number: u64,
    session_directory: std::path::PathBuf,
}
impl AdmittedEntry {
    pub fn request(&self) -> &Request { &self.request }
    pub fn command_number(&self) -> u64 { self.command_number }
}

impl Session {
    /// Resolve the unique installed custom-scheme owner before retiring any app process.
    pub fn prepare_entry(&mut self, ready: &Ready, url: &str, deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<AdmittedEntry> {
        if self.config.platform != Platform::Ios || self.config.installed_app.is_none() {
            return Err(io::Error::other("cold iOS entry requires a standalone product"));
        }
        let current: Ready = load(&self.directory.join("worker/current-ready.json"))?
            .or(load(&self.directory.join("worker/ready.json"))?)
            .ok_or_else(|| io::Error::other("cold iOS entry has no acquired worker record"))?;
        if &current != ready {
            return Err(io::Error::other("cold iOS entry differs from this worker's current acquisition"));
        }
        let request = crate::flutter_ios_entry::request(ready, url)?;
        let frontcap_core::native::external_entry::Target::Ios { bundle, .. } = &request.target else { unreachable!() };
        let inventory = self.installed_apps(deadline, cancelled)?;
        let declarations = crate::ios_external_entry::inspect(&inventory, bundle, url, || {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                Err(io::Error::other("cold iOS admission cancelled or expired"))
            } else { Ok(()) }
        })?;
        let command_number = self.next_host_number()?;
        let directory = self.directory.join("worker/entry-admissions");
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!("{command_number}.json"));
        if path.exists() { return Err(io::Error::other("cold iOS admission already recorded")); }
        #[derive(Serialize)]
        struct Admission<'a> { request: &'a Request, declarations: &'a crate::ios_external_entry::Admission }
        save_atomic(&path, &Admission { request: &request, declarations: &declarations })?;
        Ok(AdmittedEntry { request, command_number, session_directory: self.directory.clone() })
    }
    /// Send one admitted cold URL and return before waiting for its confirmation or replacement.
    pub fn begin_entry(&mut self, admission: AdmittedEntry, previous: &crate::flutter_app::Acquired,
        ready: &Ready, deadline: Instant, cancelled: &AtomicBool) -> io::Result<crate::flutter_ios_entry::Started> {
        if admission.session_directory != self.directory || admission.command_number != self.next_host_number()?
            || crate::flutter_ios_entry::request(ready, &admission.request.url)? != admission.request {
            return Err(io::Error::other("cold iOS admission no longer belongs to this next command"));
        }
        previous.validate(ready)?;
        let request = &admission.request;
        let started: crate::flutter_ios_entry::Started = serde_json::from_slice(&self.host_command(
            Operation::BeginEntry { generation: &request.generation, url: &request.url }, deadline, cancelled)?)?;
        if started.command_number != admission.command_number {
            return Err(io::Error::other("cold iOS acceptance command differs"));
        }
        started.validate(previous, ready, request)?;
        Ok(started)
    }
    /// Read readiness of that original URL. A waiting result permits separate system UI work.
    pub fn poll_entry(&mut self, started: &crate::flutter_ios_entry::Started, ready: &Ready,
        previous_poll: Option<&crate::flutter_ios_entry::Polled>, deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<crate::flutter_ios_entry::Polled> {
        started.validate(&crate::flutter_app::Acquired::Ios(started.previous.clone()), ready, &started.entry.request)?;
        let result: crate::flutter_ios_entry::Polled = serde_json::from_slice(&self.host_command(
            Operation::PollEntry { generation: &started.entry.request.generation, begin_command: started.command_number },
            deadline, cancelled)?)?;
        result.validate(started, ready, previous_poll)?;
        Ok(result)
    }
    pub(crate) fn next_host_number(&self) -> io::Result<u64> {
        self.host_number.checked_add(1).ok_or_else(|| io::Error::other("host command overflow"))
    }
    pub(crate) fn set_app_state(&mut self, generation:&str, state:frontcap_core::native::app_state::State,
        call:Option<&crate::ios::UiCall>, deadline:Instant, cancelled:&AtomicBool)
        -> io::Result<crate::flutter_app_state::Changed> {
        if self.config.installed_app.is_none() { return Err(io::Error::other("Flutter lifecycle requires a standalone product")); }
        serde_json::from_slice(&self.host_command(Operation::SetAppState {generation,state,call},deadline,cancelled)?).map_err(io::Error::other)
    }
    pub(crate) fn restart_app(&mut self, generation: &str, url: Option<&str>, deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<crate::flutter_app::Restarted> {
        if self.config.installed_app.is_none() { return Err(io::Error::other("Flutter restart requires a standalone product")); }
        serde_json::from_slice(&self.host_command(Operation::RestartApp { generation, url }, deadline, cancelled)?).map_err(io::Error::other)
    }
    pub(crate) fn system_ui(&mut self, call: &crate::ios::UiCall, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<crate::ios::UiReply> {
        let output = self.host_command(Operation::SystemUi { call }, deadline, cancelled)?;
        let reply: crate::ios::UiReply = serde_json::from_slice(&output)?;
        reply.validate(call).map_err(io::Error::other)?;
        Ok(reply)
    }
    fn host_command(&mut self, operation: Operation<'_>, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<Vec<u8>> {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(io::Error::other("Flutter host command cancelled or expired"));
        }
        if self.child.as_mut().ok_or_else(|| io::Error::other("Flutter worker retired"))?.try_wait()?.is_some() {
            return Err(io::Error::other("Flutter worker exited"));
        }
        self.host_number = self.host_number.checked_add(1).ok_or_else(|| io::Error::other("host command overflow"))?;
        let number = self.host_number;
        let directory = self.directory.join("worker/commands").join(number.to_string());
        std::fs::create_dir_all(directory.parent().unwrap())?;
        std::fs::create_dir(&directory)?;
        let timeout_ms = deadline.saturating_duration_since(Instant::now()).as_millis().clamp(1, 120_000) as u64;
        save_atomic(&directory.join("request.json"), &Command { number, timeout_ms, operation })?;
        let control = self.control.as_mut().ok_or_else(|| io::Error::other("Flutter worker authority revoked"))?;
        serde_json::to_writer(&mut *control, &Notify { command: "host", number })?;
        control.write_all(b"\n")?;
        loop {
            if let Some(reply) = load::<Reply>(&directory.join("reply.json"))? {
                if reply.number != number { return Err(io::Error::other("Flutter host response number mismatch")); }
                if let Some(error) = reply.error { return Err(io::Error::other(error)); }
                let mut bytes = Vec::new();
                File::open(directory.join("command.log"))?.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
                if bytes.len() > 4 * 1024 * 1024 { return Err(io::Error::other("Flutter host response exceeds bound")); }
                return Ok(bytes);
            }
            if self.child.as_mut().unwrap().try_wait()?.is_some() {
                return Err(io::Error::other("Flutter worker exited without host response"));
            }
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                self.control.take();
                return Err(io::Error::other("Flutter host response unresolved; authority revoked"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn installed_apps(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Vec<u8>> {
        self.host_command(Operation::InstalledApps, deadline, cancelled)
    }
    pub(crate) fn open_url(&mut self, request: &Request, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        let output = self.host_command(Operation::OpenUrl { request }, deadline, cancelled)?;
        if let frontcap_core::native::external_entry::Target::Android { package, .. } = &request.target {
            crate::external_entry::android_accepted(std::str::from_utf8(&output).map_err(io::Error::other)?, package, false)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "flutter_host_tests.rs"]
mod tests;
