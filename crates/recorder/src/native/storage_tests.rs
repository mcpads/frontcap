use super::*;

#[test]
fn control_headroom_is_physical_anonymous_and_released_only_once() {
    let runtime = tempfile::tempdir().unwrap();
    let roots = Roots::runtime(runtime.path()).unwrap();
    roots.reserve_control(runtime.path()).unwrap();
    {
        let reserve = roots.control_reserve.lock().unwrap();
        let metadata = reserve.as_ref().unwrap().metadata().unwrap();
        assert!(metadata.blocks() * 512 >= 8 * 1024 * 1024);
        assert_eq!(metadata.nlink(), 0);
    }
    assert_eq!(fs::read_dir(runtime.path()).unwrap().count(), 0);
    assert!(roots.reserve_control(runtime.path()).is_err());
    roots.release_control();
    assert!(roots.control_reserve.lock().unwrap().is_none());
    roots.release_control();
    assert_eq!(fs::read_dir(runtime.path()).unwrap().count(), 0);
}

#[test]
fn resolved_storage_refuses_retargeting_even_when_original_directory_survives() {
    let runtime = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let source = runtime.path().join("library");
    std::os::unix::fs::symlink(first.path(), &source).unwrap();
    let mut roots = Roots::runtime(runtime.path()).unwrap();
    roots.add_resolved("host_user_library", &source).unwrap();
    roots.admit(0).unwrap();
    fs::remove_file(&source).unwrap();
    std::os::unix::fs::symlink(second.path(), &source).unwrap();
    assert!(roots.admit(0).unwrap_err().to_string().contains("source resolution changed"));
    assert!(first.path().is_dir() && second.path().is_dir());
    fs::remove_file(&source).unwrap();
    assert!(roots.admit(0).is_err());
    assert_eq!(roots.roots[1].path, first.path().canonicalize().unwrap());
}

#[cfg(target_os = "macos")]
#[test]
fn host_storage_remains_admitted_when_target_storage_is_separate() {
    let runtime = tempfile::tempdir().unwrap();
    let mut roots = Roots::for_launch(runtime.path(), &flutter_storage_launch(runtime.path(), true)).unwrap();
    let expected = [
        ("host_user_library", PathBuf::from(std::env::var_os("HOME").unwrap()).join("Library")),
        ("host_shared_library", PathBuf::from("/Library")),
        ("host_system_state", PathBuf::from("/private/var")),
        ("host_temporary_storage", std::env::temp_dir()),
    ];
    for (role, path) in expected {
        let root = roots.roots.iter().find(|root| root.role == role).unwrap();
        assert_eq!(root.path, path.canonicalize().unwrap());
        assert_eq!(root.device, fs::metadata(path).unwrap().dev());
    }
    // The actual temp path is independent of the broad /private/var anchor: TMPDIR
    // may point at another volume, and its original identity must remain authoritative.
    let temporary = roots.roots.iter().find(|root| root.role == "host_temporary_storage").unwrap();
    assert_eq!(temporary.source_path.as_deref(), Some(std::env::temp_dir().as_path()));
    // Exercise the host floor independently of runtime/app-image admission, without
    // filling shared host storage or acquiring any authority over system services.
    roots.roots.retain(|root| root.role.starts_with("host_"));
    roots.admit(0).unwrap();
    assert!(roots.admit(u64::MAX).unwrap_err().to_string().contains("host_user_library"));
    assert!(!roots.cleanup_authority && !roots.whole_run_quota);
    assert!(roots.known_unregistered_writers.contains(&"simulator_data"));
    roots.roots.retain(|root| root.role == "host_temporary_storage");
    roots.admit(0).unwrap();
    assert!(roots.admit(u64::MAX).unwrap_err().to_string().contains("host_temporary_storage"));
}

#[test]
fn storage_roots_reject_replacement_without_deleting_or_reregistering_it() {
    let runtime = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let parent = external.path().join("scratch");
    fs::create_dir(&parent).unwrap();
    let mut roots = Roots::runtime(runtime.path()).unwrap();
    roots.add("android_temporary_storage", &parent).unwrap();
    roots.admit(0).unwrap();
    let original = external.path().join("original");
    fs::rename(&parent, &original).unwrap();
    fs::create_dir(&parent).unwrap();
    fs::write(parent.join("unrelated"), b"keep").unwrap();
    let reason = roots.admit(0).unwrap_err().to_string();
    assert!(reason.contains("android_temporary_storage") && reason.contains("identity changed"));
    assert!(original.is_dir());
    assert_eq!(fs::read(parent.join("unrelated")).unwrap(), b"keep");
    fs::remove_dir_all(&parent).unwrap();
    assert!(roots.admit(0).is_err());
    std::os::unix::fs::symlink(&original, &parent).unwrap();
    assert!(roots.admit(0).is_err());
    assert!(Roots::runtime(&parent).is_err());
}

