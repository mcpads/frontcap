use super::*;

#[test]
fn declared_image_inventory_keeps_external_bytes_separate_and_refuses_changed_paths() {
    use frontcap_recorder::native::{LaunchSpec, target::Launch};
    for android in [false, true] {
    if android && !cfg!(target_os = "macos") { continue; }
    let observation = |usage: Usage| if android { usage.declared_android_scratch_image } else { usage.declared_simulator_image };
    let runtime = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let parent = external.path().canonicalize().unwrap().join("images");
    fs::create_dir(&parent).unwrap();
    let mut config = serde_json::from_str::<serde_json::Value>(
        include_str!("../../recorder/tests/fixtures/native/webview-launch.json")).unwrap();
    config["simulator"]["data_volume"] = serde_json::json!({
        "python": "/python", "worker": "/volume-worker", "size_mib": 8192, "reserve_mib": 64,
    });
    config["simulator"]["data_volume_root"] = serde_json::json!(parent.join("fresh"));
    let target = if android {
        Launch::Flutter(serde_json::from_value(serde_json::json!({
            "worker":{"python":"/python", "worker_script":"/worker", "config": {
                "platform":"android", "source_sha256":"a".repeat(64), "acquisition_timeout_s":120,"lifetime_s":60,
                "android_sdk":"/sdk", "avd":"Phone", "resource_executable":"/owner",
                "installed_app":{"product":"/client.apk","apk_sha256":"a".repeat(64),"package":"dev.client","activity":"dev.client.Main"},
                "android_resources":{"avd_home":"/frozen-avd", "avd_inventory":{"files":{},"directories":[]},
                    "port":5580,"adb_server_port":15037, "scratch_volume_root":parent.join("fresh"),
                    "scratch_volume":{"python":"/python","worker":"/volume-worker","size_mib":4096,"reserve_mib":64}}
            }}, "http_script":"/http"
        })).unwrap())
    } else { Launch::Webview(Box::new(serde_json::from_value(config).unwrap())) };
    let mut spec = LaunchSpec {
        request_id: frontcap_core::ids::RequestId::new("launch").unwrap(),
        launch_id: frontcap_core::ids::LaunchId::generate(),
        run_id: frontcap_core::ids::RunId::new("storage-test").unwrap(),
        target, session_executable: "/worker".into(),
        acquisition_budget_ms: 1000, min_free_bytes: None, write_budget: None,
        session_root: None, session_volume: None, service: None,
    };
    assert!(matches!(observation(measure(runtime.path(), 100).unwrap()),
        ImageObservation::Unavailable { .. }));
    frontcap_core::state::save_atomic(&frontcap_recorder::native::spec_path(runtime.path()), &spec).unwrap();
    assert!(matches!(observation(measure(runtime.path(), 100).unwrap()),
        ImageObservation::Unavailable { .. }));
    let meta = parent.metadata().unwrap();
    frontcap_core::state::save_atomic(&runtime.path().join("native-storage-roots.json"),
        &serde_json::json!({"roots":[{"role":if android { "flutter_android_volume_storage" } else { "ios_simulator_volume_storage" }, "path":parent,
            "device":meta.dev(), "inode":meta.ino()}]})).unwrap();
    let before = measure(runtime.path(), 100).unwrap();
    let before_bytes = before.regular_files.logical_bytes;
    assert!(matches!(observation(before), ImageObservation::Absent { .. }));
    let image = parent.join("fresh/worker/volume/bounded.dmg");
    fs::create_dir_all(image.parent().unwrap()).unwrap();
    let file = fs::File::create(&image).unwrap();
    file.set_len(256 * 1024 * 1024).unwrap();
    let after = measure(runtime.path(), 100).unwrap();
    assert_eq!(before_bytes, after.regular_files.logical_bytes);
    assert_eq!(after.owned_volume_image_files.count, 0);
    match observation(after) {
        ImageObservation::Measured { path, logical_bytes, allocated_bytes } => {
            assert_eq!(path, image);
            assert_eq!(logical_bytes, file.metadata().unwrap().len());
            assert_eq!(allocated_bytes, file.metadata().unwrap().blocks() * 512);
        }
        _ => panic!("declared image was not measured"),
    }
    fs::rename(&image, image.with_extension("original")).unwrap();
    std::os::unix::fs::symlink(image.with_extension("original"), &image).unwrap();
    assert!(matches!(observation(measure(runtime.path(), 100).unwrap()),
        ImageObservation::Unavailable { .. }));
    fs::remove_file(&image).unwrap();
    fs::rename(parent.join("fresh"), parent.join("original")).unwrap();
    std::os::unix::fs::symlink(parent.join("original"), parent.join("fresh")).unwrap();
    assert!(matches!(observation(measure(runtime.path(), 100).unwrap()),
        ImageObservation::Unavailable { .. }));
    fs::rename(&parent, parent.with_extension("original")).unwrap();
    fs::create_dir(&parent).unwrap();
    assert!(matches!(observation(measure(runtime.path(), 100).unwrap()),
        ImageObservation::Unavailable { .. }));
    assert!(parent.with_extension("original").join("original/worker/volume/bounded.original").exists());
    match &mut spec.target {
        Launch::Webview(config) => config.simulator.data_volume_root = None,
        Launch::Flutter(config) => config.worker.config.android_resources.as_mut().unwrap().scratch_volume_root = None,
        _ => unreachable!(),
    }
    frontcap_core::state::save_atomic(&frontcap_recorder::native::spec_path(runtime.path()), &spec).unwrap();
    assert!(matches!(observation(measure(runtime.path(), 100).unwrap()),
        ImageObservation::NotDeclared));
    }
}

