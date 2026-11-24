//! Own the frozen XCTest invocation and its Simulator; controller protocols are separate.
use crate::{
    device::AcquisitionError,
    resource_handle::Process,
    products::ProductsManifest,
    simulator::{Simulator, SimulatorLaunch},
};
use frontcap_core::{native::ownership::Retirement, process::ProcessIdentity, state::save_atomic};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct XctestLaunch {
    /// Platform-owned XCTest installation target, distinct from the controlled consumer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_app: Option<BootstrapApp>,
    /// Declared persisted files for a fresh app, applied after runner readiness and before the first app-launch request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_data: Option<PathBuf>,
    pub simulator: SimulatorLaunch,
    pub xcodebuild: PathBuf,
    pub products: PathBuf,
    pub manifest: ProductsManifest,
    pub app: AppProfile,
}
/// Runner inputs for an already-owned simulator. No second acquisition or host lease is created.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerLaunch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_app: Option<BootstrapApp>,
    /// Bound the SDK result bundle independently; omitted for legacy/whole-session-volume routes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_bundle_volume: Option<crate::storage_volume::Config>,
    pub xcodebuild: PathBuf,
    pub products: PathBuf,
    pub manifest: ProductsManifest,
    pub app: AppProfile,
}
impl From<XctestLaunch> for RunnerLaunch {
    fn from(value: XctestLaunch) -> Self {
        Self {
            bootstrap_app: value.bootstrap_app,
            result_bundle_volume: None,
            xcodebuild: value.xcodebuild,
            products: value.products,
            manifest: value.manifest,
            app: value.app,
        }
    }
}
/// Explicit frozen platform product; never inferred from the consumer's source tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapApp {
    pub path: PathBuf,
    pub inventory: crate::products::BundleInventory,
}
/// App-specific discovery is explicit; it grants access only to these two installed bundles.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppProfile {
    pub bundle: String,
    pub runner_bundle: String,
    pub test_identifier: String,
    pub ui_descriptor: PathBuf,
    pub clock_descriptor: PathBuf,
}
#[cfg(test)]
impl Default for AppProfile {
    fn default() -> Self {
        Self {
            bundle: "org.example.app".into(),
            runner_bundle: "org.example.app.uitests.xctrunner".into(),
            test_identifier: "SampleAppTests/SampleAppTests/testWorkflowBridge".into(),
            ui_descriptor: "Documents/ui-control/ui-control.json".into(),
            clock_descriptor: "Documents/clock-control.json".into(),
        }
    }
}
impl AppProfile {
    fn validate(&self, products: &ProductsManifest) -> io::Result<()> {
        for bundle in [&self.bundle, &self.runner_bundle] {
            if bundle.split('.').any(|part| {
                part.is_empty() || !part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-')
            }) {
                return Err(io::Error::other("invalid configured bundle identity"));
            }
        }
        let identifiers: Vec<_> = self.test_identifier.split('/').collect();
        if self.bundle == self.runner_bundle
            || identifiers.len() != 3
            || identifiers[0] != products.names.tests
            || identifiers.iter().any(|part| {
                part.is_empty() || !part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            })
        {
            return Err(io::Error::other("invalid configured XCTest identity"));
        }
        for path in [&self.ui_descriptor, &self.clock_descriptor] {
            if !path.starts_with("Documents")
                || path.extension().and_then(|s| s.to_str()) != Some("json")
                || path.as_os_str().is_empty()
                || path
                    .components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(io::Error::other(
                    "descriptor must be a relative container descendant",
                ));
            }
        }
        Ok(())
    }
}
pub struct Xctest {
    simulator: Option<Simulator>,
    ready_process: Option<ProcessIdentity>,
    app: AppProfile,
    runner_executable: String,
    runner: Option<Process>,
    bundle_volume: Option<bundle_volume::BundleVolume>,
    bundle_writer: Option<ProcessIdentity>,
    bundle_writers_retired: bool,
    directory: PathBuf,
    retired: Option<Retirement>,
    xcrun: PathBuf,
    discovery_number: u64,
    containers: std::collections::BTreeMap<String, PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiReady {
    #[serde(default)]
    pub app_restart: bool,
    #[serde(default)]
    pub app_entry: bool,
    #[serde(default)]
    pub app_lifecycle: bool,
    #[serde(default)]
    pub system_alerts: bool,
    #[serde(default)]
    pub system_only: bool,
    #[serde(default)]
    pub host_app_state: bool,
    pub port: u16,
    pub identity: frontcap_core::native::ios_ui::UiIdentity,
    pub descriptor: PathBuf,
    pub process: ProcessIdentity,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyDescriptor {
    #[serde(default)]
    app_restart: bool,
    #[serde(default)]
    app_entry: bool,
    #[serde(default)]
    app_lifecycle: bool,
    #[serde(default)]
    system_alerts: bool,
    #[serde(default)]
    system_only: bool,
    #[serde(default)]
    host_app_state: bool,
    generation: String,
    pid: u32,
    port: u16,
    status: String,
    last_id: u64,
}
impl Xctest {
    pub fn start(
        config: XctestLaunch,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Self, AcquisitionError> {
        let home = std::env::var_os("HOME").ok_or_else(|| io::Error::other("HOME unavailable"))?;
        Self::start_at(
            config,
            directory,
            deadline,
            cancelled,
            &PathBuf::from(home).join(".cache/frontcap"),
        )
    }
    pub(crate) fn start_at(
        config: XctestLaunch,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
        host: &Path,
    ) -> Result<Self, AcquisitionError> {
        Self::start_owned(config, directory, deadline, cancelled, Some(host), None)
    }
    pub(crate) fn start_in_group(
        config: XctestLaunch, directory: &Path, deadline: Instant, cancelled: &AtomicBool,
        group: crate::resource_group::Group,
    ) -> Result<Self, AcquisitionError> {
        Self::start_owned(config, directory, deadline, cancelled, None, Some(group))
    }
    fn start_owned(
        config: XctestLaunch, directory: &Path, deadline: Instant, cancelled: &AtomicBool,
        host: Option<&Path>, group: Option<crate::resource_group::Group>,
    ) -> Result<Self, AcquisitionError> {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(
                io::Error::other("XCTest acquisition cancelled or deadline exceeded").into(),
            );
        }
        if config.manifest.role != crate::products::ProductRole::ClockApp {
            return Err(io::Error::other("fresh XCTest app acquisition requires clock-app products").into());
        }
        config.manifest.validate().map_err(io::Error::other)?;
        config.app.validate(&config.manifest)?;
        if let Some(group) = &group {
            if directory.canonicalize()? != group.ready.target {
                return Err(io::Error::other("UIKit group target differs").into());
            }
        } else { std::fs::create_dir(directory)?; }
        let directory = directory.canonicalize()?;
        save_atomic(&directory.join("launch.json"), &config)?;
        // No Simulator side effects are allowed until the independent executable copy is verified.
        config
            .manifest
            .copy_verified(&config.products, &directory.join("products"))?;
        configuration::prepare(&config.clone().into(), &directory)?;
        let initial_data = app_data::admit(&config, &directory)?;
        let mut simulator = if let Some(group) = group {
            Simulator::acquire_supervised(config.simulator.clone(), group, deadline, cancelled)?
        } else {
            Simulator::acquire_at(config.simulator.clone(), &directory.join("simulator"), deadline,
                cancelled, host.ok_or_else(|| io::Error::other("direct XCTest host missing"))?)?
        };
        if let Some((prepared, product)) = initial_data
            && let Err(error) = app_data::stage(&mut simulator, &config, &directory, prepared, product, deadline, cancelled)
        {
            let cause = error.to_string();
            let _ = save_atomic(&directory.join("acquisition-failure.json"), &cause);
            return Err(AcquisitionError { cause, retirement: Some(simulator.stop()) });
        }
        Self::start_verified(config.into(), simulator, directory, deadline, cancelled)
    }
    /// Transfer exclusive ownership of an existing simulator to its XCTest runner.
    /// Failures retire that owned simulator; this does not discover or adopt an inventory device.
    /// Runner readiness and attachment to a particular live app still require separate checks.
    pub fn on_simulator(
        config: RunnerLaunch,
        mut simulator: Simulator,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Self, AcquisitionError> {
        let prepare = (|| -> io::Result<PathBuf> {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(io::Error::other(
                    "XCTest attachment cancelled or deadline exceeded",
                ));
            }
            config.manifest.validate().map_err(io::Error::other)?;
            config.app.validate(&config.manifest)?;
            std::fs::create_dir(directory)?;
            let directory = directory.canonicalize()?;
            save_atomic(&directory.join("launch.json"), &config)?;
            save_atomic(&directory.join("simulator.json"), &simulator.udid())?;
            config
                .manifest
                .copy_verified(&config.products, &directory.join("products"))?;
            configuration::prepare(&config, &directory)?;
            simulator.ensure_booted(deadline, cancelled)?;
            Ok(directory)
        })();
        match prepare {
            Ok(directory) => {
                Self::start_verified(config, simulator, directory, deadline, cancelled)
            }
            Err(error) => Err(AcquisitionError {
                cause: error.to_string(),
                retirement: Some(simulator.stop()),
            }),
        }
    }
    fn start_verified(
        config: RunnerLaunch,
        simulator: Simulator,
        directory: PathBuf,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Self, AcquisitionError> {
        let xcrun = simulator.xcrun().to_owned();
        let mut owner = Self {
            simulator: Some(simulator),
            ready_process: None,
            app: config.app.clone(),
            runner_executable: format!("{}-Runner", config.manifest.names.tests),
            runner: None,
            bundle_volume: None,
            bundle_writer: None,
            bundle_writers_retired: false,
            directory,
            retired: None,
            xcrun,
            discovery_number: 0,
            containers: std::collections::BTreeMap::new(),
        };
        if let Err(error) = owner.spawn(&config, deadline, cancelled) {
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
    fn spawn(
        &mut self,
        config: &RunnerLaunch,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<()> {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(io::Error::other(
                "XCTest launch cancelled or deadline exceeded",
            ));
        }
        let bundle_path = if let Some(volume) = &config.result_bundle_volume {
            let target = self.simulator.as_ref().unwrap().command_cursor().0.parent()
                .ok_or_else(|| io::Error::other("XCTest original target root missing"))?.to_owned();
            self.bundle_volume = Some(bundle_volume::BundleVolume::prepare(volume.clone(), &self.directory,
                &target, self.simulator_id(), &self.app.runner_bundle)?);
            self.bundle_volume.as_mut().unwrap().start(deadline)?
        } else { self.directory.join("test.xcresult") };
        let products = self.directory.join("products");
        config.manifest.verify(&products)?;
        let run_configuration = configuration::verified_path(config, &self.directory)?;
        let log = if self.simulator_mut().supervision().is_none() {
            Some(File::create_new(self.directory.join("xctest.log"))?)
        } else { None };
        let mut command = Command::new(&config.xcodebuild);
        command
            .arg("test-without-building")
            .arg("-xctestrun")
            .arg(run_configuration)
            .arg("-destination")
            .arg(format!(
                "platform=iOS Simulator,id={}",
                self.simulator.as_ref().expect("owned simulator").udid()
            ))
            .args([
                "-parallel-testing-enabled",
                "NO",
                "-collect-test-diagnostics",
                "never",
            ])
            .arg(format!("-only-testing:{}", config.app.test_identifier))
            .arg("-resultBundlePath")
            .arg(bundle_path)
            .stdin(Stdio::null())
            .process_group(0);
        if let Some(log) = log { command.stdout(log.try_clone()?).stderr(log); }
        save_atomic(
            &self.directory.join("command.json"),
            &(
                command.get_program().to_string_lossy().into_owned(),
                command
                    .get_args()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>(),
            ),
        )?;
        let evidence = self.directory.clone();
        if let Some(volume) = &mut self.bundle_volume { volume.writer_starting()?; }
        self.runner = Some(self.simulator_mut().spawn_owned(&mut command, &evidence, deadline, cancelled)?);
        let child = self.runner.as_ref().unwrap();
        let identity = ProcessIdentity::of(child.id())?
            .ok_or_else(|| io::Error::other("XCTest vanished before ownership publication"))?;
        self.bundle_writer = Some(identity.clone());
        save_atomic(&self.directory.join("runner-process.json"), &identity)?;
        self.ensure_running()
    }
    /// Process liveness is not a ready XCTest endpoint or an acquired app identity.
    pub fn ensure_running(&mut self) -> io::Result<()> {
        if self.retired.is_some() {
            return Err(io::Error::other("XCTest owner already retired"));
        }
        if self
            .runner
            .as_mut()
            .ok_or_else(|| io::Error::other("missing XCTest child"))?
            .exited()?
        {
            Err(io::Error::other("XCTest runner exited"))
        } else {
            Ok(())
        }
    }
    /// Resolve only an installed container on this owned Simulator; retain every bounded attempt.
    pub fn app_container(
        &mut self,
        bundle: &str,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<PathBuf> {
        self.installed_container(bundle, "data", deadline, cancelled)
    }
    pub(crate) fn installed_container(&mut self, bundle: &str, kind: &str, deadline: Instant,
        cancelled: &AtomicBool) -> io::Result<PathBuf> {
        if !matches!(kind, "app" | "data") { return Err(io::Error::other("unsupported container kind")); }
        if bundle != self.app.bundle && bundle != self.app.runner_bundle {
            return Err(io::Error::other("unsupported owned app bundle"));
        }
        loop {
            self.ensure_running()?;
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(io::Error::other(
                    "XCTest discovery cancelled or deadline exceeded",
                ));
            }
            self.discovery_number += 1;
            let mut command = Command::new(&self.xcrun);
            command.args([
                "simctl",
                "get_app_container",
                self.simulator.as_ref().expect("owned simulator").udid(),
                bundle,
                kind,
            ]);
            let evidence = self
                .directory
                .join(format!("discovery-{}", self.discovery_number));
            let result = self.simulator_mut().capture_owned(
                &mut command,
                // Installation and discovery share CoreSimulator work. Spend the caller's
                // remaining readiness budget instead of imposing a shorter per-query timeout.
                deadline,
                cancelled,
                65536,
                &evidence,
            )?;
            if let Some(error) = &result.cleanup_failure {
                self.simulator_mut().retain_failure(error.clone());
            }
            if let Some(error) = result.cleanup_failure.or(result.evidence_failure) {
                return Err(io::Error::other(error));
            }
            if result.failure.is_none() {
                let output = std::str::from_utf8(&result.stdout)
                    .map_err(io::Error::other)?
                    .trim();
                let path = PathBuf::from(output);
                if !path.is_absolute() || output.lines().count() != 1 {
                    return Err(io::Error::other("invalid installed container path"));
                }
                let path = self.simulator.as_ref().expect("owned simulator").canonical_container(&path)?;
                self.ensure_running()?;
                if kind == "data" && bundle == self.app.bundle {
                    app_data::verify_container(&self.directory, &path)?;
                }
                if kind == "data" { self.containers.insert(bundle.into(), path.clone()); }
                return Ok(path);
            }
            // A normal nonzero discovery exit can mean the fresh installation is not present yet.
            // A timeout/signal/unknown child outcome is never treated as an ordinary missing app.
            if !result.exit_code.is_some_and(|code| code != 0)
                || result.failure.as_deref() != Some("bridge exited unsuccessfully")
            {
                return Err(io::Error::other(
                    result
                        .failure
                        .unwrap_or_else(|| "container discovery failed".into()),
                ));
            }
            std::thread::sleep(
                Duration::from_millis(100).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    pub(crate) fn apply_initial_data(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        app_data::apply(self, deadline, cancelled)
    }
    pub fn wait_ready(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<UiReady> {
        let bundle = self.app.runner_bundle.clone();
        let container = self.app_container(&bundle, deadline, cancelled)?;
        let mut descriptor = container.join(&self.app.ui_descriptor);
        let mut discovery_checked = Instant::now();
        loop {
            self.ensure_running()?;
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                return Err(io::Error::other("XCTest ready deadline or cancellation"));
            }
            match std::fs::read(&descriptor) {
                Ok(bytes) => {
                    let ready: ReadyDescriptor = serde_json::from_slice(&bytes)?;
                    let identity = frontcap_core::native::ios_ui::UiIdentity {
                        simulator: self.simulator.as_ref().expect("owned simulator").udid().into(),
                        generation: ready.generation,
                        pid: ready.pid,
                    };
                    identity.validate().map_err(io::Error::other)?;
                    if ready.status != "ready" || ready.last_id != 0 || ready.port == 0 {
                        return Err(io::Error::other(
                            "XCTest endpoint is already used or invalid",
                        ));
                    }
                    let process = ProcessIdentity::of(identity.pid)?
                        .ok_or_else(|| io::Error::other("XCTest endpoint process absent"))?;
                    if process.has_exited()? {
                        return Err(io::Error::other("XCTest endpoint process exited"));
                    }
                    self.discovery_number += 1;
                    // Linux comm is a task name, not an executable path. Keep the installed
                    // path/Simulator checks below; never substitute caller-controlled argv.
                    #[cfg(target_os = "linux")]
                    let mut command = {
                        let mut command = Command::new("/usr/bin/readlink");
                        command.arg(format!("/proc/{}/exe", identity.pid));
                        command
                    };
                    #[cfg(not(target_os = "linux"))]
                    let mut command = {
                        let mut command = Command::new("/bin/ps");
                        command.args(["-p", &identity.pid.to_string(), "-o", "comm="]);
                        command
                    };
                    let evidence = self.directory.join(format!("runner-identity-{}", self.discovery_number));
                    let result = self.simulator_mut().capture_owned(
                        &mut command,
                        deadline,
                        cancelled,
                        65536,
                        &evidence,
                    )?;
                    if let Some(error) = &result.cleanup_failure {
                        self.simulator_mut().retain_failure(error.clone());
                    }
                    if let Some(error) = result
                        .failure
                        .or(result.cleanup_failure)
                        .or(result.evidence_failure)
                    {
                        return Err(io::Error::other(error));
                    }
                    let executable = PathBuf::from(
                        std::str::from_utf8(&result.stdout)
                            .map_err(io::Error::other)?
                            .trim(),
                    );
                    if executable.file_name().and_then(|s| s.to_str())
                        != Some(self.runner_executable.as_str())
                        || !executable.parent().is_some_and(|parent| self.simulator.as_ref()
                            .expect("owned simulator").canonical_container(parent).is_ok())
                        || process.has_exited()?
                    {
                        return Err(io::Error::other(
                            "XCTest PID is not the owned installed runner",
                        ));
                    }
                    let ready = UiReady {
                        app_restart: ready.app_restart, app_entry: ready.app_entry, app_lifecycle: ready.app_lifecycle,
                        system_alerts: ready.system_alerts, system_only: ready.system_only, host_app_state: ready.host_app_state,
                        port: ready.port,
                        identity,
                        descriptor,
                        process,
                    };
                    self.ready_process = Some(ready.process.clone());
                    save_atomic(&self.directory.join("ui-ready.json"), &ready)?;
                    return Ok(ready);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    // test-without-building can replace the previously installed runner container.
                    // A missing descriptor must not pin readiness to the pre-install path.
                    if discovery_checked.elapsed() >= Duration::from_millis(250) {
                        let container = self.app_container(&bundle, deadline, cancelled)?;
                        descriptor = container.join(&self.app.ui_descriptor);
                        discovery_checked = Instant::now();
                    }
                },
                Err(error) => return Err(error),
            }
            std::thread::sleep(
                Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
    /// A separately supervised bridge whose retirement is uncertain must keep the host fenced.
    pub fn retain_cleanup_failure(&mut self, failure: String) {
        self.simulator_mut().retain_failure(failure);
    }
    pub(crate) fn simulator_mut(&mut self) -> &mut Simulator {
        self.simulator.as_mut().expect("owned simulator")
    }
    pub fn simulator_id(&self) -> &str {
        self.simulator.as_ref().expect("owned simulator").udid()
    }
    /// Retire only this runner, returning the same owned simulator and its installed data.
    /// No PID/lease adoption: failure consumes this owner and performs normal full retirement.
    pub fn release_simulator(mut self) -> Result<Simulator, AcquisitionError> {
        let result = self.retire_runner_for_transfer();
        if let Err(error) = result {
            let _ = save_atomic(&self.directory.join("transfer-failure.json"), &error.to_string());
            return Err(AcquisitionError { cause: error.to_string(), retirement: Some(self.stop()) });
        }
        Ok(self.simulator.take().expect("owned simulator"))
    }
    fn retire_runner_for_transfer(&mut self) -> io::Result<()> {
        if self.retired.is_some() { return Err(io::Error::other("XCTest already retired")); }
        let process = self.ready_process.clone().ok_or_else(|| io::Error::other("runner transfer requires verified readiness"))?;
        save_atomic(&self.directory.join("transfer-intent.json"), &process)?;
        let mut exit_code = None;
        let mut signal_error = None;
        self.runner.as_mut().ok_or_else(|| io::Error::other("missing XCTest child"))?
            .retire(&mut exit_code, &mut signal_error)?;
        self.bundle_writers_retired = true;
        self.runner.take();
        save_atomic(&self.directory.join("runner-retirement.json"), &(exit_code, &signal_error))?;
        if !process.has_exited()? {
            // CoreSimulator can supervise the native runner outside xcodebuild's process group.
            // Stop only the configured runner bundle on the still-owned simulator, once.
            let deadline = Instant::now() + Duration::from_secs(5);
            let simulator = self.simulator_id().to_owned();
            let bundle = self.app.runner_bundle.clone();
            save_atomic(&self.directory.join("native-runner-termination-intent.json"), &process)?;
            let before = self.simulator_mut().command_cursor().1;
            let mut exit_race = None;
            if let Err(error) = self.simulator_mut().command_with_environment(&["terminate", &simulator, &bundle],
                &std::collections::BTreeMap::new(), deadline, &AtomicBool::new(false)) {
                // The runner may exit after the liveness check but before simctl's termination.
                // Only its recorded ESRCH exit plus absence of this exact process resolves that race.
                let (root, number) = self.simulator_mut().command_cursor();
                let record = root.join(format!("command-{number}/exchange.json"));
                let reply = if number == before + 1 {
                    crate::process::load_exchange::<crate::bridge::BridgeReply>(&record)?
                } else { None };
                if !reply.is_some_and(|reply| reply.exit_code == Some(libc::ESRCH)
                    && reply.failure.as_deref() == Some("bridge exited unsuccessfully")
                    && reply.cleanup_failure.is_none() && reply.evidence_failure.is_none()) {
                    return Err(error);
                }
                // A diagnostic signal_error may precede successful, independently verified child
                // cleanup (Darwin's zombie-group EPERM). cleanup_failure remains authoritative.
                exit_race = Some(record);
            }
            while !process.has_exited()? {
                if Instant::now() >= deadline { return Err(io::Error::other("native runner termination unconfirmed")); }
                std::thread::sleep(Duration::from_millis(10));
            }
            if let Some(record) = exit_race {
                save_atomic(&self.directory.join("native-runner-exit-race.json"),
                    &serde_json::json!({"process":process,"exchange":record,"exit_code":libc::ESRCH,"runner_absent":true}))?;
            }
            save_atomic(&self.directory.join("native-runner-terminated.json"), &process)?;
        }
        // The consumer container remains untouched. Retain only the platform runner's Documents.
        if let Some(container) = self.containers.get(&self.app.runner_bundle) {
            archive_documents(&container.join("Documents"), &self.directory.join("transfer-runner-documents"))?;
            // Preserve the old receipt above, then prevent the next runner from seeing stale ready.
            std::fs::remove_file(container.join(&self.app.ui_descriptor))?;
        }
        self.finish_bundle()?;
        save_atomic(&self.directory.join("simulator-transfer.json"),
            &serde_json::json!({"simulator":self.simulator_id(),"runner":process,"runner_absent":true,"signal_error":signal_error}))?;
        Ok(())
    }
    fn finish_bundle(&mut self) -> io::Result<()> {
        if let Some(volume) = &mut self.bundle_volume {
            volume.finish(self.bundle_writer.clone(), self.bundle_writers_retired,
                Instant::now() + Duration::from_secs(60))?;
        }
        Ok(())
    }
    pub fn stop(&mut self) -> Retirement {
        let _writes = crate::process::RetirementWrites::enter();
        if let Some(retirement) = &self.retired {
            return retirement.clone();
        }
        let mut signal_error = None;
        let mut exit_code = None;
        if let Some(child) = &mut self.runner {
            match child.retire(&mut exit_code, &mut signal_error) {
                Ok(()) => self.bundle_writers_retired = true,
                Err(error) => self.simulator_mut().retain_failure(format!("XCTest group retirement: {error}")),
            }
        }
        if let Err(error) = self.finish_bundle() {
            self.simulator_mut().retain_failure(format!("XCTest result bundle retirement: {error}"));
        }
        if self.runner.is_some() && let Err(error) = save_atomic(
            &self.directory.join("runner-retirement.json"),
            &(exit_code, &signal_error),
        ) {
            self.simulator_mut()
                .retain_failure(format!("XCTest retirement evidence: {error}"));
        }
        let snapshots = self
            .containers
            .iter()
            .map(|(bundle, path)| {
                let result = archive_documents(
                    &path.join("Documents"),
                    &self.directory.join(format!("documents-{bundle}")),
                );
                (bundle.clone(), result.err().map(|e| e.to_string()))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let _ = save_atomic(&self.directory.join("document-snapshots.json"), &snapshots);
        let path = self.directory.join("retirement.json");
        let result = self.simulator_mut().stop_recorded(Some(&path), signal_error);
        self.retired = Some(result.clone());
        result
    }
}
// Diagnostic snapshots are sequential, bounded by size/time checks, and never completion evidence.
fn archive_documents(source: &Path, destination: &Path) -> io::Result<()> {
    fn copy(
        source: &Path,
        destination: &Path,
        deadline: Instant,
        remaining: &mut u64,
    ) -> io::Result<()> {
        if Instant::now() >= deadline {
            return Err(io::Error::other("document snapshot deadline"));
        }
        let meta = std::fs::symlink_metadata(source)?;
        if meta.is_dir() {
            std::fs::create_dir(destination)?;
            for entry in std::fs::read_dir(source)? {
                let entry = entry?;
                copy(
                    &entry.path(),
                    &destination.join(entry.file_name()),
                    deadline,
                    remaining,
                )?;
            }
        } else if meta.is_file() {
            use std::io::{Read, Write};
            use std::os::unix::fs::OpenOptionsExt;
            let mut input = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(source)?;
            let mut output = File::create_new(destination)?;
            let mut buffer = [0; 65536];
            loop {
                if Instant::now() >= deadline {
                    return Err(io::Error::other("document snapshot deadline"));
                }
                let n = input.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                *remaining = remaining
                    .checked_sub(n as u64)
                    .ok_or_else(|| io::Error::other("document snapshot size limit"))?;
                output.write_all(&buffer[..n])?;
            }
        } else {
            return Err(io::Error::other(
                "document snapshot contains a link or special file",
            ));
        }
        Ok(())
    }
    copy(
        source,
        destination,
        Instant::now() + Duration::from_secs(5),
        &mut (64 * 1024 * 1024),
    )
}
impl Drop for Xctest {
    fn drop(&mut self) {
        if self.simulator.is_some() { self.stop(); }
    }
}

#[cfg(test)]
#[path = "xctest_tests.rs"]
pub(crate) mod tests;

#[path = "xctest_app_data.rs"]
mod app_data;

#[path = "xctest_configuration.rs"]
mod configuration;

#[path = "xctest_bundle_volume.rs"]
pub(crate) mod bundle_volume;