#[test]
fn storage_roots_enforce_the_floor_without_claiming_cleanup_authority() {
    let runtime = tempfile::tempdir().unwrap();
    let roots = Roots::runtime(runtime.path()).unwrap();
    roots.admit(0).unwrap();
    assert!(roots.admit(u64::MAX).unwrap_err().to_string().contains("requires"));
    let record = serde_json::to_value(&roots).unwrap();
    assert_eq!(record["cleanup_authority"], false);
    assert_eq!(record["roots"][0]["device"], fs::metadata(runtime.path()).unwrap().dev());
}

#[test]
fn storage_generated_volume_requires_a_real_filesystem_boundary() {
    let runtime = tempfile::tempdir().unwrap();
    let mut roots = Roots::runtime(runtime.path()).unwrap();
    let mount = runtime.path().join("not-a-mount");
    roots.pending_generated_volume = Some(mount.clone());
    assert!(roots.activate_generated().is_err());
    fs::create_dir(&mount).unwrap();
    assert!(roots.activate_generated().unwrap_err().to_string().contains("separate filesystem"));
    assert_eq!(roots.roots.len(), 1);
    assert_eq!(roots.pending_generated_volume, Some(mount));
}


#[test]
fn flutter_source_storage_is_admitted_and_replacement_is_not_adopted() {
    let runtime = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let project = external.path().join("project");
    fs::create_dir(&project).unwrap();
    let target = flutter_storage_launch(&project, false);
    let roots = Roots::for_launch(runtime.path(), &target).unwrap();
    roots.admit(0).unwrap();
    assert!(roots.roots.iter().any(|root| root.role == "flutter_project_build" && root.path == project));
    assert!(!roots.whole_run_quota);
    assert!(roots.known_unregistered_writers.contains(&"flutter_sdk_and_dependency_caches"));
    fs::rename(&project, external.path().join("original")).unwrap();
    fs::create_dir(&project).unwrap();
    fs::write(project.join("foreign"), b"preserve").unwrap();
    assert!(roots.admit(0).unwrap_err().to_string().contains("flutter_project_build"));
    assert_eq!(fs::read(project.join("foreign")).unwrap(), b"preserve");
}

#[test]
fn installed_flutter_does_not_require_build_roots_or_claim_simulator_quota() {
    let runtime = tempfile::tempdir().unwrap();
    let missing = runtime.path().join("absent-build-project");
    let roots = Roots::for_launch(runtime.path(), &flutter_storage_launch(&missing, true)).unwrap();
    roots.admit(0).unwrap();
    assert!(!roots.roots.iter().any(|root| root.role == "flutter_project_build"));
    assert_eq!(roots.known_unregistered_writers, ["simulator_data"]);
    let record = serde_json::to_value(&roots).unwrap();
    assert_eq!(record["whole_run_quota"], false);
    assert_eq!(record["enforcement"], "filesystem_free_space_floor");
    assert!(!missing.exists());
    assert!(Roots::for_launch(runtime.path(), &flutter_storage_launch(&missing, false)).is_err());
}

fn flutter_storage_launch(project: &Path, installed: bool) -> super::super::target::Launch {
    let mut config = serde_json::json!({"flutter":"/unused/flutter", "project":project,
        "platform":"ios", "source_sha256":"a".repeat(64), "acquisition_timeout_s":10, "lifetime_s":60});
    if installed {
        config["installed_app"] = serde_json::json!({"product":"/unused/Runner.app", "inventory":{"files":{},"directories":[]}});
    }
    super::super::target::Launch::Flutter(serde_json::from_value(serde_json::json!({
        "worker":{"python":"/unused/python", "worker_script":"/unused/worker", "config":config},
        "http_script":"/unused/http"
    })).unwrap())
}

