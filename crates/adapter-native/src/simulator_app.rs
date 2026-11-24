//! Launch an externally prepared app on a freshly owned simulator.
//! Platform attachment environment is separate from ordinary consumer configuration.
use crate::{
    device::AcquisitionError,
    simulator::Simulator,
    xctest::{RunnerLaunch, UiReady, Xctest},
};
use frontcap_core::canonical::sha256_bytes_hex;
use frontcap_core::{
    native::{ios_ui::validate_launch_environment, ownership::Retirement},
    process::ProcessIdentity,
    write_budget::{Class, WriteBudget, save_inherited},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Instant,
};

fn save_evidence<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    save_inherited(Class::Ordinary, path, value)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentFile {
    pub path: PathBuf,
    pub sha256: String,
}
impl AttachmentFile {
    pub(crate) fn freeze(&self, destination: &Path) -> io::Result<()> {
        self.freeze_with_budget(destination, WriteBudget::from_environment()?.as_ref())
    }
    fn freeze_with_budget(&self, destination: &Path, budget: Option<&WriteBudget>) -> io::Result<()> {
        if !self.path.is_absolute() {
            return Err(io::Error::other("attachment path must be absolute"));
        }
        let bytes = std::fs::read(&self.path)?;
        if sha256_bytes_hex(&bytes) != self.sha256 {
            return Err(io::Error::other("attachment digest differs"));
        }
        if let Some(budget) = budget {
            budget.charge(Class::Ordinary, bytes.len() as u64)?;
        }
        use std::io::Write;
        let mut file = std::fs::File::create_new(destination)?;
        file.write_all(&bytes)?;
        file.sync_all()
    }
}
#[path = "ios_prepared_app.rs"]
pub mod prepared;
#[path = "simulator_app_entry.rs"]
pub mod entry;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub app: PathBuf,
    pub bundle: String,
    pub environment: BTreeMap<String, String>,
    pub library: AttachmentFile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<AttachmentFile>,
    pub mute_audio: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub page_registry: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prepared: Option<Box<prepared::Prepared>>,
    /// Completed app-data construction, applied only before the first launch on a fresh installation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_data: Option<PathBuf>,
}
impl Launch {
    pub fn attachment_sha256(&self) -> String {
        let original = (&self.library.sha256, self.script.as_ref().map(|s| &s.sha256), self.mute_audio);
        let value = if let Some(prepared) = &self.prepared {
            frontcap_core::canonical::canonical_of(&(original, "prepared-pages", &prepared.original_inventory, &prepared.inventory, &prepared.executable, &prepared.shared_libraries))
        } else if self.page_registry {
            frontcap_core::canonical::canonical_of(&(original, "pages"))
        } else { frontcap_core::canonical::canonical_of(&original) };
        frontcap_core::canonical::sha256_hex(&value)
    }
}
/// The simulator remains owned until explicit retirement or Drop, including after partial launch.
enum Owner {
    Simulator(Box<Simulator>),
    Xctest(Box<Xctest>),
}
pub struct App {
    owner: Option<Owner>,
    retired: Option<Retirement>,
    simulator_id: String,
    bundle: String,
    directory: PathBuf,
    process: ProcessIdentity,
    container: PathBuf,
    attachment_sha256: String,
    launch_config: Launch,
    entry: Option<entry::Pending>,
}
impl App {
    pub fn launch(
        mut simulator: Simulator,
        config: Launch,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Self, AcquisitionError> {
        if let Err(error) = std::fs::create_dir(directory) {
            return Err(AcquisitionError {
                cause: error.to_string(),
                retirement: Some(simulator.stop()),
            });
        }
        let directory = &directory.canonicalize()?;
        let result = Self::initialize(&mut simulator, &config, directory, deadline, cancelled);
        match result {
            Ok((process, container)) => Ok(Self {
                simulator_id: simulator.udid().to_owned(),
                owner: Some(Owner::Simulator(Box::new(simulator))),
                retired: None,
                bundle: config.bundle.clone(),
                directory: directory.canonicalize()?,
                process,
                container,
                launch_config: Launch {
                    initial_data: None,
                    library: AttachmentFile { path: directory.canonicalize()?.join("attachment.dylib"), sha256: config.library.sha256.clone() },
                    script: config.script.as_ref().map(|script| AttachmentFile { path: directory.join("attachment.js"), sha256: script.sha256.clone() }),
                    ..config.clone()
                },
                attachment_sha256: config.attachment_sha256(),
                entry: None,
            }),
            Err(error) => {
                let _ = save_evidence(&directory.join("failure.json"), &error.to_string());
                let retirement =
                    simulator.stop_recorded(Some(&directory.join("retirement.json")), None);
                Err(AcquisitionError {
                    cause: error.to_string(),
                    retirement: Some(retirement),
                })
            }
        }
    }
    fn initialize(
        simulator: &mut Simulator,
        config: &Launch,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<(ProcessIdentity, PathBuf)> {
        validate_launch_environment(&config.environment).map_err(io::Error::other)?;
        if (config.page_registry && config.script.is_none()) || config.environment.keys().any(|key| key.starts_with("FRONTCAP_")) {
            return Err(io::Error::other("invalid or overridden platform attachment configuration"));
        }
        if !config.app.is_absolute()
            || !config.app.is_dir()
            || config.bundle.is_empty()
            || !config
                .bundle
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b))
        {
            return Err(io::Error::other("invalid simulator app launch"));
        }
        config.validate_attachment()?;
        let directory = directory.canonicalize()?;
        save_evidence(&directory.join("launch.json"), config)?;
        let library = directory.join("attachment.dylib");
        let script = directory.join("attachment.js");
        config.library.freeze(&library)?;
        if let Some(input) = &config.script { input.freeze(&script)?; }
        let initial_data = if let Some(source) = &config.initial_data {
            if !source.is_absolute() { return Err(io::Error::other("initial app data must be absolute")); }
            let product = crate::products::BundleInventory::capture(&config.app)?;
            let product_sha256 = frontcap_core::canonical::sha256_hex(&frontcap_core::canonical::canonical_of(&product));
            let input = crate::app_data::Prepared::read(source, &config.bundle, &product_sha256)?;
            let frozen = directory.join("initial-data");
            let child = input.recipe.construct(&source.join("data"), &frozen,
                &simulator.udid().to_ascii_lowercase(), &config.bundle, &product_sha256)?;
            Some((child, frozen, product))
        } else { None };
        simulator.ensure_booted(deadline, cancelled)?;
        let id = simulator.udid().to_owned();
        if initial_data.is_some() {
            let apps = simulator.command_with_environment(&["listapps", &id], &BTreeMap::new(), deadline, cancelled)?;
            let apps: BTreeMap<String, serde::de::IgnoredAny> = plist::from_bytes(&apps).map_err(io::Error::other)?;
            if apps.contains_key(&config.bundle) {
                return Err(io::Error::other("initial app data requires a fresh installation"));
            }
        }
        simulator.command_with_environment(
            &[
                "install",
                &id,
                config
                    .app
                    .to_str()
                    .ok_or_else(|| io::Error::other("non-UTF8 app path"))?,
            ],
            &BTreeMap::new(),
            deadline,
            cancelled,
        )?;
        prepared::verify_installed(simulator, config, &directory, None, deadline, cancelled)?;
        let installed_data = if let Some((child, frozen, product)) = initial_data {
            product.verify(&config.app)?;
            if config.prepared.is_none() {
                let output = simulator.command_with_environment(&["get_app_container", &id, &config.bundle, "app"],
                    &BTreeMap::new(), deadline, cancelled)?;
                let installed = PathBuf::from(std::str::from_utf8(&output).map_err(io::Error::other)?.trim());
                if !installed.is_absolute() { return Err(io::Error::other("installed product must be absolute")); }
                let observed = crate::products::BundleInventory::capture(&installed)?;
                let libraries = product.files.keys().filter_map(|name| match prepared::shared_library(&config.app.join(name)) {
                    Ok(true) => Some(Ok(name.clone())), Ok(false) => None, Err(error) => Some(Err(error)),
                }).collect::<io::Result<std::collections::BTreeSet<_>>>()?;
                prepared::validate_installed_inventory(&product, &observed, &libraries)?;
                save_evidence(&directory.join("initial-data-installed-product.json"), &observed)?;
            }
            save_evidence(&directory.join("initial-data-product.json"), &product)?;
            let output = simulator.command_with_environment(&["get_app_container", &id, &config.bundle, "data"],
                &BTreeMap::new(), deadline, cancelled)?;
            let container = PathBuf::from(std::str::from_utf8(&output).map_err(io::Error::other)?.trim());
            if !container.is_absolute() { return Err(io::Error::other("initial data container must be absolute")); }
            crate::app_data::apply_new_files(&child, &frozen.join("data"), &container)?;
            save_evidence(&directory.join("initial-data-applied.json"), &(&child, &container))?;
            Some(container.canonicalize()?)
        } else { None };
        let result = Self::launch_process(simulator, config, &directory, &library, &script, deadline, cancelled)?;
        if installed_data.is_some_and(|container| result.1.canonicalize().ok().as_ref() != Some(&container)) {
            return Err(io::Error::other("initial data container changed at launch"));
        }
        Ok(result)
    }
    fn launch_process(
        simulator: &mut Simulator, config: &Launch, directory: &Path, library: &Path, script: &Path,
        deadline: Instant, cancelled: &AtomicBool,
    ) -> io::Result<(ProcessIdentity, PathBuf)> {
        let id = simulator.udid().to_owned();
        let mut environment: BTreeMap<_, _> = config
            .environment
            .iter()
            .map(|(k, v)| (format!("SIMCTL_CHILD_{k}"), v.clone()))
            .collect();
        if config.prepared.is_none() {
        environment.insert(
            "SIMCTL_CHILD_DYLD_INSERT_LIBRARIES".into(),
            library
                .to_str()
                .ok_or_else(|| io::Error::other("non-UTF8 attachment path"))?
                .into(),
        );
        if config.script.is_some() {
            environment.insert("SIMCTL_CHILD_FRONTCAP_WEBVIEW_SCRIPT".into(), script.to_str().unwrap().into());
            environment.insert("SIMCTL_CHILD_FRONTCAP_WEBVIEW_CONTROL".into(), if config.page_registry { "pages" } else { "1" }.into());
        }
        environment.insert(
            "SIMCTL_CHILD_FRONTCAP_MUTE_AUDIO".into(),
            if config.mute_audio { "1" } else { "0" }.into(),
        );
        }
        let output = simulator.command_with_environment(
            &["launch", &id, &config.bundle],
            &environment,
            deadline,
            cancelled,
        )?;
        let text = std::str::from_utf8(&output)
            .map_err(io::Error::other)?
            .trim();
        let prefix = format!("{}: ", config.bundle);
        let pid: u32 = text
            .strip_prefix(&prefix)
            .ok_or_else(|| io::Error::other("app launch identity missing"))?
            .parse()
            .map_err(io::Error::other)?;
        if pid == 0 || pid > i32::MAX as u32 {
            return Err(io::Error::other("invalid app PID"));
        }
        let process =
            ProcessIdentity::of(pid)?.ok_or_else(|| io::Error::other("launched app absent"))?;
        save_evidence(&directory.join("process.json"), &process)?;
        let output = simulator.command_with_environment(
            &["get_app_container", &id, &config.bundle, "data"],
            &BTreeMap::new(),
            deadline,
            cancelled,
        )?;
        let container = PathBuf::from(
            std::str::from_utf8(&output)
                .map_err(io::Error::other)?
                .trim(),
        );
        if !container.is_absolute() || !container.is_dir() || process.has_exited()? {
            return Err(io::Error::other("app container or process unavailable"));
        }
        save_evidence(&directory.join("container.json"), &container)?;
        Ok((process, container))
    }
    /// Replace only the app process. The caller must replace its clock/contact/lifecycle epoch too.
    /// Failure retires the owned device; this method never retries ambiguous native effects.
    pub fn restart(&mut self, directory: &Path, deadline: Instant, cancelled: &AtomicBool) -> Result<(), AcquisitionError> {
        if let Err(error) = std::fs::create_dir(directory) {
            return Err(AcquisitionError { cause: error.to_string(), retirement: Some(self.stop()) });
        }
        let result = self.restart_inner(directory, deadline, cancelled);
        if let Err(error) = result {
            let _ = save_evidence(&directory.join("failure.json"), &error.to_string());
            return Err(AcquisitionError { cause: error.to_string(), retirement: Some(self.stop()) });
        }
        Ok(())
    }
    fn restart_inner(&mut self, directory: &Path, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        let directory = directory.canonicalize()?;
        let config = self.prepare_replacement(&directory, true, deadline, cancelled)?;
        let previous = self.process.clone();
        let simulator = self.simulator_id.clone();
        let library = directory.join("attachment.dylib");
        let script = directory.join("attachment.js");
        save_evidence(&directory.join("launch-intent.json"), &config)?;
        let (process, container) = Self::launch_process(self.simulator_mut()?, &config, &directory,
            &library, &script, deadline, cancelled)?;
        if process == previous || container.canonicalize()? != self.container.canonicalize()? {
            return Err(io::Error::other("restarted app process or data container differs from replacement requirements"));
        }
        save_evidence(&directory.join("restarted.json"), &serde_json::json!({
            "previous_process":previous,"process":process,"container":container,"simulator":simulator
        }))?;
        self.process = process;
        self.container = container;
        self.directory = directory;
        self.launch_config = config;
        Ok(())
    }
    /// Preserve immutable prior evidence and retire only platform channels after proven app exit.
    /// Cold URL delivery keeps the independent system runner alive to handle OS confirmation.
    fn prepare_replacement(&mut self, directory: &Path, detach_runner: bool,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<Launch> {
        self.ensure_running(deadline, cancelled)?;
        let mut config = self.launch_config.clone();
        let previous_evidence = self.directory.clone();
        prepared::verify_installed(self.simulator_mut()?, &config, directory, Some(&previous_evidence), deadline, cancelled)?;
        let library = directory.join("attachment.dylib");
        let script = directory.join("attachment.js");
        config.library.freeze(&library)?;
        if let Some(input) = &config.script { input.freeze(&script)?; }
        config.library.path = library.clone();
        if let Some(input) = &mut config.script { input.path = script.clone(); }
        save_evidence(&directory.join("launch.json"), &config)?;
        let previous = self.process.clone();
        save_evidence(&directory.join("restart-intent.json"), &serde_json::json!({
            "previous_process":previous,"previous_evidence":self.directory,
            "simulator":self.simulator_id,"container":self.container,"bundle":self.bundle,
            "attachment_sha256":self.attachment_sha256
        }))?;
        if detach_runner { self.detach_xctest().map_err(|e| io::Error::other(e.cause))?; }
        self.ensure_running(deadline, cancelled)?;
        let simulator = self.simulator_id.clone();
        let bundle = self.bundle.clone();
        save_evidence(&directory.join("terminate-intent.json"), &previous)?;
        self.simulator_mut()?.command_with_environment(&["terminate", &simulator, &bundle],
            &BTreeMap::new(), deadline, cancelled)?;
        while !previous.has_exited()? {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) || Instant::now() >= deadline {
                return Err(io::Error::other("app termination deadline or cancellation"));
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        save_evidence(&directory.join("terminated.json"), &previous)?;
        // These are platform transport files, not consumer state. A new HID producer must never
        // encounter prior-generation commands. Move the complete channel only after its owner exits.
        let contacts = self.container.join("Documents/frontcap-hid");
        if contacts.try_exists()? {
            if std::fs::symlink_metadata(&contacts)?.file_type().is_symlink() {
                return Err(io::Error::other("linked previous contact transport"));
            }
            let endpoint: frontcap_core::native::contacts::Endpoint = frontcap_core::state::load(&contacts.join("endpoint.json"))?
                .ok_or_else(|| io::Error::other("previous contact transport has no owner"))?;
            endpoint.validate(previous.pid).map_err(io::Error::other)?;
            std::fs::rename(&contacts, directory.join("previous-native-contacts"))?;
        }
        if config.page_registry {
            // These per-process platform channels cannot survive a process replacement. Retain
            // them after proven exit, before launch; never move consumer result/storage paths.
            let descriptor = self.container.join("Documents/webview-pages.json");
            let channels = self.container.join("Documents/webview-pages");
            if descriptor.try_exists()? {
                if !std::fs::symlink_metadata(&descriptor)?.file_type().is_file() {
                    return Err(io::Error::other("invalid previous page registry"));
                }
                let inventory: frontcap_core::native::ios_host::Inventory =
                    frontcap_core::state::load(&descriptor)?.ok_or_else(|| io::Error::other("previous page registry missing"))?;
                let owner = frontcap_core::native::ios_host::Owner {
                    simulator: simulator.clone(), process: previous.clone(),
                    attachment_sha256: self.attachment_sha256.clone(),
                };
                inventory.validate(&owner).map_err(io::Error::other)?;
                std::fs::rename(&descriptor, directory.join("previous-page-registry.json"))?;
            }
            if channels.try_exists()? {
                if !std::fs::symlink_metadata(&channels)?.file_type().is_dir() {
                    return Err(io::Error::other("invalid previous page channels"));
                }
                std::fs::rename(&channels, directory.join("previous-page-channels"))?;
            }
        }
        Ok(config)
    }
    pub(crate) fn bundle(&self) -> &str { &self.bundle }
    pub(crate) fn simulator_mut(&mut self) -> io::Result<&mut Simulator> {
        match self.owner.as_mut() {
            Some(Owner::Simulator(simulator)) => Ok(simulator),
            Some(Owner::Xctest(runner)) => Ok(runner.simulator_mut()),
            None => Err(io::Error::other("owned app has no simulator")),
        }
    }
    /// Attach a separately built platform runner to this app's existing simulator.
    /// Readiness alone does not activate the consumer or prove page lifecycle delivery.
    pub fn attach_xctest(
        &mut self,
        config: RunnerLaunch,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<UiReady, AcquisitionError> {
        let result = self.attach_xctest_inner(config, deadline, cancelled);
        result.map_err(|error| AcquisitionError {
            cause: error.to_string(),
            retirement: Some(self.stop()),
        })
    }
    fn attach_xctest_inner(
        &mut self,
        config: RunnerLaunch,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<UiReady> {
        if config.app.bundle != self.bundle {
            return Err(io::Error::other("XCTest consumer differs from owned app"));
        }
        self.ensure_running(deadline, cancelled)?;
        if !matches!(self.owner, Some(Owner::Simulator(_))) {
            return Err(io::Error::other("XCTest is already attached"));
        }
        let Some(Owner::Simulator(simulator)) = self.owner.take() else {
            unreachable!()
        };
        match Xctest::on_simulator(
            config,
            *simulator,
            &self.directory.join("xctest"),
            deadline,
            cancelled,
        ) {
            Ok(runner) => self.owner = Some(Owner::Xctest(Box::new(runner))),
            Err(error) => {
                self.retired = error.retirement;
                return Err(io::Error::other(error.cause));
            }
        }
        // A runner must never turn disappearance/replacement of the original app into adoption.
        if self.process.has_exited()? {
            return Err(io::Error::other("consumer exited during XCTest attachment"));
        }
        let Some(Owner::Xctest(runner)) = &mut self.owner else {
            unreachable!()
        };
        let ready = runner.wait_ready(deadline, cancelled)?;
        if self.process.has_exited()? {
            return Err(io::Error::other("consumer exited before XCTest readiness"));
        }
        save_evidence(&self.directory.join("xctest-ready.json"), &ready)?;
        Ok(ready)
    }
    pub(crate) fn verify_system_runner(&mut self, ready: &UiReady, manifest: &crate::products::ProductsManifest,
        deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        if !ready.system_only || !ready.system_alerts || ready.process.has_exited()? {
            return Err(io::Error::other("system runner is unavailable or not system-only"));
        }
        let Some(Owner::Xctest(runner)) = &mut self.owner else { return Err(io::Error::other("missing system runner owner")); };
        let installed = runner.installed_container("dev.frontcap.system-ui.tests.xctrunner", "app", deadline, cancelled)?;
        let name = "PlugIns/SystemUITests.xctest/SystemUITests";
        let expected = manifest.files.get(&format!("Debug-iphonesimulator/SystemUITests-Runner.app/{name}"))
            .ok_or_else(|| io::Error::other("missing frozen system executable"))?;
        let bytes = std::fs::read(installed.join(name))?;
        if bytes.len() as u64 != expected.bytes || sha256_bytes_hex(&bytes) != expected.sha256 {
            return Err(io::Error::other("installed system executable differs"));
        }
        Ok(())
    }
    pub(crate) fn retain_cleanup_failure(&mut self, failure: String) {
        if let Ok(simulator) = self.simulator_mut() { simulator.retain_failure(failure); }
    }
    /// Remove only the platform runner; preserve the installed app, process and data container.
    pub fn detach_xctest(&mut self) -> Result<(), AcquisitionError> {
        let Some(owner) = self.owner.take() else {
            return Err(AcquisitionError { cause: "owned app has no simulator".into(), retirement: self.retired.clone() });
        };
        match owner {
            Owner::Simulator(simulator) => { self.owner = Some(Owner::Simulator(simulator)); Ok(()) }
            Owner::Xctest(runner) => match runner.release_simulator() {
                Ok(simulator) => { self.owner = Some(Owner::Simulator(Box::new(simulator))); Ok(()) }
                Err(error) => { self.retired = error.retirement.clone(); Err(error) }
            }
        }
    }
    pub(crate) fn supports_entry(&self) -> bool { self.launch_config.prepared.is_some() }
    pub fn page_registry(&self) -> bool { self.launch_config.page_registry }
    pub fn attachment_sha256(&self) -> &str {
        &self.attachment_sha256
    }
    pub fn capture_png(
        &mut self,
        path: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> io::Result<()> {
        self.ensure_running(deadline, cancelled)?;
        if !path.is_absolute() || path.exists() {
            return Err(io::Error::other("capture requires a fresh absolute path"));
        }
        let id = self.simulator_id.clone();
        self.simulator_mut()?.command_with_environment(
            &[
                "io",
                &id,
                "screenshot",
                "--type=png",
                path.to_str()
                    .ok_or_else(|| io::Error::other("non-UTF8 capture path"))?,
            ],
            &BTreeMap::new(),
            deadline,
            cancelled,
        )?;
        Ok(())
    }
    pub(crate) fn retirement(&self) -> Option<&Retirement> { self.retired.as_ref() }
    pub fn process(&self) -> &ProcessIdentity {
        &self.process
    }
    pub fn container(&self) -> &Path {
        &self.container
    }
    pub fn simulator_id(&self) -> &str {
        &self.simulator_id
    }
    pub fn ensure_running(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        if self.entry.is_some() { return Err(io::Error::other("app URL replacement is pending")); }
        self.ensure_platform_running(deadline, cancelled)?;
        if self.process.has_exited()? { return Err(io::Error::other("owned app exited")); }
        Ok(())
    }
    /// A live simulator/runner is not proof that its consumer has started.
    pub(crate) fn ensure_platform_running(&mut self, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        if self.retired.is_some() {
            return Err(io::Error::other("owned app retired"));
        }
        if let Some(Owner::Xctest(runner)) = &mut self.owner {
            runner.ensure_running()?;
        }
        self.simulator_mut()?.ensure_booted(deadline, cancelled)?;
        Ok(())
    }
    pub fn stop(&mut self) -> Retirement {
        if let Some(retirement) = &self.retired {
            return retirement.clone();
        }
        let retirement = match self.owner.as_mut() {
            Some(Owner::Simulator(simulator)) => {
                simulator.stop_recorded(Some(&self.directory.join("retirement.json")), None)
            }
            Some(Owner::Xctest(runner)) => runner.stop(),
            None => Retirement {
                passed: false,
                errors: vec!["app ownership transfer has no retirement receipt".into()],
                signal_error: None,
            },
        };
        self.retired = Some(retirement.clone());
        retirement
    }
}
impl Drop for App {
    fn drop(&mut self) {
        self.stop();
    }
}
#[cfg(test)]
#[path = "simulator_app_tests.rs"]
pub(crate) mod tests;
