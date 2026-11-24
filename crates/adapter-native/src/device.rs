//! Owned Android emulator acquisition. The caller must keep this owner alive for the entire session.
//! A persistent host fence prevents a later launch from replacing an unretired acquisition.
#[path = "device_apk.rs"]
mod apk;
use crate::process::{OwnedChild, capture_recorded};
pub use crate::worker::{ClockTransport, InputRegistrationTransport};
use frontcap_core::process::ProcessIdentity;
use frontcap_core::state::save_atomic;
#[cfg(test)]
use frontcap_core::state::load;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

// Shared before/after acquisition and after operation failure. No app PID may be known yet.
const SERVICE_LOG_ARGUMENTS: &[&str] = &[
    "logcat", "-d", "-b", "events", "-b", "system", "-b", "main", "-b", "crash", "-v", "epoch",
    "ActivityManager:I", "ActivityTaskManager:I", "am_anr:I", "am_crash:I",
    "am_proc_start:I", "am_proc_died:I", "AndroidRuntime:E", "libc:F", "DEBUG:F", "linker:E", "*:S",
];

fn diagnostic_output_limit(name: &str) -> usize {
    // The unfiltered broadcast inventory includes other apps and can exceed 1 MiB after boot.
    if name == "broadcasts" { 4 * 1024 * 1024 } else { 1024 * 1024 }
}


