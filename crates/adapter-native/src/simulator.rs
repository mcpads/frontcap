//! Own only the Simulator created by this acquisition; never adopt an inventory entry after loss.
use crate::{device::AcquisitionError, host::HostSession, process::capture_recorded};
use frontcap_core::{
    ids::LaunchId,
    native::{ownership::Retirement, request::canonical_uuid},
    state::save_atomic,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimulatorLaunch {
    pub xcrun: PathBuf,
    pub runtime: String,
    pub device_type: String,
    pub source_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_volume: Option<crate::storage_volume::Config>,
    /// Fresh volume-worker directory outside a containing session filesystem when required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_volume_root: Option<PathBuf>,
}
impl SimulatorLaunch {
    pub fn validate_data_volume(&self) -> io::Result<()> {
        if let Some(volume) = &self.data_volume { volume.validate()?; }
        if let Some(root) = &self.data_volume_root {
            let parent = root.parent().ok_or_else(|| io::Error::other("Simulator volume parent missing"))?;
            if self.data_volume.is_none() || !root.is_absolute() || root.file_name().is_none()
                || parent.canonicalize()? != parent
                || root.components().any(|part| matches!(part, std::path::Component::ParentDir)) {
                return Err(io::Error::other("Simulator volume root requires a volume and canonical absolute parent"));
            }
        }
        Ok(())
    }
    pub fn data_volume_directory(&self, simulator: &Path) -> PathBuf {
        self.data_volume_root.clone().unwrap_or_else(|| simulator.join("data-volume"))
    }
}
#[derive(Debug, Deserialize)]
struct Inventory {
    devices: BTreeMap<String, Vec<Device>>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Device {
    udid: String,
    name: String,
    state: String,
    device_type_identifier: String,
    #[serde(default)]
    data_path: Option<PathBuf>,
}

pub struct Simulator {
    config: SimulatorLaunch,
    directory: PathBuf,
    host_record: Option<PathBuf>,
    lease: Option<crate::host::HostLease>,
    name: String,
    udid: Option<String>,
    create_started: bool,
    number: u64,
    unretired_commands: Vec<String>,
    retired: Option<Retirement>,
    supervision: Option<crate::resource_handle::Shared>,
    resource_number: u64,
    data_volume: Option<data::DataVolume>,
    data_volume_attempted: bool,
}
impl Simulator {
    pub fn acquire(
        config: SimulatorLaunch,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Self, AcquisitionError> {
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME unavailable"))?;
        Self::acquire_at(
            config,
            directory,
            deadline,
            cancelled,
            &PathBuf::from(home).join(".cache/frontcap"),
        )
    }
    pub(crate) fn acquire_at(
        config: SimulatorLaunch,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
        host: &Path,
    ) -> Result<Self, AcquisitionError> {
        Self::acquire_owned(config, directory, deadline, cancelled, Some(host))
    }
    /// Called only by the live resource group holding the host lease. The group alone publishes
    /// host retirement after its runners and this Simulator have both retired.
    pub(crate) fn acquire_in_group(
        config: SimulatorLaunch, directory: &Path, deadline: Instant, cancelled: &AtomicBool,
        _lease: &crate::host::HostLease,
    ) -> Result<Self, AcquisitionError> {
        Self::acquire_owned(config, directory, deadline, cancelled, None)
    }
    fn acquire_owned(
        config: SimulatorLaunch, directory: &Path, deadline: Instant, cancelled: &AtomicBool,
        host: Option<&Path>,
    ) -> Result<Self, AcquisitionError> {
        if !config
            .runtime
            .starts_with("com.apple.CoreSimulator.SimRuntime.")
            || !config
                .device_type
                .starts_with("com.apple.CoreSimulator.SimDeviceType.")
            || config.source_sha256.len() != 64
            || !config
                .source_sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(io::Error::other("invalid Simulator launch inputs").into());
        }
        config.validate_data_volume()?;
        if config.data_volume.is_some() && host.is_some() {
            return Err(io::Error::other("Simulator data volume requires an independent resource group").into());
        }
        budget(deadline, cancelled)?;
        let lease = host.map(crate::host::acquire).transpose()?;
        std::fs::create_dir(directory)?;
        let directory = directory.canonicalize()?;
        save_atomic(&directory.join("launch.json"), &config)?;
        let name = format!("frontcap-{}", LaunchId::generate());
        save_atomic(&directory.join("name.json"), &name)?;
        let host_record = host.map(|host| host.join("native-session.json"));
        if let Some(path) = &host_record { save_atomic(
            path,
            &HostSession {
                directory: directory.clone(),
                retired: false,
            },
        )?; }
        let mut owner = Self {
            config,
            directory,
            host_record,
            lease,
            name,
            udid: None,
            create_started: false,
            number: 0,
            unretired_commands: vec![],
            retired: None,
            supervision: None,
            resource_number: 0,
            data_volume: None, data_volume_attempted: false,
        };
        if let Err(error) = owner.launch(deadline, cancelled) {
            let cause = error.to_string();
            let _ = save_atomic(&owner.directory.join("acquisition-failure.json"), &cause);
            let retirement = owner.stop();
            return Err(AcquisitionError {
                cause,
                retirement: Some(retirement),
            });
        }
        Ok(owner)
    }
    pub(crate) fn xcrun(&self) -> &Path {
        &self.config.xcrun
    }
    pub fn udid(&self) -> &str {
        self.udid.as_deref().expect("acquired Simulator identity")
    }
    pub(crate) fn canonical_container(&self, path: &Path) -> io::Result<PathBuf> {
        let path = path.canonicalize()?;
        if !path.is_dir() { return Err(io::Error::other("installed container is not a directory")); }
        if self.config.data_volume.is_some() {
            data::validate_container(&self.directory, self.udid(), &path)?;
        } else if !path.components().any(|c| c.as_os_str().to_str()
            .is_some_and(|s| s.eq_ignore_ascii_case(self.udid()))) {
            return Err(io::Error::other("container is outside the owned Simulator"));
        }
        Ok(path)
    }
    fn command(
        &mut self,
        args: &[&str],
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<Vec<u8>> {
        self.command_with_environment(args, &BTreeMap::new(), deadline, cancelled)
    }
    pub(crate) fn command_cursor(&self) -> (&Path, u64) { (&self.directory, self.number) }
    pub(crate) fn command_with_environment(
        &mut self, args: &[&str], environment: &BTreeMap<String, String>,
        deadline: Instant, cancelled: &AtomicBool,
    ) -> io::Result<Vec<u8>> {
        budget(deadline, cancelled)?;
        if self.supervision.is_some() { return self.supervised_command(args, environment, deadline, cancelled); }
        self.number += 1;
        let mut command = Command::new(&self.config.xcrun);
        command.arg("simctl").args(args);
        // Only the app owner supplies child overrides; do not inherit another probe's attachment.
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("SIMCTL_CHILD_") { command.env_remove(key); }
        }
        command.envs(environment);
        let exchange = capture_recorded(
            &mut command,
            deadline,
            cancelled,
            4 * 1024 * 1024,
            &self.directory.join(format!("command-{}", self.number)),
        )?;
        if let Some(error) = &exchange.cleanup_failure {
            self.unretired_commands.push(error.clone());
        }
        if let Some(error) = exchange
            .failure
            .or(exchange.cleanup_failure)
            .or(exchange.evidence_failure)
        {
            return Err(io::Error::other(error));
        }
        Ok(exchange.stdout)
    }
    fn inventory(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Inventory> {
        serde_json::from_slice(&self.command(
            &["list", "devices", "--json"],
            deadline,
            cancelled,
        )?)
        .map_err(io::Error::other)
    }
    fn matching<'a>(&self, inventory: &'a Inventory) -> io::Result<Option<&'a Device>> {
        let id = self.udid();
        let mut found = None;
        for (runtime, devices) in &inventory.devices {
            for device in devices {
                if device.udid.eq_ignore_ascii_case(id) {
                    if found.is_some()
                        || runtime != &self.config.runtime
                        || device.name != self.name
                        || device.device_type_identifier != self.config.device_type
                    {
                        return Err(io::Error::other(
                            "Simulator inventory identity differs from created device",
                        ));
                    }
                    found = Some(device);
                }
            }
        }
        Ok(found)
    }
    fn launch(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        let before = self.inventory(deadline, cancelled)?;
        if before
            .devices
            .values()
            .flatten()
            .any(|device| device.name == self.name)
        {
            return Err(io::Error::other("fresh Simulator name already present"));
        }
        let (name, kind, runtime) = (
            self.name.clone(),
            self.config.device_type.clone(),
            self.config.runtime.clone(),
        );
        budget(deadline, cancelled)?;
        // Persist the uncertainty boundary before simctl can create a device. Lost stdout never
        // authorizes claiming another inventory entry by name or clearing the host fence.
        save_atomic(&self.directory.join("create-intent.json"), &name)?;
        self.create_started = true;
        let output = self.command(&["create", &name, &kind, &runtime], deadline, cancelled)?;
        let udid = std::str::from_utf8(&output)
            .map_err(io::Error::other)?
            .trim();
        if !canonical_uuid(&udid.to_ascii_lowercase()) {
            return Err(io::Error::other("missing canonical created Simulator UUID"));
        }
        self.udid = Some(udid.to_ascii_uppercase());
        save_atomic(&self.directory.join("simulator.json"), &self.udid)?;
        let after = self.inventory(deadline, cancelled)?;
        let device = self
            .matching(&after)?
            .ok_or_else(|| io::Error::other("created Simulator absent from inventory"))?;
        if device.state != "Shutdown" {
            return Err(io::Error::other("new Simulator is not shutdown"));
        }
        if let Some(config) = &self.config.data_volume {
            let source = device.data_path.as_ref().ok_or_else(|| io::Error::other("created Simulator data path absent"))?;
            self.data_volume_attempted = true;
            self.data_volume = Some(data::DataVolume::start(config, source, self.udid(), &self.config.data_volume_directory(&self.directory), deadline)?);
            self.data_volume.as_mut().unwrap().relocate(&self.directory, deadline, cancelled)?;
        }
        let id = self.udid().to_owned();
        self.command(&["boot", &id], deadline, cancelled)?;
        self.command(&["bootstatus", &id, "-b"], deadline, cancelled)?;
        let after = self.inventory(deadline, cancelled)?;
        if !self.matching(&after)?.is_some_and(|d| d.state == "Booted") {
            return Err(io::Error::other("Simulator boot not confirmed"));
        }
        save_atomic(&self.directory.join("ready.json"), &self.udid)
    }
    pub(crate) fn ensure_booted(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        if self.retired.is_some() { return Err(io::Error::other("Simulator already retired")); }
        let inventory = self.inventory(deadline, cancelled)?;
        if !self.matching(&inventory)?.is_some_and(|device| device.state == "Booted") {
            return Err(io::Error::other("owned Simulator is not booted"));
        }
        Ok(())
    }
    /// A higher-level owner must not publish a clean host after losing an owned child/resource.
    pub(crate) fn retain_failure(&mut self, error: String) {
        self.unretired_commands.push(error);
    }
    pub fn stop(&mut self) -> Retirement {
        self.stop_recorded(None, None)
    }
    /// Publish higher-level retirement evidence before releasing the shared host fence.
    pub(crate) fn stop_recorded(
        &mut self,
        additional: Option<&Path>,
        signal_error: Option<String>,
    ) -> Retirement {
        let _writes = crate::process::RetirementWrites::enter();
        if let Some(result) = &self.retired {
            return result.clone();
        }
        if self.supervision.is_some() { return self.stop_supervised(additional, signal_error); }
        let cancelled = AtomicBool::new(false);
        let deadline = Instant::now() + Duration::from_secs(45);
        let mut result = Retirement {
            passed: false,
            errors: self.unretired_commands.clone(),
            signal_error,
        };
        if let Some(id) = self.udid.clone() {
            // An inventory mismatch cannot grant permission to delete a replacement device.
            match self.inventory(deadline, &cancelled).and_then(|inventory| {
                self.matching(&inventory)
                    .map(|d| d.map(|d| d.state.clone()))
            }) {
                Ok(Some(state)) => {
                    if state != "Shutdown" {
                        // Keep shutdown failures separately, then still attempt owned deletion.
                        if let Err(error) = self.command(&["shutdown", &id], deadline, &cancelled) {
                            let _ = save_atomic(
                                &self.directory.join("shutdown-failure.json"),
                                &error.to_string(),
                            );
                        }
                    }
                    if let Err(error) = self.command(&["delete", &id], deadline, &cancelled) {
                        result.errors.push(error.to_string());
                    }
                    match self
                        .inventory(deadline, &cancelled)
                        .and_then(|inventory| self.matching(&inventory).map(|d| d.is_none()))
                    {
                        Ok(true) => (),
                        Ok(false) => result.errors.push("owned Simulator still present".into()),
                        Err(error) => result.errors.push(error.to_string()),
                    }
                }
                Ok(None) => (),
                Err(error) => result.errors.push(error.to_string()),
            }
        } else if self.create_started {
            result
                .errors
                .push("Simulator creation outcome unresolved; no deletion authority".into());
        }
        // Cleanup commands themselves may have failed to retire even if inventory looks empty.
        for error in &self.unretired_commands {
            if !result.errors.contains(error) {
                result.errors.push(error.clone());
            }
        }
        if self.data_volume_attempted && self.data_volume.is_none() {
            result.errors.push("Simulator data-volume acquisition unresolved".into());
        }
        if result.errors.is_empty() && let Some(volume) = self.data_volume.take()
            && let Err(error) = volume.retire(&self.directory, Instant::now() + Duration::from_secs(20)) {
            result.errors.push(error.to_string());
        }
        if self.config.data_volume.is_some() && let Err(error) = frontcap_core::write_budget::save_inherited(
            frontcap_core::write_budget::Class::Retirement, &self.directory.join("data-volume-attempted.json"), &self.data_volume_attempted) {
            result.errors.push(error.to_string());
        }
        result.passed = result.errors.is_empty();
        if let Err(error) = save_atomic(&self.directory.join("retirement.json"), &result) {
            result.errors.push(error.to_string());
        }
        if let Some(path) = additional
            && let Err(error) = save_atomic(path, &result)
        {
            result.errors.push(error.to_string());
        }
        if result.errors.is_empty()
            && let Some(host_record) = &self.host_record
            && let Err(error) = save_atomic(
                host_record,
                &HostSession {
                    directory: self.directory.clone(),
                    retired: true,
                },
            )
        {
            result.errors.push(error.to_string());
        }
        result.passed = result.errors.is_empty();
        if !result.passed {
            let _ = save_atomic(&self.directory.join("retirement.json"), &result);
            if let Some(path) = additional {
                let _ = save_atomic(path, &result);
            }
        }
        self.retired = Some(result.clone());
        self.lease.take();
        result
    }
}
impl Drop for Simulator {
    fn drop(&mut self) {
        self.stop();
    }
}
fn budget(deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
        Err(io::Error::other(
            "Simulator acquisition cancelled or deadline exceeded",
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "simulator_tests.rs"]
pub(crate) mod tests;

#[path = "simulator_supervision.rs"]
mod supervision;

#[path = "simulator_data.rs"]
pub(crate) mod data;
