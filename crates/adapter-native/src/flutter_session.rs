//! Own the Python Flutter SDK worker through a live control pipe, never a saved PID.
use crate::{
    flutter_bridge::Endpoint,
    flutter_store::{Owner, Platform},
};
use frontcap_core::{
    native::ownership::Retirement,
    process::ProcessIdentity,
    state::{load, save_atomic},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flutter: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    pub platform: Platform,
    pub source_sha256: String,
    pub acquisition_timeout_s: u64,
    pub lifetime_s: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android_sdk: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub app_defines: std::collections::BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_ui: Option<crate::flutter_system_ui::Launch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_app: Option<crate::flutter_app::Launch>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_data: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_volume: Option<DataVolume>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_executable: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub android_resources: Option<AndroidResources>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DataVolume { pub size_mib: u32, pub reserve_mib: u32 }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidResources {
    pub avd_home: PathBuf,
    pub avd_inventory: crate::products::BundleInventory,
    pub port: u16,
    pub adb_server_port: u16,
    pub scratch_volume: crate::storage_volume::Config,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_volume_root: Option<PathBuf>,
}
impl Config {
    pub fn data_volume_mount(&self) -> PathBuf {
        if let Some(root) = self.android_resources.as_ref().and_then(|android| android.scratch_volume_root.as_ref()) {
            return root.join("worker/volume/mount");
        }
        if self.android_resources.is_some() { "session/worker/resource-group/scratch-volume/worker/volume/mount" }
        else if self.resource_executable.is_some() { "session/worker/simulator-data-volume/worker/volume/mount" }
        else { "session/worker/simulator-data-volume/mount" }.into()
    }
    pub fn validate_data_volume(&self) -> io::Result<()> {
        if let Some(executable) = &self.resource_executable
            && (!cfg!(target_os = "macos") || !executable.is_absolute() || (self.platform == Platform::Android && self.android_resources.is_none())
                || self.installed_app.is_none()) {
            return Err(io::Error::other("independent Flutter resources require an installed product, an absolute executable with original resource ownership"));
        }
        if let Some(android) = &self.android_resources {
            if !cfg!(target_os = "macos") || self.platform != Platform::Android || self.resource_executable.is_none()
                || self.installed_app.is_none() || self.data_volume.is_some() || !android.avd_home.is_absolute()
                || self.android_sdk.as_ref().is_none_or(|sdk| !sdk.is_absolute())
                || self.avd.as_ref().is_none_or(|name| name.is_empty() || name.len() > 128 || name == "." || name == ".."
                    || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)))
                || android.port < 1024 || !android.port.is_multiple_of(2)
                || android.adb_server_port < 1024 || android.adb_server_port == 5037
                || [android.port, android.port + 1].contains(&android.adb_server_port) {
                return Err(io::Error::other("invalid installed Flutter Android resource configuration"));
            }
            android.avd_inventory.validate()?;
            android.scratch_volume.validate()?;
            if let Some(root) = &android.scratch_volume_root { crate::resource_group::validate_volume_root(root)?; }
        }
        if let Some(volume) = &self.data_volume
            && (!cfg!(target_os = "macos") || self.platform != Platform::Ios || self.installed_app.is_none()
                || !(256..=65536).contains(&volume.size_mib)
                || !(1..=64.min(volume.size_mib / 4)).contains(&volume.reserve_mib)) {
            return Err(io::Error::other("Flutter data volume requires an installed iOS product on macOS and valid bounds"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub python: PathBuf,
    pub worker_script: PathBuf,
    pub config: Config,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub owner: Owner,
    pub endpoint: Endpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_url_target: Option<frontcap_core::native::external_entry::Target>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_ui: Option<frontcap_core::native::ios_ui::UiIdentity>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Stopped,
    Interrupted,
    Failed,
}
/// Worker metadata contains additional diagnostic fields; only terminal state and verified
/// retirement authorize release. This record does not authorize process adoption.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultRecord {
    pub state: State,
    pub retirement: Retirement,
    pub error: Option<String>,
}
pub enum Recovery {
    Active,
    Finished(ResultRecord),
    Unresolved,
}
pub fn inspect(directory: &Path) -> io::Result<Recovery> {
    let lock = match File::options()
        .read(true)
        .write(true)
        .open(directory.join("worker/session.lock"))
    {
        Ok(lock) => lock,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Recovery::Unresolved),
        Err(e) => return Err(e),
    };
    let _lock = match crate::store::RequestLease::try_acquire(lock) {
        Ok(lease) => lease,
        Err(std::fs::TryLockError::WouldBlock) => return Ok(Recovery::Active),
        Err(std::fs::TryLockError::Error(e)) => return Err(e),
    };
    if let Some(result) = crate::worker_control::read_result(directory)? {
        return Ok(Recovery::Finished(result));
    }
    let Some(config) = load::<Config>(&directory.join("config.json"))? else { return Ok(Recovery::Unresolved); };
    config.validate_data_volume()?;
    let Some(executable) = config.resource_executable else { return Ok(Recovery::Unresolved); };
    let group = directory.join("worker/resource-group");
    let Some(binding) = load::<crate::resource_group::Config>(&group.join("config.json"))? else { return Ok(Recovery::Unresolved); };
    if binding.resource_executable != executable || binding.resource_arguments != ["--resource-process"]
        || binding.evidence_root.as_deref() != Some(directory.join("worker").as_path()) {
        return Err(io::Error::other("Flutter resource group differs from original configuration"));
    }
    use crate::resource_group::inspection::{inspect_with_volume_root, Inspection};
    let ports = config.android_resources.as_ref().map(|android| vec![android.port, android.port + 1]).unwrap_or_default();
    let volume = config.android_resources.as_ref().map(|android| &android.scratch_volume);
    if let Some(android) = &config.android_resources
        && let Some(server) = load::<crate::resource_group::AdbServer>(&group.join("adb-server.json"))?
        && (server.id.as_str() != "adb" || server.port != android.adb_server_port) {
        return Err(io::Error::other("Flutter private server differs from original configuration"));
    }
    if binding.temporary_storage_root.is_some() { return Err(io::Error::other("unexpected Flutter external scratch root")); }
    Ok(match inspect_with_volume_root(&group, &directory.join("worker/resource-target"), &ports, volume,
        config.android_resources.as_ref().and_then(|android| android.scratch_volume_root.as_deref()))? {
        Inspection::Active => Recovery::Active,
        Inspection::Finished(result) => Recovery::Finished(ResultRecord {
            state: State::Interrupted,
            retirement: Retirement { passed: result.passed, errors: result.errors,
                signal_error: result.resources.values().find_map(|r| r.signal_error.clone()) },
            error: Some("Flutter worker exited; original independent resources retired".into()),
        }),
        Inspection::Unresolved => Recovery::Unresolved,
    })
}
pub struct Session {
    child: Option<Child>,
    control: Option<ChildStdin>,
    directory: PathBuf,
    config: Config,
    host_number: u64,
    initial_data: Option<initial_data::Application>,
    write_budget: Option<frontcap_core::write_budget::WriteBudget>,
}
impl Session {
    pub fn start(launch: Launch, directory: &Path, deadline: Instant) -> io::Result<Self> {
        Self::start_with_budget(launch, directory, deadline, None)
    }
    pub fn start_with_budget(mut launch: Launch, directory: &Path, deadline: Instant,
        budget: Option<&frontcap_core::write_budget::WriteBudget>) -> io::Result<Self> {
        launch.config.validate_data_volume()?;
        if let Some(app) = &launch.config.installed_app {
            if !launch.config.app_defines.is_empty() {
                return Err(io::Error::other("standalone Flutter product requires frozen build definitions"));
            }
            app.verify(&launch.config.platform)?;
        }
        if let Some(system) = &launch.config.system_ui {
            if launch.config.platform != Platform::Ios { return Err(io::Error::other("system UI requires iOS")); }
            system.validate()?;
            system.manifest.verify(&system.products)?;
        }
        if launch.config.app_defines.len() > 16 || launch.config.app_defines.iter().any(|(key, value)| {
            key.is_empty() || key.len() > 64 || !key.as_bytes()[0].is_ascii_uppercase()
                || key.bytes().any(|b| !b.is_ascii_uppercase() && !b.is_ascii_digit() && b != b'_')
                || key.starts_with("FRONTCAP_") || value.len() > 2048 || value.chars().any(char::is_control)
        }) { return Err(io::Error::other("invalid Flutter app definitions")); }

        if Instant::now() >= deadline
            || !(1..=900).contains(&launch.config.acquisition_timeout_s)
            || !(1..=3500).contains(&launch.config.lifetime_s)
            || (launch.config.installed_app.is_none()
                && (launch.config.flutter.as_ref().is_none_or(|path| !path.is_absolute())
                    || launch.config.project.as_ref().is_none_or(|path| !path.is_absolute())))
            || !launch.worker_script.is_absolute()
            || !launch.python.is_absolute()
        {
            return Err(io::Error::other(
                "invalid Flutter worker launch paths or budget",
            ));
        }
        let admitted_data = initial_data::admit(&launch.config)?;
        std::fs::create_dir(directory)?;
        let directory = directory.canonicalize()?;
        match budget {
            Some(budget) => budget.save_atomic(frontcap_core::write_budget::Class::Ordinary, &directory.join("launch.json"), &launch)?,
            None => save_atomic(&directory.join("launch.json"), &launch)?,
        }
        let initial_data = initial_data::freeze(&mut launch.config, &directory, admitted_data)?;
        match budget {
            Some(budget) => budget.save_atomic(frontcap_core::write_budget::Class::Ordinary, &directory.join("config.json"), &launch.config)?,
            None => save_atomic(&directory.join("config.json"), &launch.config)?,
        }
        let log = crate::diagnostic_log::create_output_with_budget(&directory.join("worker.log"), budget)?;
        let control_directory = crate::worker_control::create(&directory)?;
        let mut command = Command::new(&launch.python);
        frontcap_core::write_budget::WriteBudget::configure_child(&mut command, budget)?;
        command.arg(&launch.worker_script)
            .arg(directory.join("config.json"))
            .arg(directory.join("worker"));
        if let Some(directory) = control_directory { command.arg("--control-directory").arg(directory); }
        let mut child = command.process_group(0)
            .stdin(Stdio::piped())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        let control = child.stdin.take();
        let mut session = Self {
            child: Some(child),
            control,
            directory,
            config: launch.config,
            host_number: 0,
            initial_data,
            write_budget: budget.cloned(),
        };
        let identity =
            ProcessIdentity::of(session.child.as_ref().unwrap().id())?.ok_or_else(|| {
                io::Error::other("Flutter worker vanished before identity publication")
            })?;
        match budget {
            Some(budget) => budget.save_atomic(frontcap_core::write_budget::Class::Retirement, &session.directory.join("worker-process.json"), &identity)?,
            None => save_atomic(&session.directory.join("worker-process.json"), &identity)?,
        }
        // Control frames are small notifications; host requests themselves are saved separately.
        session
            .control
            .as_mut()
            .unwrap()
            .write_all(b"{\"command\":\"start\"}\n")?;
        Ok(session)
    }
    pub fn wait_ready(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<Ready> {
        loop {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                self.control.take();
                return Err(io::Error::other(
                    "Flutter acquisition cancelled or observation deadline expired",
                ));
            }
            if self.child.as_mut().unwrap().try_wait()?.is_some() {
                return Err(io::Error::other("Flutter worker exited before readiness"));
            }
            if let Err(error) = self.prepare_initial_data(deadline, cancelled) {
                self.control.take();
                return Err(error);
            }
            if let Some(ready) = load::<Ready>(&self.directory.join("worker/ready.json"))? {
                if let Err(error) = self.check_initial_data_ready(&ready) {
                    self.control.take();
                    return Err(error);
                }
                ready.owner.validate().map_err(io::Error::other)?;
                crate::flutter_system_ui::validate_ready(&self.config, &ready)?;
                if let Some(target) = &ready.external_url_target {
                    ready.owner.validate_external_target(target).map_err(io::Error::other)?;
                }
                if ready.owner.platform != self.config.platform
                    || ready.owner.source_sha256 != self.config.source_sha256
                    || ready.endpoint.port == 0
                    || ready.endpoint.token.len() != 48
                    || !ready
                        .endpoint
                        .token
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                {
                    return Err(io::Error::other(
                        "Flutter readiness differs from frozen launch",
                    ));
                }
                return Ok(ready);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    pub fn stop(mut self, deadline: Instant) -> io::Result<Recovery> {
        if self.child.as_mut().unwrap().try_wait()?.is_none()
            && let Some(control) = self.control.as_mut() {
                let _ = control.write_all(b"{\"command\":\"stop\"}\n");
            }
        self.control.take();
        loop {
            if self.child.as_mut().unwrap().try_wait()?.is_some() {
                self.child.take();
                loop {
                    let result = inspect(&self.directory)?;
                    if self.config.resource_executable.is_none() || matches!(result, Recovery::Finished(_))
                        || Instant::now() >= deadline { return Ok(result); }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            if Instant::now() >= deadline {
                return Err(io::Error::other(
                    "Flutter retirement unresolved; inspect existing worker",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[path = "flutter_host.rs"]
mod host;
#[path = "flutter_initial_data.rs"]
mod initial_data;
pub use initial_data::apply_in_group as apply_initial_data_in_group;
pub use host::AdmittedEntry;
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

#[cfg(test)]
#[path = "flutter_session_tests.rs"]
mod tests;
