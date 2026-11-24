//! Live group handles for external Android workers; saved identities never grant signal authority.
use super::*;
use frontcap_core::ids::RequestId;

pub use crate::resource_handle::Supervision;
pub(super) use crate::resource_handle::{Shared, Process, with_group};
impl Device {
    /// External host/WebView route. Consumer applications do not participate in supervision.
    pub fn acquire_supervised(config: AndroidLaunch, directory: &Path, deadline: Instant,
        cancelled: &AtomicBool, supervision: Supervision) -> Result<Self, AcquisitionError> {
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME unavailable"))?;
        Self::acquire_owned(config, directory, deadline, cancelled, &PathBuf::from(home).join(".cache/frontcap"), Some(supervision))
    }
    pub(super) fn spawn_owned(&mut self, command: &mut Command, role: &str, evidence: &Path,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<Process> {
        self.apply_adb_endpoint(command);
        let mut confined = self.confined_adb_command(command)?;
        let command = confined.as_mut().unwrap_or(command);
        if let Some(shared) = &self.supervision {
            self.resource_number += 1;
            let id = RequestId::new(format!("{role}-{}", self.resource_number)).map_err(io::Error::other)?;
            let identity = with_group(shared, |group| group.start_resource(id.clone(), command, deadline, cancelled))?;
            save_atomic(&evidence.join("resource.json"), &id)?;
            Ok(Process::Supervised { group: shared.clone(), id, identity, result: None })
        } else { Ok(Process::Direct(OwnedChild::new(command.spawn()?))) }
    }
    pub(super) fn capture_owned(&mut self, command: &mut Command, deadline: Instant,
        cancelled: &AtomicBool, limit: usize, evidence: &Path) -> io::Result<crate::process::Exchange> {
        self.apply_adb_endpoint(command);
        let mut confined = self.confined_adb_command(command)?;
        let command = confined.as_mut().unwrap_or(command);
        if let Some(shared) = &self.supervision {
            with_group(shared, |group| group.capture(command, evidence, deadline, cancelled, limit))
        } else { capture_recorded(command, deadline, cancelled, limit, evidence) }
    }
    pub(super) fn capture_artifact_owned(&mut self, command: &mut Command, deadline: Instant,
        cancelled: &AtomicBool, limit: usize, evidence: &Path) -> io::Result<crate::process::ArtifactExchange> {
        self.apply_adb_endpoint(command);
        let mut confined = self.confined_adb_command(command)?;
        let command = confined.as_mut().unwrap_or(command);
        if let Some(shared) = &self.supervision {
            with_group(shared, |group| group.capture_artifact(command, evidence, "screen.png", deadline, cancelled, limit))
        } else {
            let budget = crate::resource_group::capture::artifact_budget(self.config.artifact_budget_bytes)?;
            let limits = crate::resource_group::capture::remaining_artifact_limits(limit, 0, self.artifact_bytes, budget)?;
            let result = crate::process::capture_artifact_recorded(command, deadline, cancelled, limits, evidence, "screen.png")?;
            self.artifact_bytes += result.stdout_file.as_ref().map_or(0, |file| file.bytes as usize);
            Ok(result)
        }
    }
    pub(super) fn stop_supervised(&mut self) -> Retirement {
        // The group owns forwards, all input epochs, helpers, emulator and host fence. Do not send
        // separate stop effects here after cancellation; EOF reaches the same group retirement path.
        let mut result = Retirement { passed: false, errors: self.unretired_commands.clone(), signal_error: None };
        let shared = self.supervision.as_ref().unwrap();
        let retired = (|| {
            let group = shared.lock().map_err(|_| io::Error::other("resource group lock poisoned"))?
                .take().ok_or_else(|| io::Error::other("resource group retirement already observed"))?;
            group.stop(Instant::now() + Duration::from_secs(35))
        })();
        match retired {
            Ok(retired) => {
                result.errors.extend(retired.errors);
                for child in retired.resources.values() {
                    if result.signal_error.is_none() { result.signal_error = child.signal_error.clone(); }
                }
                if !retired.passed && result.errors.is_empty() { result.errors.push("resource group retirement failed".into()); }
                if let Some(Process::Supervised { id, .. }) = &self.contact_process {
                    if let Some(child) = retired.resources.get(id.as_str()) {
                        if let Err(error) = save_atomic(&self.host_evidence_directory().join("contact-driver/retirement.json"),
                            &(child.exit_code, &child.signal_error, &child.retirement_error)) { result.errors.push(error.to_string()); }
                    } else { result.errors.push("contact transport retirement proof missing".into()); }
                }
                if retired.passed && let Some(service) = &self.webview_reverse
                    && let Err(error) = save_atomic(&self.directory.join("webview-reverse-retired.json"),
                        &(service, true, Option::<String>::None)) { result.errors.push(error.to_string()); }
            }
            Err(error) => result.errors.push(error.to_string()),
        }
        result.passed = result.errors.is_empty();
        if let Err(error) = save_atomic(&self.directory.join("retirement.json"), &result) {
            result.passed = false; result.errors.push(error.to_string());
        }
        self.retired = Some(result.clone());
        result
    }
}