fn default_broadcast_interval_ms() -> u32 { 250 }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidLaunch {
    /// Session-wide file-backed screenshot bytes; defaults to 64 MiB, maximum 1 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_budget_bytes: Option<u64>,
    /// Native process acquisition, before any optional WebView or consumer service exists.
    #[serde(default)]
    pub host_app: Option<crate::android_webview::Profile>,
    /// External WebView discovery; does not require consumer clock/input hooks.
    #[serde(default)]
    pub webview_app: Option<crate::android_webview::Profile>,
    /// Instrumented Android View attachment.
    #[serde(default)]
    pub view_app: Option<crate::android_app::Profile>,
    pub emulator: PathBuf,
    pub adb: PathBuf,
    /// Dedicated group-owned ADB server; requires supervised confined emulator storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adb_server_port: Option<u16>,
    pub avd: String,
    /// Optional run-owned AVD inventory; never edits the account's shared AVD registry.
    #[serde(default)]
    pub avd_home: Option<PathBuf>,
    /// Frozen single-device source inventory; copy into supervised scratch before starting writers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avd_inventory: Option<crate::products::BundleInventory>,
    /// Existing storage parent for a fresh owned SDK scratch child; parent is never deleted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temporary_storage_root: Option<PathBuf>,
    /// Optional supervised scratch-volume owner; AVD copying is separately selected by avd_inventory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scratch_volume: Option<crate::storage_volume::Config>,
    /// Restrict emulator descendant direct writes to supervised scratch on macOS.
    #[serde(default)]
    pub confine_emulator_writes: bool,
    pub port: u16,
    #[serde(default)]
    pub cores: Option<u32>,
    #[serde(default)]
    pub memory_mb: Option<u32>,
    /// Opt-in service evidence after boot, before any fixture package operation.
    #[serde(default)]
    pub startup_diagnostics: bool,
    /// Passive pre-fixture queue sampling window; zero disables it, maximum 30 seconds.
    #[serde(default)]
    pub startup_broadcast_sample_ms: u32,
    /// Delay between completed observations, independent of the total sampling window.
    #[serde(default = "default_broadcast_interval_ms")]
    pub startup_broadcast_interval_ms: u32,
    pub apk: PathBuf,
    pub apk_sha256: String,
    /// Prepared declared data, applied only to a fresh installation before first launch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_data: Option<PathBuf>,
    #[serde(default)]
    pub python: PathBuf,
    #[serde(default)]
    pub bridge_script: PathBuf,
    /// Explicit registration route; inherited bridge environment cannot change this selection.
    #[serde(default)]
    pub input_registration: InputRegistrationTransport,
    /// Explicit clock route, independently frozen from input registration.
    #[serde(default)]
    pub clock_transport: ClockTransport,
    pub source_sha256: String,
}
pub use frontcap_core::native::ownership::{Ready, Retirement};
#[derive(Debug, thiserror::Error)]
#[error("native acquisition: {cause}")]
pub struct AcquisitionError {
    pub cause: String,
    pub retirement: Option<Retirement>,
}
impl From<io::Error> for AcquisitionError {
    fn from(error: io::Error) -> Self {
        Self {
            cause: error.to_string(),
            retirement: None,
        }
    }
}
use crate::host::HostSession;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailureObservation {
    pub expected: Ready,
    pub operation_error: String,
    pub started_unix_ms: u128,
    pub commands: Vec<DiagnosticCommand>,
    pub stopped: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticCommand {
    pub name: String,
    pub began_ms: u128,
    pub finished_ms: u128,
    pub failure: Option<String>,
}

#[path = "device_host.rs"]
mod host_process;

#[path = "device_speech.rs"]
mod speech;

#[path = "device_host_restart.rs"]
mod host_restart;
pub use host_restart::read_host_replacement;

#[path = "device_host_entry.rs"]
mod host_entry;
pub use host_entry::read_host_url;

#[path = "device_host_visibility.rs"]
mod host_visibility;
pub use host_visibility::read_host_transition;

#[path = "device_contacts.rs"]
mod contacts;
pub use contacts::{ContactDriver, ContactDriverLimits};
#[path = "device_contact_input.rs"]
mod contact_input;
pub use contact_input::ContactCapture;
#[path = "device_webview_lifecycle.rs"]
mod webview_lifecycle;
#[path = "device_view_restart.rs"]
mod view_restart;
pub use view_restart::read_view_replacement;
#[path = "device_view_lifecycle.rs"]
mod view_lifecycle;
pub use view_lifecycle::read_view_transition;
pub use webview_lifecycle::{WebViewLifecycle, WebViewLifecycleReceipt};

#[path = "device_supervision.rs"]
mod supervision;
#[path = "device_adb_server.rs"]
mod adb_server;
pub use supervision::Supervision;

pub struct Device {
    config: AndroidLaunch,
    directory: PathBuf,
    lease: Option<crate::host::HostLease>,
    host_record: PathBuf,
    process: Option<supervision::Process>,
    adb_server: Option<ProcessIdentity>,
    adb_write_root: Option<PathBuf>,
    supervision: Option<supervision::Shared>,
    resource_number: u64,
    artifact_bytes: usize,
    temporary_disks: Option<crate::emulator_disks::TemporaryDisks>,
    contact_process: Option<supervision::Process>,
    contact_state: Option<contact_input::Session>,
    contact_poisoned: bool,
    webview_lifecycle_uncertain: bool,
    webview_lifecycle_progress: frontcap_core::native::android_progress::Lifecycle,
    view_ready: Option<crate::android_app::Ready>,
    view_epoch: Option<PathBuf>,
    view_restart_uncertain: bool,
    view_lifecycle_uncertain: bool,
    view_lifecycle_state: frontcap_core::native::app_state::State,
    view_lifecycle_process: Option<frontcap_core::native::android_lifecycle::Process>,
    retired_view_entries: Vec<crate::external_entry::Dispatches>,
    entry_dispatches: Option<crate::external_entry::Dispatches>,
    webview_debugger: Option<crate::android_webview::Debugger>,
    app_process: Option<frontcap_core::native::android_webview::AppProcess>,
    host_epoch: Option<PathBuf>,
    host_visibility: Option<frontcap_core::native::app_state::State>,
    webview_forward: Option<u16>,
    webview_control: std::collections::BTreeMap<String,PathBuf>,
    webview_reverse: Option<crate::android_webview::LocalService>,
    webview_pages: Option<Vec<crate::android_webview::Page>>,
    retired: Option<Retirement>,
    command_number: u64,
    unretired_commands: Vec<String>,
}
impl Device {
    /// Shares the existing native runner's per-account OS lock. No ADB server is killed or claimed.
    pub fn acquire(
        config: AndroidLaunch,
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
        config: AndroidLaunch,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
        host: &Path,
    ) -> Result<Self, AcquisitionError> {
        Self::acquire_owned(config, directory, deadline, cancelled, host, None)
    }
    fn acquire_owned(config: AndroidLaunch, directory: &Path, deadline: Instant,
        cancelled: &AtomicBool, host: &Path, supervision: Option<Supervision>) -> Result<Self, AcquisitionError> {
        if config.view_app.is_none() && config.host_app.is_none() && config.webview_app.is_none() { return Err(io::Error::other("missing Android app profile").into()); }
        crate::resource_group::capture::artifact_budget(config.artifact_budget_bytes)?;
        if supervision.is_some() && config.webview_app.is_none() && config.host_app.is_none() {
            return Err(io::Error::other("supervised acquisition requires an external host or WebView profile").into());
        }
        if config.webview_app.is_none() && config.host_app.is_none()
            && (config.python.as_os_str().is_empty()) {
            return Err(io::Error::other("source Android adapter requires a Python path").into());
        }
        if let Some(app) = config.webview_app.as_ref().or(config.host_app.as_ref()) {
            app.validate()?;
            if config.view_app.is_some() || (config.webview_app.is_some() && config.host_app.is_some()) || !config.adb.is_absolute() {
                return Err(io::Error::other("external app requires an exclusive profile and absolute adb").into());
            }
        }
        if let Some(app) = &config.view_app {
            app.validate()?;
            if !config.python.is_absolute() || !config.adb.is_absolute() {
                return Err(io::Error::other("generic Android tools require absolute paths").into());
            }
        }
        if config.avd_home.as_ref().is_some_and(|path| !path.is_absolute() || !path.is_dir()) {
            return Err(io::Error::other("AVD home must be an existing absolute directory").into());
        }
        if let Some(inventory) = &config.avd_inventory {
            inventory.validate()?;
            if config.avd_home.is_none() || supervision.is_none() {
                return Err(io::Error::other("owned AVD copy requires an explicit source home and supervision").into());
            }
        }
        if let Some(volume) = &config.scratch_volume {
            volume.validate()?;
            if config.temporary_storage_root.is_some() || supervision.is_none() {
                return Err(io::Error::other("scratch volume requires supervised acquisition and no external scratch root").into());
            }
        }
        crate::emulator_confinement::validate(config.confine_emulator_writes,
            config.scratch_volume.is_some(), config.avd_inventory.is_some(), supervision.is_some())?;
        if let Some(port) = config.adb_server_port
            && (port < 1024 || port == 5037 || port == config.port || Some(port) == config.port.checked_add(1)
                || !config.confine_emulator_writes || supervision.is_none()) {
            return Err(io::Error::other("private ADB server requires a distinct non-default port and supervised confined emulator storage").into());
        }
        if !(250..=30_000).contains(&config.startup_broadcast_interval_ms) {
            return Err(io::Error::other("broadcast sampling interval must be 250..30000 milliseconds").into());
        }
        if config.startup_broadcast_sample_ms > 30_000 {
            return Err(io::Error::other("broadcast sampling exceeds 30 seconds").into());
        }
        if config.cores == Some(0) || config.memory_mb == Some(0) {
            return Err(io::Error::other("Android resources must be positive").into());
        }
        if config.port == 0
            || !config.port.is_multiple_of(2)
            || config.port == u16::MAX
            || config.avd.is_empty()
            || config.avd.starts_with('-')
            || !digest_valid(&config.source_sha256)
            || !digest_valid(&config.apk_sha256)
        {
            return Err(io::Error::other("invalid Android launch identity").into());
        }
        check_budget(deadline, cancelled)?;
        apk::verify(&config.apk, &config.apk_sha256)?;
        if let Some(source) = &config.initial_data {
            if !source.is_absolute() {
                return Err(io::Error::other("initial app data must be absolute").into());
            }
            let package = config.webview_app.as_ref().or(config.host_app.as_ref())
                .map(|p| p.package.as_str())
                .or_else(|| config.view_app.as_ref().map(|p| p.package.as_str()))
                .ok_or_else(|| io::Error::other("missing Android package profile"))?;
            crate::app_data::Prepared::read(source, package, &config.apk_sha256)?;
        }
        let host_record = host.join("native-session.json");
        let (lease, supervision, directory) = if let Some(supervision) = supervision {
            let parent = directory.parent().ok_or_else(|| io::Error::other("target parent absent"))?.canonicalize()?;
            let name = directory.file_name().ok_or_else(|| io::Error::other("target name absent"))?;
            let target = parent.join(name);
            let group_directory = target.with_file_name(format!("{}-resources", name.to_str().ok_or_else(|| io::Error::other("non-UTF8 target name"))?));
            let group_config = crate::resource_group::Config { host: host.to_owned(), target,
                evidence_root: Some(parent), artifact_budget_bytes: config.artifact_budget_bytes, temporary_storage_root: config.temporary_storage_root.clone(), scratch_volume: config.scratch_volume.clone(), scratch_volume_root: None, resource_executable: supervision.executable.clone(),
                resource_arguments: supervision.resource_arguments, ports: vec![config.port, config.port + 1],
                deadline_ns: supervision.deadline_ns };
            let mut command = Command::new(supervision.executable); command.args(supervision.group_arguments);
            let group = crate::resource_group::Group::start(&mut command, &group_config, &group_directory, deadline)?;
            let target = group.ready.target.clone();
            (None, Some(std::sync::Arc::new(std::sync::Mutex::new(Some(group)))), target)
        } else {
            let lease = crate::host::acquire(host)?;
            std::fs::create_dir(directory)?;
            let directory = directory.canonicalize()?;
            save_atomic(&host_record, &HostSession { directory: directory.clone(), retired: false })?;
            (Some(lease), None, directory)
        };
        save_atomic(&directory.join("launch.json"), &config)?;
        let mut device = Self {
            config,
            directory,
            lease,
            supervision,
            resource_number: 0,
            artifact_bytes: 0,
            host_record,
            process: None,
            adb_server: None,
            adb_write_root: None,
            temporary_disks: None,
            contact_process: None,
            contact_state: None,
            contact_poisoned: false,
            webview_lifecycle_uncertain: false,
            webview_lifecycle_progress: Default::default(),
            view_ready: None,
            view_epoch: None,
            view_restart_uncertain: false,
            view_lifecycle_uncertain: false,
            view_lifecycle_state: frontcap_core::native::app_state::State::Foreground,
            view_lifecycle_process: None,
            retired_view_entries: Vec::new(),
            entry_dispatches: None,
            webview_debugger: None,
            app_process: None,
            host_epoch: None,
            host_visibility: Some(frontcap_core::native::app_state::State::Foreground),
            webview_forward: None,
            webview_control: Default::default(),
            webview_reverse: None,
            webview_pages: None,
            retired: None,
            command_number: 0,
            unretired_commands: Vec::new(),
        };
        let acquired = device.launch(deadline, cancelled);
        if let Err(error) = acquired {
            let cause = error.to_string();
            let _ = save_atomic(&device.directory.join("acquisition-failure.json"), &cause);
            // No app identity exists yet. Retain service evidence only while this owner still
            // has its emulator, without replacing the primary failure or retrying acquisition.
            if device.config.startup_diagnostics && device.process.is_some()
                && device.unretired_commands.is_empty() && !cancelled.load(Ordering::Acquire)
                && let Err(error) = device.observe_services("acquisition-observation",
                    Instant::now() + Duration::from_secs(6), cancelled) {
                let _ = save_atomic(&device.directory.join("acquisition-observation-error.json"),
                    &error.to_string());
            }
            let retirement = device.stop();
            return Err(AcquisitionError {
                cause,
                retirement: Some(retirement),
            });
        }
        Ok(device)
    }
    fn launch(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        check_budget(deadline, cancelled)?;
        // Refuse occupied ports before starting; never issue ADB commands to an existing emulator.
        let console = TcpListener::bind((Ipv4Addr::LOCALHOST, self.config.port))?;
        let transport = TcpListener::bind((Ipv4Addr::LOCALHOST, self.config.port + 1))?;
        let scratch = if let Some(shared) = &self.supervision {
            supervision::with_group(shared, |group| Ok(group.ready.temporary_disks.clone()))?
        } else {
            self.temporary_disks = Some(crate::emulator_disks::TemporaryDisks::create(
                &self.directory, self.config.temporary_storage_root.as_deref())?);
            self.temporary_disks.as_ref().unwrap().path.clone()
        };
        let mut command = Command::new(&self.config.emulator);
        command
            // The SDK's overlay directory ignores TMPDIR on Unix.
            .env("ANDROID_TMP", &scratch)
            .args([
                "-avd",
                &self.config.avd,
                "-no-window",
                "-no-audio",
                "-no-snapshot",
                "-read-only",
                "-no-boot-anim",
                "-port",
                &self.config.port.to_string(),
            ])
            .process_group(0)
            .stdin(Stdio::null());
        if self.supervision.is_none() {
            let log = crate::diagnostic_log::create_output(&self.directory.join("emulator.log"))?;
            command.stdout(log.try_clone()?).stderr(log);
        }
        let avd_home = if let Some(inventory) = &self.config.avd_inventory {
            let source = self.config.avd_home.as_ref().expect("validated source home");
            let shared = self.supervision.as_ref().expect("validated supervision");
            let reply = supervision::with_group(shared, |group| group.execute_until(
                crate::resource_group::Operation::PrepareAvd { source: source.clone(), name: self.config.avd.clone(), inventory: inventory.clone() },
                deadline, cancelled))?;
            match reply {
                crate::resource_group::Reply::AvdPrepared { directory } if directory == scratch.join("avd") => Some(directory),
                _ => return Err(io::Error::other(format!("owned AVD preparation did not complete: {reply:?}"))),
            }
        } else { self.config.avd_home.clone() };
        // Keep failed AVD preparation lightweight: no installable target exists yet.
        // The source APK was validated before acquisition and is reverified while snapshotting.
        apk::snapshot(&self.config.apk, &self.directory.join("app.apk"), &self.config.apk_sha256)?;
        if let Some(home) = &avd_home { command.env("ANDROID_AVD_HOME", home); }
        if let Some(cores) = self.config.cores {
            command.args(["-cores", &cores.to_string()]);
        }
        if let Some(memory_mb) = self.config.memory_mb {
            command.args(["-memory", &memory_mb.to_string()]);
        }
        if self.config.confine_emulator_writes {
            command = crate::emulator_confinement::prepare(&command, &scratch,
                avd_home.as_deref().expect("validated staged AVD"), &self.directory)?;
        }
        if let Some(port) = self.config.adb_server_port {
            self.start_adb_server(&scratch, deadline, cancelled)?;
            command.env("ANDROID_ADB_SERVER_PORT", port.to_string());
        }
        let arguments: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        save_atomic(
            &self.directory.join("emulator-command.json"),
            &(PathBuf::from(command.get_program()), arguments),
        )?;
        drop((console, transport));
        self.process = Some(self.spawn_owned(&mut command, "emulator", &self.directory.clone(), deadline, cancelled)?);
        let pid = self.process.as_ref().unwrap().id();
        let identity = ProcessIdentity::of(pid)?
            .ok_or_else(|| io::Error::other("emulator disappeared during spawn"))?;
        save_atomic(&self.directory.join("emulator-process.json"), &identity)?;
        self.ensure_alive()?;
        let boot = deadline.min(Instant::now() + Duration::from_secs(180));
        loop {
            check_budget(boot, cancelled)?;
            let output = self.adb(
                &["shell", "getprop", "sys.boot_completed"],
                boot,
                cancelled,
                true,
            )?;
            match output.as_deref().map(str::trim) {
                Some("1") => break,
                None | Some("" | "0") => {}
                _ => return Err(io::Error::other("invalid Android boot status")),
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if self.config.startup_diagnostics { self.observe_services("startup-observation", deadline, cancelled)?; }
        if self.config.startup_broadcast_sample_ms != 0 { self.sample_startup_broadcasts(deadline, cancelled)?; }
        // Retain guest-visible resources before installing or starting the fixture. MemTotal
        // excludes reserved memory; it must not be equated with the requested emulator MiB.
        // These required reads share acquisition's remaining budget and the normal ADB cap;
        // a separate diagnostic-sized timeout must not reject an otherwise usable target.
        for (name, path) in [
            ("cpu-present.txt", "/sys/devices/system/cpu/present"),
            ("cpu-online.txt", "/sys/devices/system/cpu/online"),
            ("meminfo.txt", "/proc/meminfo"),
        ] {
            let output = self
                .adb(
                    &["shell", "cat", path],
                    deadline,
                    cancelled,
                    false,
                )?
                .unwrap();
            frontcap_core::state::save_atomic_bytes(&self.directory.join(name), output.as_bytes())?;
        }
        let package = self.config.webview_app.as_ref().or(self.config.host_app.as_ref()).map(|app| app.package.as_str())
            .or_else(|| self.config.view_app.as_ref().map(|app| app.package.as_str()))
            .ok_or_else(|| io::Error::other("missing Android package profile"))?.to_owned();
        let activity = self.config.webview_app.as_ref().or(self.config.host_app.as_ref()).map(|app| app.activity.as_str())
            .or_else(|| self.config.view_app.as_ref().map(|app| app.activity.as_str()))
            .ok_or_else(|| io::Error::other("missing Android activity profile"))?;
        let component = format!("{package}/{activity}");
        // AVD snapshots may already contain the fixture. Clear only this package on our owned device.
        let installed = self
            .adb(
                &[
                    "shell",
                    "pm",
                    "list",
                    "packages",
                    "--user",
                    "0",
                    &package,
                ],
                deadline,
                cancelled,
                false,
            )?
            .unwrap();
        if installed
            .lines()
            .any(|line| !line.trim().is_empty() && !line.trim().starts_with("package:"))
        {
            return Err(io::Error::other("invalid installed-package inventory"));
        }
        if installed
            .lines()
            .any(|line| line.trim() == format!("package:{package}"))
        {
            if self.config.initial_data.is_some() {
                return Err(io::Error::other("initial app data requires a fresh installation"));
            }
            self.adb(
                &["uninstall", &package],
                deadline,
                cancelled,
                false,
            )?;
        }
        let apk = self.directory.join("app.apk");
        let apk = apk
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF8 APK path"))?;
        self.adb(
            &["install", "--no-incremental", apk],
            deadline,
            cancelled,
            false,
        )?;
        if self.config.initial_data.is_some() {
            let activity = component.split_once('/').unwrap().1;
            self.apply_initial_data(&package, activity, deadline, cancelled)?;
        }
        self.adb(
            &["shell", "input", "keyevent", "KEYCODE_WAKEUP"],
            deadline,
            cancelled,
            false,
        )?;
        self.adb(
            &["shell", "wm", "dismiss-keyguard"],
            deadline,
            cancelled,
            false,
        )?;
        if let Some(service) = self.config.webview_app.as_ref().or(self.config.host_app.as_ref()).and_then(|app|app.local_service.clone()) {
            self.connect_webview_service(service,deadline,cancelled)?;
        }
        let mut launch_args = crate::android_app::launch_arguments(&component, self.config.view_app.as_ref());
        // WebView acquisition waits for the process/debugger itself. Activity's -W completion
        // can block on unrelated presentation before that readiness check can run.
        if self.config.webview_app.is_some() || self.config.host_app.is_some() { launch_args.retain(|arg| arg != "-W"); }
        self.adb(
            &launch_args.iter().map(String::as_str).collect::<Vec<_>>(),
            deadline,
            cancelled,
            false,
        )?;
        if self.config.webview_app.is_some() || self.config.host_app.is_some() {
            self.discover_app_process(deadline, cancelled)?;
        }
        if self.config.host_app.is_some() {
            return Ok(());
        }
        if self.config.webview_app.is_some() {
            return self.discover_webview_debugger(deadline, cancelled);
        }
        let endpoint = deadline.min(Instant::now() + Duration::from_secs(20));
        let clock_ready = loop {
            check_budget(endpoint, cancelled)?;
            if let Some(raw) = self
                .adb(
                    &[
                        "shell",
                        "run-as",
                        &package,
                        "cat",
                        "files/clock-control.json",
                    ],
                    endpoint,
                    cancelled,
                    true,
                )?
            {
                break raw;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        self.initialize_view(&clock_ready, deadline, cancelled)
    }

    fn connect_webview_service(&mut self,service:crate::android_webview::LocalService,
        deadline:Instant,cancelled:&AtomicBool) -> io::Result<()> {
        save_atomic(&self.directory.join("webview-reverse-intent.json"),&service)?;
        self.webview_reverse=Some(service.clone());
        self.adb(&["reverse","--no-rebind",&format!("tcp:{}",service.device_port),
            &format!("tcp:{}",service.host_port)],deadline,cancelled,false)?;
        let inventory=self.adb(&["reverse","--list"],deadline,cancelled,false)?.unwrap();
        if !crate::android_webview::has_reverse(&inventory,&service) {return Err(io::Error::other("local service reverse mapping missing"));}
        save_atomic(&self.directory.join("webview-reverse.json"),&service)
    }
    fn retire_webview_service(&mut self,service:&crate::android_webview::LocalService) -> io::Result<()> {
        let mut guest_exited=self.process.as_mut().ok_or_else(||io::Error::other("missing emulator owner"))?.exited()?;
        let cleanup=(|| -> io::Result<()> {
            if !guest_exited {
                let deadline=Instant::now()+Duration::from_secs(10);let cancelled=AtomicBool::new(false);
                let inventory=self.adb(&["reverse","--list"],deadline,&cancelled,false)?.unwrap();
                if crate::android_webview::has_reverse(&inventory,service) {
                    self.adb(&["reverse","--remove",&format!("tcp:{}",service.device_port)],deadline,&cancelled,false)?;
                    let after=self.adb(&["reverse","--list"],deadline,&cancelled,false)?.unwrap();
                    if crate::android_webview::has_reverse(&after,service) {return Err(io::Error::other("local service reverse mapping remains"));}
                }
            }
            Ok(())
        })();
        let diagnostic=cleanup.as_ref().err().map(ToString::to_string);
        if let Err(error)=cleanup {
            guest_exited=self.process.as_mut().unwrap().exited()?;
            if !guest_exited {return Err(error);}
        }
        // Reverse listeners die with the guest; preserve an in-flight cleanup error as evidence.
        // Any unretired ADB subprocess still independently prevents successful device retirement.
        save_atomic(&self.directory.join("webview-reverse-retired.json"),&(service,guest_exited,diagnostic))
    }

    /// Discover the debugger only after a native action creates a WebView. Never adopts a
    /// replacement app process or changes native input ownership.
    pub fn discover_webview_debugger(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        let process = self.app_process.clone().ok_or_else(|| io::Error::other("external app process unavailable"))?;
        loop {
            self.verify_app_process(&process, deadline, cancelled)?;
            let socket = format!("webview_devtools_remote_{}", process.pid);
            let sockets = self.adb(&["shell", "cat", "/proc/net/unix"], deadline, cancelled, false)?.unwrap();
            if sockets.lines().any(|line| line.split_whitespace().last() == Some(&format!("@{socket}"))) {
                self.verify_app_process(&process, deadline, cancelled)?;
                let debugger = crate::android_webview::Debugger {
                    emulator: process.emulator, package: process.package, activity: process.activity,
                    serial: process.serial, pid: process.pid, process_start_ticks: process.process_start_ticks,
                    socket, apk_sha256: process.apk_sha256, source_sha256: process.source_sha256,
                };
                save_atomic(&self.host_evidence_directory().join("webview-debugger.json"), &debugger)?;
                self.webview_debugger = Some(debugger);
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    /// Discovery evidence only; no page clock/input authority is implied.
    pub fn webview_debugger(&self) -> Option<&crate::android_webview::Debugger> {
        if self.retired.is_none() { self.webview_debugger.as_ref() } else { None }
    }
    /// Forward only the acquired debugger. Failure retires the owner; never retries a bind.
    pub fn connect_webview_debugger(&mut self, deadline: Instant, cancelled: &AtomicBool)
        -> Result<u16, AcquisitionError> {
        let work = (|| -> io::Result<u16> {
            if self.retired.is_some() || self.webview_forward.is_some() {
                return Err(io::Error::other("WebView forwarding already attempted or retired"));
            }
            let debugger = self.webview_debugger.clone().ok_or_else(|| io::Error::other("WebView debugger unavailable"))?;
            self.verify_webview_process(&debugger, deadline, cancelled)?;
            let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
            let port = reservation.local_addr()?.port();
            save_atomic(&self.host_evidence_directory().join("webview-forward-intent.json"), &(port, &debugger))?;
            if let Some(shared) = &self.supervision {
                let registration = crate::resource_group::ForwardRegistration {
                    adb: self.config.adb.clone(), server_port: self.config.adb_server_port,
                    write_root: self.adb_write_root.clone(),
                    emulator: debugger.emulator.clone(), serial: debugger.serial.clone(),
                    socket: debugger.socket.clone(), port,
                    bind_evidence: supervision::with_group(shared, |group| Ok(self.directory.join(format!("command-{}", self.command_number + 1))
                        .strip_prefix(&group.ready.evidence_root).map_err(io::Error::other)?.to_owned()))?,
                };
                supervision::with_group(shared, |group| group.register_forward(registration, deadline, cancelled))?;
            }
            self.webview_forward = Some(port);
            drop(reservation);
            self.adb(&["forward", "--no-rebind", &format!("tcp:{port}"),
                &format!("localabstract:{}", debugger.socket)], deadline, cancelled, false)?;
            let inventory = self.adb(&["forward", "--list"], deadline, cancelled, false)?.unwrap();
            if !crate::android_webview::has_forward(&inventory, &debugger, port) {
                return Err(io::Error::other("owned WebView forwarding missing"));
            }
            self.verify_webview_process(&debugger, deadline, cancelled)?;
            save_atomic(&self.host_evidence_directory().join("webview-forward.json"), &(port, &debugger))?;
            Ok(port)
        })();
        work.map_err(|error| AcquisitionError { cause: error.to_string(), retirement: Some(self.stop()) })
    }
    /// Read candidate pages through the owned forwarding. The caller must select a page explicitly.
    pub fn inspect_webview_pages(&mut self, curl: &Path, evidence: &Path,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<Vec<crate::android_webview::Page>> {
        if !curl.is_absolute() || self.retired.is_some() {
            return Err(io::Error::other("page discovery requires an absolute HTTP tool and live owner"));
        }
        let debugger = self.webview_debugger.clone().ok_or_else(|| io::Error::other("WebView debugger unavailable"))?;
        let port = self.webview_forward.ok_or_else(|| io::Error::other("WebView forwarding unavailable"))?;
        self.verify_webview_process(&debugger, deadline, cancelled)?;
        check_budget(deadline, cancelled)?;
        let mut command = Command::new(curl);
        command.args(["--noproxy", "*", "--fail", "--silent", "--show-error", "--max-filesize", "1048576",
            "--max-time", &deadline.saturating_duration_since(Instant::now()).as_secs_f64().to_string(),
            &format!("http://127.0.0.1:{port}/json/list")]);
        let exchange = self.capture_owned(&mut command, deadline, cancelled, 1024*1024, evidence)?;
        if let Some(error) = &exchange.cleanup_failure { self.unretired_commands.push(error.clone()); }
        if let Some(error) = exchange.failure.or(exchange.cleanup_failure).or(exchange.evidence_failure) {
            return Err(io::Error::other(error));
        }
        self.verify_webview_process(&debugger, deadline, cancelled)?;
        let pages = crate::android_webview::pages(&exchange.stdout, port)?;
        save_atomic(&evidence.join("pages.json"), &pages)?;
        self.webview_pages = Some(pages.clone());
        Ok(pages)
    }
    /// Select an enumerated page explicitly; the connection borrows this device's ownership.
    pub async fn connect_webview_page(&mut self, page_id: &str, evidence: &Path,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<crate::android_webview_page::Connection<'_>> {
        let page = self.webview_pages.as_ref().and_then(|pages| pages.iter().find(|page| page.id == page_id))
            .cloned().ok_or_else(|| io::Error::other("page was not enumerated on this owner"))?;
        let debugger = self.webview_debugger.clone().ok_or_else(|| io::Error::other("WebView debugger unavailable"))?;
        self.verify_webview_process(&debugger, deadline, cancelled)?;
        let client = crate::android_webview_page::connect(&page, evidence, deadline).await?;
        if let Err(error) = self.verify_webview_process(&debugger, deadline, cancelled) {
            client.close();
            return Err(error);
        }
        if let Err(error)=save_atomic(&evidence.join("connection-owner.json"),&debugger) {client.close();return Err(error);}
        Ok(crate::android_webview_page::Connection::new(self, debugger, page, client, evidence.to_owned()))
    }
    /// Transfer a released transport to a fresh document journal without replacing its CDP agent.
    pub(crate) fn reuse_webview_transport(&mut self, page_id: &str, previous: &Path,
        evidence: &Path, client: cdp::Client, deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<crate::android_webview_page::Connection<'_>> {
        let page = self.webview_pages.as_ref().and_then(|pages| pages.iter().find(|page| page.id == page_id))
            .cloned().ok_or_else(|| io::Error::other("page was not enumerated on this owner"))?;
        let debugger = self.webview_debugger.clone().ok_or_else(|| io::Error::other("WebView debugger unavailable"))?;
        let original: crate::android_webview::Page = frontcap_core::state::load(&previous.join("selected-page.json"))?
            .ok_or_else(|| io::Error::other("missing original selected page"))?;
        let original_owner: crate::android_webview::Debugger = frontcap_core::state::load(&previous.join("connection-owner.json"))?
            .ok_or_else(|| io::Error::other("missing original connection owner"))?;
        if original.id != page.id || original.websocket != page.websocket || original_owner != debugger {
            return Err(io::Error::other("released transport differs from selected target or owner"));
        }
        self.verify_webview_process(&debugger, deadline, cancelled)?;
        std::fs::create_dir(evidence)?;
        save_atomic(&evidence.join("selected-page.json"), &page)?;
        save_atomic(&evidence.join("connection-owner.json"), &debugger)?;
        Ok(crate::android_webview_page::Connection::new(self, debugger, page, client, evidence.to_owned()))
    }
    /// Reopen the original frame journal on the same acquired guest, without replaying any command.
    pub async fn resume_webview_page(&mut self,evidence:&Path,reconnection:&frontcap_core::ids::RequestId,
        deadline:Instant,cancelled:&AtomicBool)->io::Result<crate::android_webview_page::Connection<'_>> {
        let debugger:crate::android_webview::Debugger=frontcap_core::state::load(&evidence.join("connection-owner.json"))?
            .ok_or_else(||io::Error::other("missing original page owner"))?;
        if self.retired.is_some() || self.webview_debugger.as_ref()!=Some(&debugger) {
            return Err(io::Error::other("original page owner differs from acquired guest"));
        }
        let page:crate::android_webview::Page=frontcap_core::state::load(&evidence.join("selected-page.json"))?
            .ok_or_else(||io::Error::other("missing original selected page"))?;
        if !self.webview_pages.as_ref().is_some_and(|pages|pages.iter().any(|p|p.id==page.id&&p.websocket==page.websocket)) {
            return Err(io::Error::other("original page is not an acquired candidate"));
        }
        let ledger:crate::android_frames::Ledger=frontcap_core::state::load(&evidence.join("frame-producer.json"))?
            .ok_or_else(||io::Error::other("no original frame ledger to reconcile"))?;
        ledger.validate()?;
        self.claim_webview_control(&page.id,evidence)?;
        self.verify_webview_process(&debugger,deadline,cancelled)?;
        let reconnects=evidence.join("reconnections");std::fs::create_dir_all(&reconnects)?;
        let attempt=reconnects.join(reconnection.as_str());
        let client=crate::android_webview_page::connect(&page,&attempt,deadline).await?;
        if let Err(error)=self.verify_webview_process(&debugger,deadline,cancelled) {client.close();return Err(error);}
        if let Err(error)=save_atomic(&attempt.join("resumed.json"),&(evidence.canonicalize()?,&debugger,&ledger)) {
            client.close();return Err(error);
        }
        Ok(crate::android_webview_page::Connection::restored(self,debugger,page,client,evidence.to_owned(),ledger))
    }
    pub(crate) fn capture_webview_png(&mut self,evidence:&Path,deadline:Instant,cancelled:&AtomicBool)->io::Result<PathBuf> {
        let debugger=self.webview_debugger.clone().ok_or_else(||io::Error::other("WebView debugger unavailable"))?;
        self.verify_webview_process(&debugger,deadline,cancelled)?;
        self.capture_app_png(evidence,deadline,cancelled)
    }
    /// Native raster only. No page/clock readiness or targeting authority is implied.
    pub fn capture_app_png(&mut self,evidence:&Path,deadline:Instant,cancelled:&AtomicBool)->io::Result<PathBuf> {
        let process=self.app_process.clone().ok_or_else(||io::Error::other("external app process unavailable"))?;
        self.verify_app_process(&process,deadline,cancelled)?;
        let mut command=Command::new(&self.config.adb);
        command.args(["-s",&process.serial,"exec-out","screencap","-p"]);
        let result=self.capture_artifact_owned(&mut command,deadline,cancelled,16*1024*1024,evidence)?;
        let exchange=&result.exchange;
        if let Some(error)=&exchange.cleanup_failure {self.unretired_commands.push(error.clone());}
        if let Some(error)=exchange.failure.as_ref().or(exchange.cleanup_failure.as_ref()).or(exchange.evidence_failure.as_ref()) {return Err(io::Error::other(error.clone()));}
        self.verify_app_process(&process,deadline,cancelled)?;
        let path=evidence.canonicalize()?.join("screen.png");
        let bytes=result.read_stdout(&path)?;
        if bytes.len()<24 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {return Err(io::Error::other("invalid Android screenshot"));}
        save_atomic(&evidence.join("capture.json"),&(&process,bytes.len(),frontcap_core::canonical::sha256_bytes_hex(&bytes)))?;
        Ok(path)
    }
    pub(crate) fn require_unclaimed_webview(&self, page: &str) -> io::Result<()> {
        if self.webview_control.contains_key(page) {
            return Err(io::Error::other("page control belongs to its original journal"));
        }
        Ok(())
    }
    pub(crate) fn claim_webview_control(&mut self,page:&str,directory:&Path)->io::Result<()> {
        let directory=directory.canonicalize()?;
        if let Some(original)=self.webview_control.get(page) {
            if *original!=directory {return Err(io::Error::other("page control belongs to its original journal"));}
            return Ok(());
        }
        let mut claims=self.webview_control.clone();claims.insert(page.into(),directory);
        save_atomic(&self.host_evidence_directory().join("webview-control.json"),&claims)?;
        self.webview_control=claims;Ok(())
    }
    pub(crate) fn transfer_webview_control(&mut self, page: &str, previous: &Path, directory: &Path) -> io::Result<()> {
        let previous = previous.canonicalize()?;
        let directory = directory.canonicalize()?;
        if previous == directory || self.webview_control.get(page) != Some(&previous) {
            return Err(io::Error::other("document selection requires the original released control journal"));
        }
        let mut claims = self.webview_control.clone();
        claims.insert(page.into(), directory);
        save_atomic(&self.host_evidence_directory().join("webview-control.json"), &claims)?;
        self.webview_control = claims;
        Ok(())
    }
    pub(crate) fn verify_webview_process(&mut self, debugger: &crate::android_webview::Debugger,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        self.verify_app_process(&debugger.process(), deadline, cancelled)
    }
    fn retire_webview_forward(&mut self, port: u16) -> io::Result<()> {
        let debugger = self.webview_debugger.clone().ok_or_else(|| io::Error::other("missing forwarding owner"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        let cancelled = AtomicBool::new(false);
        let inventory = self.adb_recorded(&["forward", "--list"], deadline, &cancelled, false)?.unwrap();
        // A failed no-rebind may have encountered another owner's mapping. Never remove it.
        if crate::android_webview::has_forward(&inventory, &debugger, port) {
            self.adb_recorded(&["forward", "--remove", &format!("tcp:{port}")], deadline, &cancelled, false)?;
            let after = self.adb_recorded(&["forward", "--list"], deadline, &cancelled, false)?.unwrap();
            if crate::android_webview::has_forward(&after, &debugger, port) {
                return Err(io::Error::other("WebView forwarding remains after removal"));
            }
            if TcpStream::connect_timeout(&SocketAddr::from((Ipv4Addr::LOCALHOST, port)), Duration::from_millis(250)).is_ok() {
                return Err(io::Error::other("WebView forwarding listener remains"));
            }
        }
        save_atomic(&self.host_evidence_directory().join("webview-forward-retired.json"), &port)
    }
    fn initialize_view(&mut self, raw: &str, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        let clock: crate::android_app::InitialClock = serde_json::from_str(raw)?;
        clock.validate()?;
        let app = self.config.view_app.as_ref().expect("selected View profile");
        self.view_ready = Some(crate::android_app::Ready {
            package: app.package.clone(), activity: app.activity.clone(),
            serial: format!("emulator-{}", self.config.port), generation: clock.generation,
            pid: clock.pid, source_sha256: self.config.source_sha256.clone(),
            apk_sha256: self.config.apk_sha256.clone(),
        });
        // The generic catalog/screen check also independently confirms PID and foreground Activity.
        // Attachment reads descriptors without issuing a clock request.
        let capture = self.collect_view(&self.directory.join("initial-view"), deadline, cancelled)
            .map_err(io::Error::other)?;
        if capture.now_ns != 0 {
            return Err(io::Error::other("Android View clock changed during attachment"));
        }
        save_atomic(&self.directory.join("view-ready.json"), self.view_ready.as_ref().unwrap())?;
        Ok(())
    }
    pub fn view_ready(&self) -> Option<&crate::android_app::Ready> {
        if self.retired.is_none() && !self.view_restart_uncertain && !self.view_lifecycle_uncertain { self.view_ready.as_ref() } else { None }
    }
    pub(crate) fn view_directory(&self) -> &Path { self.view_epoch.as_deref().unwrap_or(&self.directory) }
    pub(crate) fn has_view_entry(&self, id: &frontcap_core::ids::RequestId) -> bool {
        self.entry_dispatches.as_ref().is_some_and(|entries| entries.contains(id)) || self.retired_view_entries.iter().any(|entries|entries.contains(id))
    }
    /// Dispatch through this live owner, restricted to the acquired View app. The caller must
    /// invalidate observation/input bases before calling and observe the destination separately.
    /// This is OS acceptance only; it neither replaces the owner nor resolves system confirmation.
    pub fn dispatch_view_url(&mut self, id: &frontcap_core::ids::RequestId,
        request: frontcap_core::native::external_entry::Request,
        deadline: Instant, cancelled: &AtomicBool)
        -> io::Result<frontcap_core::native::external_entry::Accepted> {
        use crate::external_entry::{Dispatches, android_arguments, android_accepted};
        use frontcap_core::native::external_entry::Target;
        request.validate().map_err(io::Error::other)?;
        let owner = self.view_ready().ok_or_else(|| io::Error::other("View app is not owned"))?.clone();
        let target = Target::Android { serial: owner.serial.clone(), package: owner.package.clone() };
        if request.target != target || request.generation != owner.generation {
            return Err(io::Error::other("external entry differs from acquired View owner"));
        }
        let package = owner.package.clone();
        let mut dispatches = match self.entry_dispatches.take() {
            Some(dispatches) => dispatches,
            None => Dispatches::create(&self.view_directory().join("external-entry"), target, owner.generation.clone())?,
        };
        let result = dispatches.dispatch(id, request, |request| {
            self.require_view_foreground()?;
            check_budget(deadline, cancelled)?;
            self.ensure_alive()?;
            if !self.unretired_commands.is_empty() {
                return Err(io::Error::other("Android child retirement unresolved"));
            }
            let args = android_arguments(&package, &request.url);
            let output = self.adb(&args.iter().map(String::as_str).collect::<Vec<_>>(), deadline, cancelled, false)?
                .ok_or_else(|| io::Error::other("Android URL output unavailable"))?;
            android_accepted(&output, &package, false)
        });
        self.entry_dispatches = Some(dispatches);
        result
    }
    /// Capture while this live handle owns the emulator. Partial collection never retries and a
    /// failed child retirement poisons further collection, then contributes to device retirement.
    pub fn collect_view(&mut self, directory: &Path, deadline: Instant, cancelled: &AtomicBool)
        -> Result<frontcap_core::native::android_capture::Capture, crate::android_collection::Error> {
        match self.collect_view_observation(directory,deadline,cancelled)? {
            frontcap_core::native::android_capture::Collection::Observed(capture)=>Ok(capture),
            frontcap_core::native::android_capture::Collection::Unavailable(_)=>Err(io::Error::other("source changed during acquisition capture").into()),
        }
    }
    pub fn collect_view_observation(&mut self, directory: &Path, deadline: Instant, cancelled: &AtomicBool)
        -> Result<frontcap_core::native::android_capture::Collection, crate::android_collection::Error> {
        self.require_view_foreground()?;
        let ready = self.view_ready().ok_or_else(|| io::Error::other("View app is not ready"))?.clone();
        self.collect_view_for(&ready, directory, deadline, cancelled)
    }
    fn collect_view_for(&mut self, ready: &crate::android_app::Ready, directory: &Path, deadline: Instant, cancelled: &AtomicBool)
        -> Result<frontcap_core::native::android_capture::Collection, crate::android_collection::Error> {
        check_budget(deadline, cancelled)?;
        self.ensure_alive()?;
        if !self.unretired_commands.is_empty() {
            return Err(io::Error::other("Android child retirement is unresolved").into());
        }
        let app = self.config.view_app.as_ref().ok_or_else(|| io::Error::other("missing View profile"))?;
        let collector = crate::android_collection::Collector {
            python: &self.config.python, script: &app.capture_script,
            adb: &self.config.adb, serial: &ready.serial,
        };
        let result = collector.collect_observation(&crate::android_app::capture_owner(ready), directory, deadline, cancelled);
        if let Err(error) = &result
            && let Some(cleanup) = &error.cleanup_failure {
                self.unretired_commands.push(cleanup.clone());
            }
        if result.is_ok() {
            self.ensure_alive()?;
            check_budget(deadline, cancelled)?;
        }
        result
    }
    pub(crate) fn execute_view_operation(&mut self, request: &crate::request::NativeRequest, evidence: &Path,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<crate::request::Completion> {
        self.require_view_foreground()?;
        check_budget(deadline, cancelled)?;
        self.ensure_alive()?;
        if !self.unretired_commands.is_empty() {return Err(io::Error::other("Android child retirement unresolved"));}
        let owner = self.view_ready().ok_or_else(||io::Error::other("missing View owner"))?.clone();
        let profile = self.config.view_app.as_ref().ok_or_else(||io::Error::other("missing View profile"))?;
        let client = self.view_directory().join(match request {
            crate::request::NativeRequest::ViewText(_) => "view-edit",
            crate::request::NativeRequest::ViewTap(_) => "view-tap",
            crate::request::NativeRequest::ViewScroll(_) => "view-scroll",
            _ => "view-clock",
        });
        let control = crate::android_control::Client { python: &self.config.python, adb: &self.config.adb, owner: &owner, directory: &client };
        let exchange=match request {
            crate::request::NativeRequest::Clock(r) if r.generation==owner.generation => control.exchange(
                &profile.clock_script,evidence,r,deadline,cancelled)?,
            crate::request::NativeRequest::ViewScroll(r) if r.generation==owner.generation => control.exchange(
                &profile.scroll_script,evidence,r,deadline,cancelled)?,
            crate::request::NativeRequest::ViewTap(r) if r.generation==owner.generation => control.exchange(
                &profile.tap_script,evidence,r,deadline,cancelled)?,
            crate::request::NativeRequest::ViewText(r) if r.generation==owner.generation => control.exchange(
                &profile.edit_script,evidence,r,deadline,cancelled)?,
            _=>return Err(io::Error::other("foreign View request/channel")),
        };
        if let Some(error) = &exchange.cleanup_failure { self.unretired_commands.push(error.clone()); }
        if let Some(error) = exchange.failure.or(exchange.cleanup_failure).or(exchange.evidence_failure) {
            return Err(io::Error::other(error));
        }
        let completion=exchange.reply.ok_or_else(||io::Error::other("missing View completion"))?;
        completion.validate(&owner.identity(),request).map_err(io::Error::other)?;
        crate::receipt::verify(&client.join("receipts"),&completion)?;
        check_budget(deadline,cancelled)?;
        Ok(completion)
    }
    fn sample_startup_broadcasts(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        let directory = self.directory.join("startup-broadcasts");
        std::fs::create_dir(&directory)?;
        let started = Instant::now();
        let end = deadline.min(started + Duration::from_millis(self.config.startup_broadcast_sample_ms.into()));
        let mut samples = Vec::new();
        save_atomic(&directory.join("samples.json"), &samples)?;
        while Instant::now() < end {
            check_budget(deadline, cancelled)?;
            self.ensure_alive()?;
            let name = format!("sample-{:04}", samples.len());
            let began_ms = started.elapsed().as_millis();
            let mut command = Command::new(&self.config.adb);
            command.args(["-s", &format!("emulator-{}", self.config.port),
                "shell", "dumpsys", "activity", "broadcasts"]);
            let exchange = self.capture_owned(&mut command, end.min(Instant::now() + Duration::from_secs(2)),
                cancelled, diagnostic_output_limit("broadcasts"), &directory.join(&name))?;
            if let Some(error) = &exchange.cleanup_failure { self.unretired_commands.push(error.clone()); }
            samples.push(DiagnosticCommand { name, began_ms, finished_ms: started.elapsed().as_millis(),
                failure: exchange.failure.clone().or(exchange.cleanup_failure.clone()).or(exchange.evidence_failure.clone()) });
            save_atomic(&directory.join("samples.json"), &samples)?;
            if let Some(error) = exchange.cleanup_failure.or(exchange.evidence_failure) {
                return Err(io::Error::other(error));
            }
            // Rate-limit observation, not guest scheduling. Cancellation remains responsive.
            let next = end.min(Instant::now() + Duration::from_millis(self.config.startup_broadcast_interval_ms.into()));
            while Instant::now() < next {
                check_budget(deadline, cancelled)?;
                std::thread::sleep(Duration::from_millis(10).min(next.saturating_duration_since(Instant::now())));
            }
        }
        check_budget(deadline, cancelled)?;
        // Completion of this observation window says nothing about guest readiness.
        save_atomic(&directory.join("finished.json"), &started.elapsed().as_millis())
    }

    fn observe_services(&mut self, name: &str, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        let directory = self.directory.join(name);
        std::fs::create_dir(&directory)?;
        let started = Instant::now();
        let mut commands = Vec::new();
        for (name, args) in [
            ("service-events", SERVICE_LOG_ARGUMENTS),
            ("system-app-anr", &["shell", "dumpsys", "dropbox", "--print", "system_app_anr"][..]),
            ("broadcasts", &["shell", "dumpsys", "activity", "broadcasts"][..]),
        ] {
            check_budget(deadline, cancelled)?;
            self.ensure_alive()?;
            let began_ms = started.elapsed().as_millis();
            let mut command = Command::new(&self.config.adb);
            command.args(["-s", &format!("emulator-{}", self.config.port)]).args(args);
            let exchange = self.capture_owned(&mut command,
                deadline.min(Instant::now() + Duration::from_secs(2)), cancelled, diagnostic_output_limit(name),
                &directory.join(name))?;
            if let Some(error) = &exchange.cleanup_failure { self.unretired_commands.push(error.clone()); }
            commands.push(DiagnosticCommand {
                name: name.into(), began_ms, finished_ms: started.elapsed().as_millis(),
                failure: exchange.failure.clone().or(exchange.cleanup_failure.clone()).or(exchange.evidence_failure.clone()),
            });
            save_atomic(&directory.join("commands.json"), &commands)?;
            // An unavailable report is observational, but unconfirmed process retirement or
            // failed evidence publication cannot be ignored while starting the fixture.
            if let Some(error) = exchange.cleanup_failure.or(exchange.evidence_failure) {
                return Err(io::Error::other(error));
            }
        }
        Ok(())
    }
    fn adb(
        &mut self,
        args: &[&str],
        deadline: Instant,
        cancelled: &AtomicBool,
        allow_missing: bool,
    ) -> io::Result<Option<String>> {
        check_budget(deadline, cancelled)?;
        self.ensure_alive()?;
        let output = self.adb_recorded(args, deadline, cancelled, allow_missing)?;
        self.ensure_alive()?;
        Ok(output)
    }
    // Only forwarding cleanup may call this without a live emulator; the ADB server outlives it.
    fn adb_recorded(&mut self, args: &[&str], deadline: Instant, cancelled: &AtomicBool,
        allow_missing: bool) -> io::Result<Option<String>> {
        check_budget(deadline, cancelled)?;
        self.command_number += 1;
        let mut command = Command::new(&self.config.adb);
        command
            .args(["-s", &format!("emulator-{}", self.config.port)])
            .args(args);
        let exchange = self.capture_owned(
            &mut command,
            deadline.min(Instant::now() + Duration::from_secs(20)),
            cancelled,
            1024 * 1024,
            &self
                .directory
                .join(format!("command-{}", self.command_number)),
        )?;
        if let Some(error) = &exchange.cleanup_failure {
            self.unretired_commands.push(error.clone());
        }
        if let Some(error) = exchange.cleanup_failure.or(exchange.evidence_failure) {
            return Err(io::Error::other(error));
        }
        if let Some(error) = exchange.failure {
            if allow_missing
                && error == "bridge exited unsuccessfully"
                && exchange.exit_code.is_some_and(|code| code != 0)
            {
                return Ok(None);
            }
            return Err(io::Error::other(error));
        }
        String::from_utf8(exchange.stdout)
            .map(Some)
            .map_err(io::Error::other)
    }
    fn ensure_alive(&mut self) -> io::Result<()> {
        if let Some(server) = &self.adb_server && server.has_exited()? {
            return Err(io::Error::other("owned ADB server exited"));
        }
        if self
            .process
            .as_mut()
            .ok_or_else(|| io::Error::other("emulator not acquired"))?
            .exited()?
        {
            return Err(io::Error::other("owned emulator exited"));
        }
        Ok(())
    }
    pub fn stop(&mut self) -> Retirement {
        let _writes = crate::process::RetirementWrites::enter();
        if let Some(result) = &self.retired {
            return result.clone();
        }
        if self.supervision.is_some() { return self.stop_supervised(); }
        let mut result = Retirement {
            passed: true,
            errors: self.unretired_commands.clone(),
            signal_error: None,
        };
        if let Some(service)=self.webview_reverse.clone()
            && let Err(error)=self.retire_webview_service(&service) {result.errors.push(error.to_string());}
        if let Some(port) = self.webview_forward
            && let Err(error) = self.retire_webview_forward(port) {
            result.errors.push(error.to_string());
        }
        for error in &self.unretired_commands {
            if !result.errors.contains(error) { result.errors.push(error.clone()); }
        }
        if let Some(process) = self.process.as_mut() {
            if let Err(error) = process.retire(&mut None, &mut result.signal_error) {
                result.errors.push(error.to_string());
            }
            for port in [self.config.port, self.config.port + 1] {
                let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
                match TcpStream::connect_timeout(&address, Duration::from_millis(250)) {
                    Err(error) if error.kind() == io::ErrorKind::ConnectionRefused => {}
                    other => result.errors.push(format!(
                        "emulator port {port} absence unconfirmed: {other:?}"
                    )),
                }
            }
        }
        if let Some(process) = self.contact_process.as_mut() {
            let mut signal_error = None;
            let mut exit_code = None;
            let failure = process.retire(&mut exit_code, &mut signal_error).err().map(|e|e.to_string());
            if let Some(error) = &failure {
                result.errors.push(format!("contact transport retirement: {error}"));
            }
            if let Err(error) = save_atomic(&self.host_evidence_directory().join("contact-driver/retirement.json"), &(exit_code, signal_error, failure)) {
                result.errors.push(error.to_string());
            }
        }
        if result.errors.is_empty()
            && let Some(disks) = &self.temporary_disks
            && let Err(error) = disks.retire()
        {
            result.errors.push(format!("emulator temporary disk retirement: {error}"));
        }
        result.passed = result.errors.is_empty();
        if let Err(error) = save_atomic(&self.directory.join("retirement.json"), &result) {
            result.errors.push(error.to_string());
        }
        if result.errors.is_empty()
            && let Err(error) = save_atomic(
                &self.host_record,
                &HostSession {
                    directory: self.directory.clone(),
                    retired: true,
                },
            )
        {
            result.errors.push(error.to_string());
            result.passed = false;
            // Retain failure to publish the host's terminal state when evidence storage works.
            let _ = save_atomic(&self.directory.join("retirement.json"), &result);
        }
        result.passed = result.errors.is_empty();
        self.retired = Some(result.clone());
        self.lease.take();
        result
    }
}
impl Drop for Device {
    fn drop(&mut self) {
        self.stop();
    }
}
fn digest_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
fn check_budget(deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
    if cancelled.load(Ordering::Acquire) {
        Err(io::Error::other("native acquisition cancelled"))
    } else if Instant::now() >= deadline {
        Err(io::Error::other("native acquisition deadline exceeded"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "device_tests.rs"]
pub(crate) mod tests;

#[path = "device_app_data.rs"]
mod app_data;
