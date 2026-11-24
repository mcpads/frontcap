//! Constant-cost admission check. A free-space sample is not a reservation or a run quota.
use std::{ffi::CString, io, mem::MaybeUninit, os::unix::ffi::OsStrExt, path::Path};
use std::{fs, os::unix::fs::MetadataExt, path::PathBuf};
use serde::{Deserialize, Serialize};

pub const DEFAULT_MIN_FREE_BYTES: u64 = 1024 * 1024 * 1024;

/// Live observation identities, never authority to delete or adopt a saved target.
#[derive(Serialize)]
pub(super) struct Roots {
    scope: &'static str,
    cleanup_authority: bool,
    enforcement: &'static str,
    whole_run_quota: bool,
    known_unregistered_writers: Vec<&'static str>,
    roots: Vec<Root>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_generated_volume: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_simulator_data_volume: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_flutter_data_volume: Option<PathBuf>,
    #[serde(skip)]
    external_simulator_data_volume: bool,
    #[serde(skip)]
    external_flutter_data_volume: bool,
    #[serde(skip)]
    flutter_android_volume: bool,
    #[serde(skip)]
    control_reserve: std::sync::Mutex<Option<fs::File>>,
}
#[derive(Deserialize, Serialize)]
struct Root {
    role: String,
    path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source_path: Option<PathBuf>,
    device: u64,
    inode: u64,
}
impl Roots {
    pub fn runtime(directory: &Path) -> io::Result<Self> {
        let mut roots = Self { scope: "registered_native_storage_roots", cleanup_authority: false,
            enforcement: "filesystem_free_space_floor", whole_run_quota: false,
            known_unregistered_writers: vec![], roots: vec![], pending_generated_volume: None, pending_simulator_data_volume: None, pending_flutter_data_volume: None, external_simulator_data_volume: false, external_flutter_data_volume: false, flutter_android_volume: false,
            control_reserve: std::sync::Mutex::new(None) };
        roots.add("runtime", directory)?;
        Ok(roots)
    }
    pub fn for_launch(directory: &Path, target: &super::target::Launch) -> io::Result<Self> {
        let mut roots = Self::runtime(directory)?;
        // Target images may be on another filesystem. Host services keep writing outside
        // them; observing these anchors grants no cleanup authority over shared state.
        #[cfg(target_os = "macos")]
        roots.add_host_storage()?;
        match target {
            super::target::Launch::Flutter(config) => {
                let config = &config.worker.config;
                config.validate_data_volume()?;
                // Installed products need no source/SDK tree. Do not make stale optional build
                // paths a dependency of the installed route.
                if config.installed_app.is_none() {
                    let project = config.project.as_deref()
                        .ok_or_else(|| io::Error::other("Flutter source storage requires a project directory"))?;
                    roots.add("flutter_project_build", project)?;
                    roots.known_unregistered_writers.push("flutter_sdk_and_dependency_caches");
                }
                roots.known_unregistered_writers.push(match config.platform {
                    frontcap_core::native::flutter::Platform::Android => "android_sdk_and_avd_state",
                    frontcap_core::native::flutter::Platform::Ios => "simulator_data",
                });
                if config.data_volume.is_some() || config.android_resources.is_some() {
                    roots.pending_flutter_data_volume = Some(directory.join("native-session").join(config.data_volume_mount()));
                    if let Some(android) = &config.android_resources {
                        roots.flutter_android_volume = true;
                        if let Some(root) = &android.scratch_volume_root {
                            roots.add("flutter_android_volume_storage", root.parent().unwrap())?;
                            roots.external_flutter_data_volume = true;
                        }
                        roots.known_unregistered_writers.retain(|writer| *writer != "android_sdk_and_avd_state");
                        roots.known_unregistered_writers.push("android_shared_services_and_logs");
                    } else {
                        roots.known_unregistered_writers.retain(|writer| *writer != "simulator_data");
                        roots.known_unregistered_writers.push("simulator_shared_services_and_logs");
                    }
                }
            }
            super::target::Launch::Ios(_) | super::target::Launch::IosHost(_)
                | super::target::Launch::Webview(_) => roots.known_unregistered_writers.push("simulator_data"),
            super::target::Launch::Android(_) | super::target::Launch::AndroidHost(_)
                | super::target::Launch::AndroidWebview(_) => roots.known_unregistered_writers.push("android_sdk_state"),
        }
        let simulator = match target {
            super::target::Launch::Ios(config) => {
                config.validate_resources()?;
                Some(&config.xctest.simulator)
            }
            super::target::Launch::IosHost(config) => Some(&config.simulator),
            super::target::Launch::Webview(config) => Some(&config.simulator),
            _ => None,
        };
        if let Some(simulator) = simulator { simulator.validate_data_volume()?; }
        if let Some(simulator) = simulator.filter(|config| config.data_volume.is_some()) {
            if let Some(root) = &simulator.data_volume_root {
                roots.add("ios_simulator_volume_storage", root.parent().unwrap())?;
                roots.external_simulator_data_volume = true;
            }
            roots.pending_simulator_data_volume = Some(simulator.data_volume_directory(
                &directory.join("native-session/target/simulator")).join("worker/volume/mount"));
            roots.known_unregistered_writers.retain(|writer| *writer != "simulator_data");
            roots.known_unregistered_writers.push("simulator_shared_services_and_logs");
        }
        let android = match target {
            super::target::Launch::Android(config) => Some(config),
            super::target::Launch::AndroidHost(config) | super::target::Launch::AndroidWebview(config) => Some(&config.device),
            _ => None,
        };
        if let Some(config) = android {
            if let Some(inventory) = &config.avd_inventory {
                inventory.validate()?;
                if config.avd_home.is_none() || matches!(target, super::target::Launch::Android(_)) {
                    return Err(io::Error::other("owned AVD copy requires Android host/WebView and an explicit source home"));
                }
            }
            if let Some(volume) = &config.scratch_volume {
                volume.validate()?;
                if config.temporary_storage_root.is_some() || matches!(target, super::target::Launch::Android(_)) {
                    return Err(io::Error::other("automatic scratch volume requires Android host/WebView and no external scratch root"));
                }
                roots.pending_generated_volume = Some(directory.join("native-session/target-resources/scratch-volume/worker/volume/mount"));
            }
            if let Some(root) = &config.temporary_storage_root { roots.add("android_temporary_storage", root)?; }
            if config.avd_inventory.is_none() && let Some(root) = &config.avd_home { roots.add("android_avd_home", root)?; }
        }
        roots.reserve_control(directory)?;
        Ok(roots)
    }
    fn reserve_control(&self, directory: &Path) -> io::Result<()> {
        use std::io::Read;
        let mut slot = self.control_reserve.lock().unwrap();
        if slot.is_some() { return Err(io::Error::other("native control reserve already allocated")); }
        let mut file = tempfile::tempfile_in(directory)?;
        let bytes = 8 * 1024 * 1024;
        if io::copy(&mut fs::File::open("/dev/urandom")?.take(bytes), &mut file)? != bytes {
            return Err(io::Error::other("native control reserve allocation incomplete"));
        }
        file.sync_all()?;
        if file.metadata()?.blocks() * 512 < bytes {
            return Err(io::Error::other("native control reserve is not physically allocated"));
        }
        *slot = Some(file);
        Ok(())
    }
    pub fn release_control(&self) {
        // Closing only our live anonymous descriptor grants no authority over any target.
        self.control_reserve.lock().unwrap().take();
    }
    #[cfg(target_os = "macos")]
    fn add_host_storage(&mut self) -> io::Result<()> {
        let home = std::env::var_os("HOME").map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| io::Error::other("native host storage requires an absolute HOME"))?;
        for (role, path) in [
            ("host_user_library", home.join("Library")),
            ("host_shared_library", PathBuf::from("/Library")),
            ("host_system_state", PathBuf::from("/private/var")),
            ("host_temporary_storage", std::env::temp_dir()),
        ] {
            self.add_resolved(role, &path)?;
        }
        Ok(())
    }
    pub fn route_session(&mut self, parent: &Path, launch: &frontcap_core::ids::LaunchId) -> io::Result<()> {
        self.add("session_storage", parent)?;
        if let Some(path) = &self.pending_flutter_data_volume && !self.external_flutter_data_volume {
            let relative = path.strip_prefix(self.roots[0].path.join("native-session"))
                .map_err(io::Error::other)?;
            self.pending_flutter_data_volume = Some(parent.join(launch.as_str()).join(relative));
        }
        if self.pending_generated_volume.is_some() {
            self.pending_generated_volume = Some(parent.join(launch.as_str())
                .join("target-resources/scratch-volume/worker/volume/mount"));
        }
        if self.pending_simulator_data_volume.is_some() && !self.external_simulator_data_volume {
            self.pending_simulator_data_volume = Some(parent.join(launch.as_str())
                .join("target/simulator/data-volume/worker/volume/mount"));
        }
        Ok(())
    }
    pub fn register_session(&mut self, path: &Path) -> io::Result<()> {
        self.add("session_evidence", path)
    }
    pub fn activate_generated(&mut self) -> io::Result<()> {
        if let Some(path) = self.pending_flutter_data_volume.clone() {
            if fs::symlink_metadata(&path)?.dev() == self.roots[0].device {
                return Err(io::Error::other("Flutter data volume did not establish a separate filesystem"));
            }
            let name = if self.flutter_android_volume {
                "flutter_android_generated_scratch_volume"
            } else { "flutter_ios_generated_data_volume" };
            self.add(name, &path)?;
            self.pending_flutter_data_volume = None;
        }
        if let Some(path) = self.pending_generated_volume.clone() {
            if fs::symlink_metadata(&path)?.dev() == self.roots[0].device {
                return Err(io::Error::other("generated scratch volume did not establish a separate filesystem"));
            }
            self.add("android_generated_scratch_volume", &path)?;
            self.pending_generated_volume = None;
        }
        if let Some(path) = self.pending_simulator_data_volume.clone() {
            if fs::symlink_metadata(&path)?.dev() == self.roots[0].device {
                return Err(io::Error::other("generated Simulator data volume did not establish a separate filesystem"));
            }
            self.add("ios_generated_data_volume", &path)?;
            self.pending_simulator_data_volume = None;
        }
        Ok(())
    }
    fn add(&mut self, role: &'static str, path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if !path.is_absolute() || !metadata.is_dir() {
            return Err(io::Error::other(format!("{role} storage root must be an absolute directory, not a symlink")));
        }
        self.roots.push(Root { role: role.into(), path: path.to_owned(), source_path: None,
            device: metadata.dev(), inode: metadata.ino() });
        Ok(())
    }
    #[cfg(any(target_os = "macos", test))]
    fn add_resolved(&mut self, role: &'static str, path: &Path) -> io::Result<()> {
        if !path.is_absolute() {
            return Err(io::Error::other("host storage source must be absolute"));
        }
        let resolved = path.canonicalize()?;
        self.add(role, &resolved)?;
        self.roots.last_mut().unwrap().source_path = Some(path.to_owned());
        Ok(())
    }
    pub fn admit(&self, minimum: u64) -> io::Result<()> {
        for root in &self.roots {
            let check = || -> io::Result<()> {
                if let Some(source) = &root.source_path
                    && source.canonicalize()? != root.path {
                    return Err(io::Error::other("registered storage source resolution changed"));
                }
                let metadata = fs::symlink_metadata(&root.path)?;
                if !metadata.is_dir() || metadata.dev() != root.device || metadata.ino() != root.inode {
                    return Err(io::Error::other("registered storage root identity changed"));
                }
                admit(&root.path, minimum)
            };
            check().map_err(|error| io::Error::other(format!("{} ({}): {error}", root.role, root.path.display())))?;
        }
        Ok(())
    }
}