#[test]
fn measurement_keeps_file_bytes_and_skips_links_without_mutating_evidence() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("large"), vec![0; 1024]).unwrap();
    fs::create_dir(root.path().join("nested")).unwrap();
    for (name, data) in [
        ("nested/stdout.bin.gz", b"compressed".as_slice()),
        ("screen.png", b"image"),
        ("app.apk", b"app"),
        ("record.json", b"{}"),
    ] {
        fs::write(root.path().join(name), data).unwrap();
    }
    std::os::unix::fs::symlink(outside.path(), root.path().join("linked")).unwrap();
    // SDK staging logs and finalized attachments share one category, including restart epochs.
    for parent in ["native-session/target", "native-session/target/epochs/g-replacement"] {
        let bundle = root.path().join(parent).join("test.xcresult/Staging");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("stdout.bin"), b"sdk log").unwrap();
        fs::write(bundle.join("screen.png"), b"sdk image").unwrap();
    }
    fs::write(root.path().join("test.xcresult"), b"ordinary file").unwrap();
    let usage = measure(root.path(), 100).unwrap();
    assert_eq!(usage.regular_files.logical_bytes, 65);
    assert_eq!(usage.regular_files.count, 9);
    assert_eq!(usage.xctest_result_files.count, 4);
    assert_eq!(usage.xctest_result_files.logical_bytes, 32);
    assert_eq!(usage.png_files.logical_bytes, 5);
    assert_eq!(usage.other_files.logical_bytes, 15);
    assert_eq!(usage.diagnostic_stream_files.logical_bytes, 10);
    assert_eq!(usage.symlinks_skipped, 1);
    assert!(!usage.limit_reached);
    assert_eq!(fs::read(root.path().join("record.json")).unwrap(), b"{}");
    assert_eq!(fs::read(outside.path().join("large")).unwrap().len(), 1024);
    assert!(measure(&root.path().join("linked"), 100).is_err());
    let bounded = measure(root.path(), 1).unwrap();
    assert_eq!(bounded.entries_scanned, 1);
    assert!(bounded.limit_reached);
    assert!(bounded.regular_files.logical_bytes <= usage.regular_files.logical_bytes);
}