#[test]
#[cfg(target_os = "macos")]
fn flutter_data_volume_tracks_its_worker_mount_and_requires_installed_ios() {
    for supervised in [false, true] {
    let root = tempfile::tempdir().unwrap();
    let mut target = flutter_storage_launch(root.path(), true);
    let super::super::target::Launch::Flutter(config) = &mut target else { unreachable!() };
    config.worker.config.data_volume = Some(frontcap_native::flutter_session::DataVolume { size_mib: 8192, reserve_mib: 64 });
    if supervised { config.worker.config.resource_executable = Some("/resource-worker".into()); }
    let relative = config.worker.config.data_volume_mount();
    let mut roots = Roots::for_launch(root.path(), &target).unwrap();
    assert_eq!(roots.known_unregistered_writers, ["simulator_shared_services_and_logs"]);
    let mount = root.path().join("native-session").join(&relative);
    assert_eq!(roots.pending_flutter_data_volume, Some(mount.clone()));
    fs::create_dir_all(&mount).unwrap();
    assert!(roots.activate_generated().unwrap_err().to_string().contains("separate filesystem"));
    let launch = frontcap_core::ids::LaunchId::generate();
    roots.route_session(root.path(), &launch).unwrap();
    assert_eq!(roots.pending_flutter_data_volume, Some(root.path().join(launch.as_str())
        .join(&relative)));
    let super::super::target::Launch::Flutter(config) = &mut target else { unreachable!() };
    if supervised {
        config.worker.config.initial_data = Some("/supervised-input".into());
        assert!(Roots::for_launch(root.path(), &target).is_ok());
        let super::super::target::Launch::Flutter(config) = &mut target else { unreachable!() };
        config.worker.config.initial_data = None;
        config.worker.config.resource_executable = Some("relative-worker".into());
        assert!(Roots::for_launch(root.path(), &target).is_err());
    }
    let super::super::target::Launch::Flutter(config) = &mut target else { unreachable!() };
    config.worker.config.installed_app = None;
    assert!(Roots::for_launch(root.path(), &target).is_err());
    }
}

fn simulator_volume_launch() -> super::super::target::Launch {
    let mut config: frontcap_native::webview_session::Config = serde_json::from_str(
        include_str!("../../tests/fixtures/native/webview-launch.json")).unwrap();
    config.simulator.data_volume = Some(frontcap_native::storage_volume::Config {
        python: "/python".into(), worker: "/volume-worker".into(), size_mib: 8192, reserve_mib: 64,
    });
    super::super::target::Launch::Webview(Box::new(config))
}

#[test]
fn simulator_data_floor_tracks_generated_and_external_session_routes_without_claiming_shared_writers() {
    let mut uikit: frontcap_native::ios_target::IosLaunch = serde_json::from_str(
        include_str!("../../tests/fixtures/native/ios-launch.json")).unwrap();
    uikit.resource_lifetime_ms = Some(600_000);
    uikit.xctest.simulator.data_volume = Some(frontcap_native::storage_volume::Config {
        python: "/python".into(), worker: "/volume-worker".into(), size_mib: 8192, reserve_mib: 64,
    });
    for target in [simulator_volume_launch(), super::super::target::Launch::Ios(uikit)] {

    let root = tempfile::tempdir().unwrap();
    let mut roots = Roots::for_launch(root.path(), &target).unwrap();
    assert_eq!(roots.known_unregistered_writers, ["simulator_shared_services_and_logs"]);
    let pending = roots.pending_simulator_data_volume.clone().unwrap();
    fs::create_dir_all(&pending).unwrap();
    assert!(roots.activate_generated().unwrap_err().to_string().contains("separate filesystem"));
    assert!(!roots.roots.iter().any(|root| root.role == "ios_generated_data_volume"));
    assert!(!roots.whole_run_quota && !roots.cleanup_authority);
    let external = tempfile::tempdir().unwrap();
    let launch = frontcap_core::ids::LaunchId::generate();
    roots.route_session(external.path(), &launch).unwrap();
    assert_eq!(roots.pending_simulator_data_volume.unwrap(), external.path().join(launch.as_str())
        .join("target/simulator/data-volume/worker/volume/mount"));
    assert!(pending.exists());
    }
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "requires a live caller-owned APFS mount at the generated Simulator data path"]
fn simulator_data_volume_floor_uses_the_mounted_filesystem() {
    let root = PathBuf::from(std::env::var_os("FRONTCAP_STORAGE_MOUNTED_ROOT").expect("owned fixture"));
    let mut roots = Roots::for_launch(&root, &simulator_volume_launch()).unwrap();
    let path = roots.pending_simulator_data_volume.clone().unwrap();
    roots.activate_generated().unwrap();
    let registered = roots.roots.iter().find(|r| r.role == "ios_generated_data_volume").unwrap();
    assert_eq!(registered.path, path); assert_ne!(registered.device, roots.roots[0].device);
    roots.admit(0).unwrap();
    let path_bytes = CString::new(path.as_os_str().as_bytes()).unwrap();
    let mut stats = MaybeUninit::<libc::statvfs>::uninit();
    assert_eq!(unsafe { libc::statvfs(path_bytes.as_ptr(), stats.as_mut_ptr()) }, 0);
    let stats = unsafe { stats.assume_init() };
    let unavailable = u64::from(stats.f_bavail) * stats.f_frsize + 1024 * 1024;
    let error = roots.admit(unavailable).unwrap_err().to_string();
    assert!(error.contains("ios_generated_data_volume"), "{error}");
    assert!(path.join("probe-data").exists());
    assert!(!roots.whole_run_quota);
}