pub fn admit(directory: &Path, minimum: u64) -> io::Result<()> {
    let path = CString::new(directory.as_os_str().as_bytes())
        .map_err(|_| io::Error::other("storage path contains NUL"))?;
    let mut stats = MaybeUninit::<libc::statvfs>::uninit();
    // Both pointers are valid for the call; statvfs initializes stats only on success.
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stats = unsafe { stats.assume_init() };
    let available = u128::from(stats.f_bavail) * u128::from(stats.f_frsize);
    if available < u128::from(minimum) {
        return Err(io::Error::other(format!(
            "filesystem has {available} available bytes; requires {minimum}"
        )));
    }
    Ok(())
}

#[cfg(test)]
#[path = "storage_tests.rs"]
mod tests;


#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SessionLocation {
    launch_id: frontcap_core::ids::LaunchId,
    session_root: Option<PathBuf>,
}
#[derive(Deserialize)]
struct Registered { roots: Vec<Root> }

/// Resolve only an explicitly located Simulator image for metadata observation.
/// This is neither image-content identity nor permission to adopt or delete it.
pub fn declared_simulator_image(runtime: &Path) -> io::Result<Option<PathBuf>> {
    let spec = storage_spec(runtime)?;
    declared_image_for_target(runtime, &spec.target)
}