#[test]
fn temporary_emulator_disks_are_separate_from_retained_evidence_and_report_allocation() {
    for target in ["device", "target"] {
        let root = tempfile::tempdir().unwrap();
        let owner = root.path().join("native-session").join(target);
        let temporary = owner.join("emulator-temporary");
        fs::create_dir_all(&temporary).unwrap();
        let disk = temporary.join("overlay.qcow2");
        let file = fs::File::create(&disk).unwrap();
        file.set_len(16 * 1024 * 1024).unwrap();
        let receipt = owner.join("emulator-temporary-cleanup.json");
        fs::write(&receipt, b"{}").unwrap();
        let usage = measure(root.path(), 100).unwrap();
        assert!(usage.allocated_bytes_measured);
        assert_eq!(usage.emulator_temporary_files.count, 1);
        assert_eq!(
            usage.emulator_temporary_files.logical_bytes,
            file.metadata().unwrap().len()
        );
        assert_eq!(
            usage.emulator_temporary_files.allocated_bytes,
            file.metadata().unwrap().blocks() * 512
        );
        assert_eq!(usage.other_files.count, 1);
        assert_eq!(
            usage.regular_files.allocated_bytes,
            usage.emulator_temporary_files.allocated_bytes
                + receipt.metadata().unwrap().blocks() * 512
        );
        fs::remove_dir_all(&temporary).unwrap();
        let retired = measure(root.path(), 100).unwrap();
        assert_eq!(retired.emulator_temporary_files.count, 0);
        assert_eq!(retired.regular_files.logical_bytes, 2);
    }
}

#[test]
fn volume_images_are_separate_and_foreign_filesystem_entries_are_not_descended() {
    let root = tempfile::tempdir().unwrap();
    let volume = root.path().join("native-session/target-resources/scratch-volume/worker/volume");
    fs::create_dir_all(volume.join("mount")).unwrap();
    let image = fs::File::create(volume.join("bounded.dmg")).unwrap();
    image.set_len(256 * 1024 * 1024).unwrap();
    fs::write(volume.join("volume.json"), b"{}").unwrap();
    let mut usage = measure(root.path(), 100).unwrap();
    assert_eq!(usage.owned_volume_image_files.count, 1);
    assert_eq!(usage.owned_volume_image_files.logical_bytes, image.metadata().unwrap().len());
    assert_eq!(usage.owned_volume_image_files.allocated_bytes, image.metadata().unwrap().blocks() * 512);
    assert_eq!(usage.other_files.logical_bytes, 2);
    // Simulate traversal encountering a device other than the one declared at acquisition.
    // Real mounted-filesystem behavior belongs to the installed capacity probe.
    usage.filesystem_device = usage.filesystem_device.wrapping_add(1);
    let files_before = usage.regular_files.count;
    let entries_before = usage.entries_scanned;
    visit(root.path(), root.path(), 0, &mut usage);
    assert_eq!(usage.filesystem_boundaries_skipped, 1);
    assert_eq!(usage.entries_scanned - entries_before, 1);
    assert_eq!(usage.regular_files.count, files_before);
    assert!(volume.join("bounded.dmg").exists());
}

#[test]
#[ignore = "requires a fresh caller-owned mounted APFS fixture in FRONTCAP_STORAGE_MOUNTED_ROOT"]
fn mounted_volume_contents_do_not_duplicate_backing_image_storage() {
    assert_mounted_volume_accounting("native-session/target-resources/scratch-volume/worker/volume");
}

#[test]
#[ignore = "requires a fresh caller-owned mounted APFS Simulator fixture in FRONTCAP_STORAGE_MOUNTED_ROOT"]
fn mounted_simulator_data_does_not_duplicate_backing_image_storage() {
    assert_mounted_volume_accounting("native-session/target/simulator/data-volume/worker/volume");
}

fn assert_mounted_volume_accounting(relative: &str) {
    let root = std::path::PathBuf::from(std::env::var_os("FRONTCAP_STORAGE_MOUNTED_ROOT").expect("mounted fixture"));
    let volume = root.join(relative);
    assert_ne!(volume.join("mount").metadata().unwrap().dev(), root.metadata().unwrap().dev());
    assert!(volume.join("mount/probe-data").metadata().unwrap().len() > 0);
    let usage = measure(&root, 1000).unwrap();
    assert_eq!(usage.filesystem_boundaries_skipped, 1);
    assert!(!usage.limit_reached);
    assert_eq!(usage.unreadable_entries, 0);
    assert_eq!(usage.regular_files.count, 2); // Image and live owner's volume.json, no mounted contents.
    assert_eq!(usage.owned_volume_image_files.count, 1);
    assert_eq!(usage.regular_files.logical_bytes,
        volume.join("bounded.dmg").metadata().unwrap().len() + volume.join("volume.json").metadata().unwrap().len());
}