#[test]
fn external_simulator_image_location_keeps_its_registered_parent_when_session_is_routed() {
    let runtime = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let parent = external.path().canonicalize().unwrap();
    let mut target = simulator_volume_launch();
    let super::super::target::Launch::Webview(config) = &mut target else { unreachable!() };
    config.simulator.data_volume_root = Some(parent.join("fresh"));
    let mut roots = Roots::for_launch(runtime.path(), &target).unwrap();
    assert!(roots.roots.iter().any(|r| r.role == "ios_simulator_volume_storage" && r.path == parent));
    let pending = roots.pending_simulator_data_volume.clone();
    roots.route_session(runtime.path(), &frontcap_core::ids::LaunchId::generate()).unwrap();
    assert_eq!(roots.pending_simulator_data_volume, pending);
    assert_eq!(pending.unwrap(), parent.join("fresh/worker/volume/mount"));
    roots.admit(0).unwrap();
}

#[test]
#[cfg(target_os = "macos")]
fn flutter_android_volume_tracks_original_mount_and_rejects_partial_ownership() {
    let root = tempfile::tempdir().unwrap();
    let mut target: super::super::target::Launch = super::super::target::Launch::Flutter(serde_json::from_value(serde_json::json!({
        "worker":{"python":"/python", "worker_script":"/worker", "config": {
            "platform":"android", "source_sha256":"a".repeat(64), "acquisition_timeout_s":120,"lifetime_s":60,
            "android_sdk":"/sdk", "avd":"Phone", "resource_executable":"/owner",
            "installed_app":{"product":"/client.apk","apk_sha256":"a".repeat(64),"package":"dev.client","activity":"dev.client.Main"},
            "android_resources":{"avd_home":"/frozen-avd", "avd_inventory":{"files":{},"directories":[]},
                "port":5580,"adb_server_port":15037,
                "scratch_volume":{"python":"/python","worker":"/volume-worker","size_mib":4096,"reserve_mib":64}}
        }}, "http_script":"/http"
    })).unwrap());
    let mut roots = Roots::for_launch(root.path(), &target).unwrap();
    let relative = "session/worker/resource-group/scratch-volume/worker/volume/mount";
    assert_eq!(roots.known_unregistered_writers, ["android_shared_services_and_logs"]);
    assert_eq!(roots.pending_flutter_data_volume, Some(root.path().join("native-session").join(relative)));
    let launch = frontcap_core::ids::LaunchId::generate();
    roots.route_session(root.path(), &launch).unwrap();
    let mount = root.path().join(launch.as_str()).join(relative);
    assert_eq!(roots.pending_flutter_data_volume, Some(mount.clone()));
    fs::create_dir_all(mount).unwrap();
    assert!(roots.activate_generated().unwrap_err().to_string().contains("separate filesystem"));
    let external = root.path().canonicalize().unwrap().join("external-volume");
    let super::super::target::Launch::Flutter(config) = &mut target else { unreachable!() };
    config.worker.config.android_resources.as_mut().unwrap().scratch_volume_root = Some(external.clone());
    let mut roots = Roots::for_launch(root.path(), &target).unwrap();
    roots.route_session(root.path(), &launch).unwrap();
    assert_eq!(roots.pending_flutter_data_volume, Some(external.join("worker/volume/mount")));
    assert!(roots.roots.iter().any(|r| r.role == "flutter_android_volume_storage" && r.path == external.parent().unwrap()));
    assert!(roots.flutter_android_volume);
    let super::super::target::Launch::Flutter(config) = &mut target else { unreachable!() };
    config.worker.config.android_resources.as_mut().unwrap().adb_server_port = 5581;
    assert!(Roots::for_launch(root.path(), &target).is_err());
    let super::super::target::Launch::Flutter(config) = &mut target else { unreachable!() };
    config.worker.config.android_resources.as_mut().unwrap().adb_server_port = 15037;
    config.worker.config.resource_executable = None;
    assert!(Roots::for_launch(root.path(), &target).is_err());
}