/// Observe the explicitly located Flutter Android backing image without adopting it.
pub fn declared_android_scratch_image(runtime: &Path) -> io::Result<Option<PathBuf>> {
    let spec = storage_spec(runtime)?;
    let super::target::Launch::Flutter(config) = &spec.target else { return Ok(None) };
    let config = &config.worker.config;
    let Some(directory) = config.android_resources.as_ref().and_then(|android| android.scratch_volume_root.as_ref())
        else { return Ok(None) };
    config.validate_data_volume()?;
    registered_image(runtime, directory, "flutter_android_volume_storage")
}

fn storage_spec(runtime: &Path) -> io::Result<super::LaunchSpec> {
    let spec: super::LaunchSpec = frontcap_core::state::load(&super::spec_path(runtime))?
        .ok_or_else(|| io::Error::other("native launch specification missing"))?;
    if crate::runtime::load_runtime(runtime)?.is_some_and(|record| record.launch_id != spec.launch_id) {
        return Err(io::Error::other("storage specification belongs to another launch"));
    }
    Ok(spec)
}

fn declared_image_for_target(runtime: &Path, target: &super::target::Launch) -> io::Result<Option<PathBuf>> {
    let simulator = match target {
        super::target::Launch::Ios(config) => &config.xctest.simulator,
        super::target::Launch::IosHost(config) => &config.simulator,
        super::target::Launch::Webview(config) => &config.simulator,
        _ => return Ok(None),
    };
    let Some(directory) = &simulator.data_volume_root else { return Ok(None) };
    simulator.validate_data_volume()?;
    registered_image(runtime, directory, "ios_simulator_volume_storage")
}