#[test]
fn simulator_data_backing_image_is_classified_separately_from_evidence() {
    let root = tempfile::tempdir().unwrap();
    let volume = root.path().join("native-session/target/simulator/data-volume/worker/volume");
    fs::create_dir_all(&volume).unwrap();
    fs::write(volume.join("bounded.dmg"), b"backing image").unwrap();
    fs::write(volume.join("volume.json"), b"{}").unwrap();
    let usage = measure(root.path(), 100).unwrap();
    assert_eq!(usage.owned_volume_image_files.count, 1);
    assert_eq!(usage.owned_volume_image_files.logical_bytes, 13);
    assert_eq!(usage.other_files.logical_bytes, 2);
    assert!(volume.join("bounded.dmg").exists());
}


#[test]
fn external_session_inventory_is_separate_bounded_and_refuses_replacement() {
    let runtime = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let parent = external.path().canonicalize().unwrap();
    let launch = frontcap_core::ids::LaunchId::generate();
    let path = parent.join(launch.as_str());
    let parent_meta = parent.metadata().unwrap();
    let location = runtime.path().join("native-session-location.json");
    let save = frontcap_core::state::save_atomic::<serde_json::Value>;
    save(&location, &serde_json::json!({"launch_id":launch,"session_root":parent})).unwrap();
    save(&runtime.path().join("native-storage-roots.json"), &serde_json::json!({"roots":[{
        "role":"session_storage","path":parent,"device":parent_meta.dev(),"inode":parent_meta.ino()
    }]})).unwrap();
    assert!(matches!(measure(runtime.path(), 100).unwrap().external_session, SessionObservation::Absent { .. }));
    fs::create_dir(&path).unwrap();
    let meta = path.metadata().unwrap();
    save(&runtime.path().join("native-storage-acquired-roots.json"), &serde_json::json!({"roots":[{
        "role":"session_evidence","path":path,"device":meta.dev(),"inode":meta.ino()
    }]})).unwrap();
    let before = measure(runtime.path(), 100).unwrap();
    fs::write(path.join("result.json"), b"original-result").unwrap();
    std::os::unix::fs::symlink(runtime.path(), path.join("do-not-follow")).unwrap();
    let volume = path.join("session/worker/resource-group/scratch-volume/worker/volume");
    fs::create_dir_all(&volume).unwrap();
    let image = fs::File::create(volume.join("bounded.dmg")).unwrap();
    image.set_len(4096).unwrap();
    let location_bytes = fs::read(&location).unwrap();
    let observed = measure(runtime.path(), 100).unwrap();
    assert_eq!(observed.regular_files.logical_bytes, before.regular_files.logical_bytes);
    let SessionObservation::Measured { path: measured, usage } = observed.external_session else { panic!("external session not measured") };
    assert_eq!(measured, path);
    assert_eq!(usage.scope, "external_session_directory_same_filesystem");
    assert_eq!(usage.regular_files.logical_bytes, 15 + 4096);
    assert_eq!(usage.owned_volume_image_files.logical_bytes, 4096);
    assert_eq!(usage.symlinks_skipped, 1);
    assert!(usage.entries_scanned + observed.entries_scanned <= 100);
    let budget = before.entries_scanned + 1;
    let limited = measure(runtime.path(), budget).unwrap();
    let SessionObservation::Measured { usage, .. } = limited.external_session else { panic!("external budget was not shared") };
    assert!(usage.limit_reached);
    assert!(limited.entries_scanned + usage.entries_scanned <= budget);
    assert!(matches!(measure(runtime.path(), 1).unwrap().external_session, SessionObservation::EntryBudgetExhausted { .. }));
    assert_eq!(fs::read(&location).unwrap(), location_bytes);
    fs::rename(&path, parent.join("original")).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("foreign"), b"replacement").unwrap();
    assert!(matches!(measure(runtime.path(), 100).unwrap().external_session, SessionObservation::Unavailable { .. }));
    fs::remove_file(&location).unwrap();
    assert!(matches!(measure(runtime.path(), 100).unwrap().external_session, SessionObservation::Unavailable { .. }));
    assert_eq!(fs::read(parent.join("original/result.json")).unwrap(), b"original-result");
    assert_eq!(fs::read(path.join("foreign")).unwrap(), b"replacement");
}
