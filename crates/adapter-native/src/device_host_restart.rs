//! Replace an ordinary Android app without reinstalling it or adopting a new emulator.
use super::*;
use frontcap_core::{
    ids::RequestId,
    native::{android_host::{Ready as HostReady, Replacement}, android_webview::AppProcess},
    state::load,
};
use std::collections::BTreeMap;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    previous: HostReady,
    process: frontcap_core::native::android_lifecycle::Process,
    installation: frontcap_core::native::android_lifecycle::Installation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    replacement: Replacement,
    commands: BTreeMap<String, String>,
}

/// Historical evidence only. Never contacts ADB, repeats a restart or publishes live ownership.
pub fn read_host_replacement(root: &Path, id: &RequestId, previous: &AppProcess, url: Option<&str>)
    -> io::Result<Option<Replacement>> {
    let directory = root.join("host-restarts").join(id.as_str());
    let Some(intent): Option<Intent> = load(&directory.join("intent.json"))? else { return Ok(None); };
    if intent.previous.owner != *previous || intent.url.as_deref() != url {
        return Err(io::Error::other("Android host restart request owner or URL conflicts"));
    }
    let Some(proof): Option<Proof> = load(&directory.join("receipt.json"))? else { return Ok(None); };
    let replacement = proof.replacement;
    replacement.validate()?;
    if replacement.previous != intent.previous || replacement.previous_process != intent.process
        || replacement.entry.as_ref().map(|entry| entry.url.as_str()) != url
        || replacement.installation != intent.installation || proof.commands.is_empty()
        || load::<frontcap_core::native::android_lifecycle::Process>(&directory.join("terminated.json"))? != Some(intent.process)
        || load::<AppProcess>(&directory.join("app-process.json"))? != Some(replacement.ready.owner.clone())
    {
        return Err(io::Error::other("Android host restart evidence differs from original intent"));
    }
    if let Some(old) = &replacement.previous.contacts
        && load::<ContactDriver>(&directory.join("input-retired.json"))?.as_ref() != Some(old) {
        return Err(io::Error::other("previous Android input retirement is unconfirmed"));
    }
    if let Some(new) = &replacement.ready.contacts
        && (load::<ContactDriver>(&directory.join("contact-driver/owner.json"))?.as_ref() != Some(new)
            || frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(directory.join("contact-driver/driver.jar"))?) != new.jar_sha256) {
        return Err(io::Error::other("replacement Android input evidence differs"));
    }
    for (name, digest) in proof.commands {
        let parts: Vec<_> = name.split('/').collect();
        if parts.len() != 2 || !parts[0].strip_prefix("command-").is_some_and(|n|
            n.parse::<u64>().is_ok_and(|value| value > 0 && value.to_string() == n))
            || !["command.json", "exchange.json"].contains(&parts[1]) {
            return Err(io::Error::other("invalid Android host restart evidence path"));
        }
        let path = root.join(name);
        if std::fs::symlink_metadata(path.parent().unwrap())?.file_type().is_symlink()
            || std::fs::symlink_metadata(&path)?.file_type().is_symlink()
            || frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(&path)?) != digest {
            return Err(io::Error::other("Android host restart command evidence changed"));
        }
        crate::process::validate_exchange_files(&path)?;
    }
    Ok(Some(replacement))
}

impl Device {
    /// Current process's immutable namespace. Prior producer journals stay at their original paths.
    pub fn host_evidence_directory(&self) -> &Path {
        self.host_epoch.as_deref().unwrap_or(&self.directory)
    }