fn registered_image(runtime: &Path, directory: &Path, role: &str) -> io::Result<Option<PathBuf>> {
    let parent = directory.parent().ok_or_else(|| io::Error::other("image parent missing"))?;
    let registered: Registered = frontcap_core::state::load(&runtime.join("native-storage-roots.json"))?
        .ok_or_else(|| io::Error::other("declared image storage registration missing"))?;
    let roots: Vec<_> = registered.roots.iter().filter(|root| root.role == role).collect();
    let metadata = fs::symlink_metadata(parent)?;
    if roots.len() != 1 || roots[0].path != parent || !metadata.is_dir()
        || metadata.dev() != roots[0].device || metadata.ino() != roots[0].inode
        || fs::canonicalize(parent)? != parent {
        return Err(io::Error::other("declared image storage identity changed"));
    }
    let image = directory.join("worker/volume/bounded.dmg");
    // Never traverse symlinks or mounted children to inspect a backing file. Missing
    // directories can mean either not acquired or retired, not proof of either.
    for path in [directory.to_path_buf(), directory.join("worker"), directory.join("worker/volume")] {
        match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.is_dir() || meta.dev() != metadata.dev() =>
                return Err(io::Error::other("declared image path changed type or filesystem")),
            Ok(_) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(Some(image))
}

pub(super) fn record_session_location(runtime: &Path, spec: &super::LaunchSpec,
    budget: Option<&frontcap_core::write_budget::WriteBudget>) -> io::Result<()> {
    super::save_budgeted(budget, frontcap_core::write_budget::Class::Ordinary, &runtime.join("native-session-location.json"), &SessionLocation {
        launch_id: spec.launch_id.clone(), session_root: spec.session_root.clone(),
    })
}

pub(super) fn session_directory(runtime: &Path) -> io::Result<PathBuf> {
    let location: Option<SessionLocation> = frontcap_core::state::load(&runtime.join("native-session-location.json"))?;
    let Some(SessionLocation { launch_id, session_root: Some(parent) }) = location else {
        // Legacy default runs have no location record. An external registration must never
        // silently fall back to unrelated files in the runtime after losing that record.
        if frontcap_core::state::load::<Registered>(&runtime.join("native-storage-roots.json"))?
            .is_some_and(|r| r.roots.iter().any(|root| root.role == "session_storage")) {
            return Err(io::Error::other("external session location missing"));
        }
        return Ok(runtime.join("native-session"));
    };
    if crate::runtime::load_runtime(runtime)?.is_some_and(|record| record.launch_id != launch_id) {
        return Err(io::Error::other("session location belongs to another launch"));
    }
    let registered: Registered = frontcap_core::state::load(&runtime.join("native-storage-roots.json"))?
        .ok_or_else(|| io::Error::other("external session storage registration missing"))?;
    let mut matching = registered.roots.iter().filter(|root| root.role == "session_storage");
    let root = matching.next().ok_or_else(|| io::Error::other("external session storage identity missing"))?;
    let metadata = fs::symlink_metadata(&parent)?;
    if matching.next().is_some() || root.path != parent || !parent.is_absolute() || !metadata.is_dir()
        || metadata.dev() != root.device || metadata.ino() != root.inode {
        return Err(io::Error::other("external session storage identity changed"));
    }
    let path = parent.join(launch_id.as_str());
    match fs::symlink_metadata(&path) {
        Ok(metadata) if !metadata.is_dir() => return Err(io::Error::other("external session is not a directory")),
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {},
    }
    if let Some(acquired) = frontcap_core::state::load::<Registered>(&runtime.join("native-storage-acquired-roots.json"))? {
        let entries: Vec<_> = acquired.roots.iter().filter(|root| root.role == "session_evidence").collect();
        let metadata = fs::symlink_metadata(&path)?;
        if entries.len() != 1 || entries[0].path != path || !metadata.is_dir()
            || entries[0].device != metadata.dev() || entries[0].inode != metadata.ino() {
            return Err(io::Error::other("external session evidence identity changed"));
        }
    } else if runtime.join("native-status.json").exists() {
        return Err(io::Error::other("external session evidence registration missing"));
    }
    Ok(path)
}