    fn inspect_host_installation(&mut self, owner: &AppProcess, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<frontcap_core::native::android_lifecycle::Installation> {
        crate::android_installation::installation(&owner.package, &owner.activity, &owner.apk_sha256,
            |args| self.adb(args, deadline, cancelled, false)?
                .ok_or_else(|| io::Error::other("missing Android installation response")))
    }

    fn inspect_host_process(&mut self, owner: &AppProcess, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<frontcap_core::native::android_lifecycle::Process> {
        crate::android_installation::process(&owner.package, &owner.activity, owner.pid,
            |args| self.adb(args, deadline, cancelled, false)?
                .ok_or_else(|| io::Error::other("missing Android process response")))
    }

    // The consumer is already proven absent. Do not synthesize UP/CANCEL or resolve an old pending
    // request. Stop the external producer before any replacement consumer can receive its input.
    fn retire_host_contacts(&mut self, owner: &ContactDriver, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<()> {
        let process = self.contact_process.as_mut()
            .ok_or_else(|| io::Error::other("missing original Android contact transport"))?;
        let mut exit_code = None;
        let mut signal_error = None;
        let result = process.retire(&mut exit_code, &mut signal_error);
        save_atomic(&self.host_evidence_directory().join("contact-driver/retirement.json"),
            &(exit_code, signal_error, result.as_ref().err().map(ToString::to_string)))?;
        result?;
        self.contact_process = None;
        let path = format!("/proc/{}", owner.pid);
        let mut signalled = false;
        loop {
            if self.adb(&["shell", "test", "!", "-d", &path], deadline, cancelled, true)?.is_some() {
                break;
            }
            // Any transport/permission error or PID reuse fails closed, before another signal.
            let stat = self.adb(&["shell", "cat", &format!("{path}/stat")], deadline, cancelled, false);
            let stat = match stat {
                Ok(Some(stat)) => stat,
                Err(error) => {
                    // Exit can occur between the existence check and stat. Only an affirmative
                    // absence check settles it; a failed read alone never proves retirement.
                    if self.adb(&["shell", "test", "!", "-d", &path], deadline, cancelled, false)?.is_some() {
                        break;
                    }
                    return Err(error);
                }
                Ok(None) => return Err(io::Error::other("missing Android contact process stat")),
            };
            if crate::android_webview::start_ticks(&stat, owner.pid)? != owner.process_start_ticks {
                return Err(io::Error::other("Android contact producer identity changed during retirement"));
            }
            if !signalled {
                if let Err(error) = self.adb(&["shell", "kill", "-TERM", &owner.pid.to_string()], deadline, cancelled, false) {
                    // Likewise, the original producer may exit before the signal arrives. Never
                    // send it again; disappearance is the only accepted alternative completion.
                    if self.adb(&["shell", "test", "!", "-d", &path], deadline, cancelled, false)?.is_some() {
                        break;
                    }
                    return Err(error);
                }
                signalled = true;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    }

    /// Same-install replacement. Invalid owners reject before effects; uncertainty after durable
    /// intent retires the owned emulator. Reusing an ID only reads its original completion.
    pub fn restart_host(&mut self, id: &RequestId, previous: &AppProcess, url: Option<&str>, deadline: Instant,
        cancelled: &AtomicBool) -> Result<Replacement, AcquisitionError> {
        if let Some(url) = url { frontcap_core::native::external_entry::validate_url(url).map_err(io::Error::other)?; }
        if let Some(result) = read_host_replacement(&self.directory, id, previous, url)? { return Ok(result); }
        let directory = self.directory.join("host-restarts").join(id.as_str());
        if directory.exists() || self.config.host_app.is_none() || self.retired.is_some()
            || !self.unretired_commands.is_empty() {
            return Err(io::Error::other("Android host restart unresolved or owner unavailable").into());
        }
        self.require_host_foreground()?;
        self.verify_app_process(previous, deadline, cancelled)?;
        let old_directory = self.host_evidence_directory().to_owned();
        let contacts = self.inspect_contact_progress().map(|value| value.owner);
        if self.contact_process.is_some() != contacts.is_some()
            || contacts.as_ref().is_some_and(|owner| owner.consumer.process() != *previous) {
            return Err(io::Error::other("Android host input owner differs").into());
        }
        let first = self.command_number + 1;
        let installation = self.inspect_host_installation(previous, deadline, cancelled)?;
        let process = self.inspect_host_process(previous, deadline, cancelled)?;
        if process.start_ticks != previous.process_start_ticks {
            return Err(io::Error::other("Android host process changed before restart").into());
        }
        let intent = Intent { previous: HostReady { owner: previous.clone(), contacts }, process, installation, url: url.map(str::to_owned) };
        std::fs::create_dir_all(directory.parent().unwrap())?;
        std::fs::create_dir(&directory)?;
        save_atomic(&directory.join("intent.json"), &intent)?;
        self.invalidate_contact_capture();
        let result = (|| -> io::Result<Replacement> {
            // Forwarding belongs to the old process. Its evidence is never overwritten by the next.
            if let Some(port) = self.webview_forward {
                self.retire_webview_forward(port)?;
                self.webview_forward = None;
            }
            self.adb(&["shell", "am", "force-stop", &previous.package], deadline, cancelled, false)?;
            while !self.app_process_absent(&previous.package, previous.pid, deadline, cancelled)? {
                std::thread::sleep(Duration::from_millis(25));
            }
            save_atomic(&directory.join("terminated.json"), &intent.process)?;
            if let Some(owner) = &intent.previous.contacts {
                self.retire_host_contacts(owner, deadline, cancelled)?;
                save_atomic(&directory.join("input-retired.json"), owner)?;
            }
            if self.inspect_host_installation(previous, deadline, cancelled)? != intent.installation {
                return Err(io::Error::other("Android host retirement changed installation"));
            }
            self.contact_state = None;
            self.contact_poisoned = false;
            self.app_process = None;
            self.webview_debugger = None;
            self.webview_pages = None;
            self.webview_control.clear();
            self.webview_lifecycle_uncertain = false;
            self.webview_lifecycle_progress = Default::default();
            self.host_epoch = Some(directory.clone());
            let entry = if let Some(url) = url {
                let args = crate::external_entry::android_arguments(&previous.package, url);
                let output = self.adb(&args.iter().map(String::as_str).collect::<Vec<_>>(), deadline, cancelled, false)?
                    .ok_or_else(|| io::Error::other("missing Android cold URL response"))?;
                crate::external_entry::android_accepted(&output, &previous.package, true)?;
                Some(frontcap_core::native::android_host::UrlAcceptance { owner: previous.clone(), url: url.into(),
                    confirmation: frontcap_core::native::external_entry::Observation::Unobserved,
                    destination: frontcap_core::native::external_entry::Observation::Unobserved })
            } else {
                let component = format!("{}/{}", previous.package, previous.activity);
                self.adb(&["shell", "am", "start", "-n", &component], deadline, cancelled, false)?;
                None
            };
            self.discover_app_process(deadline, cancelled)?;
            let owner = self.app_process.clone().unwrap();
            let process = self.inspect_host_process(&owner, deadline, cancelled)?;
            if self.inspect_host_installation(&owner, deadline, cancelled)? != intent.installation {
                return Err(io::Error::other("Android host replacement changed installation"));
            }
            let mut replacement = Replacement { previous: intent.previous.clone(),
                ready: HostReady { owner, contacts: None }, previous_process: intent.process,
                process, installation: intent.installation, entry };
            // Validate process lineage before starting a new injector.
            let mut process_only = replacement.clone();
            process_only.previous.contacts = None;
            process_only.validate()?;
            if let Some(old) = &replacement.previous.contacts {
                replacement.ready.contacts = Some(self.start_contact_driver_bounded(
                    &old_directory.join("contact-driver/driver.jar"), &old.jar_sha256,
                    ContactDriverLimits { lifetime: Duration::from_millis(old.lifetime_ms), request_capacity: old.request_capacity },
                    deadline, cancelled).map_err(io::Error::other)?);
            }
            replacement.validate()?;
            let mut commands = BTreeMap::new();
            for number in first..=self.command_number {
                for name in ["command.json", "exchange.json"] {
                    let path = format!("command-{number}/{name}");
                    commands.insert(path.clone(), frontcap_core::canonical::sha256_bytes_hex(
                        &std::fs::read(self.directory.join(path))?));
                }
            }
            // Test the boundary after real replacement effects but before their durable proof.
            // This is a returned fault with owned retirement, not simulated abrupt worker death.
            #[cfg(feature = "fault-injection")]
            if std::env::var("FRONTCAP_ANDROID_HOST_RESTART_BEFORE_PROOF").ok().as_deref() == Some(id.as_str()) {
                save_atomic(&directory.join("injected-before-proof.json"), &(id, &replacement))?;
                return Err(io::Error::other("injected Android host restart failure before replacement proof"));
            }
            #[cfg(feature = "fault-injection")]
            if std::env::var("FRONTCAP_ANDROID_HOST_RESTART_LOSE_WORKER_BEFORE_PROOF").ok().as_deref() == Some(id.as_str()) {
                save_atomic(&directory.join("injected-worker-loss.json"), &(id, &replacement))?;
                // Kill only this operation worker. No destructor can fabricate its missing proof;
                // resource supervisors must independently retire the acquired children.
                unsafe { libc::raise(libc::SIGKILL); }
                return Err(io::Error::other("failed to inject Android worker loss"));
            }
            save_atomic(&directory.join("receipt.json"), &Proof { replacement: replacement.clone(), commands })?;
            Ok(replacement)
        })();
        result.map_err(|error| AcquisitionError { cause: error.to_string(), retirement: Some(self.stop()) })
    }
}

#[cfg(test)]
#[path = "device_host_restart_tests.rs"]
mod tests;
