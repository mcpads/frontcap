use super::*;
pub(crate) fn fixture(root: &Path) {
    for name in REQUIRED {
        let path = root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, name).unwrap();
        std::fs::set_permissions(
            path,
            std::fs::Permissions::from_mode(if name.ends_with("Info.plist") {
                0o600
            } else {
                0o700
            }),
        )
        .unwrap();
    }
    std::fs::write(root.join("SampleApp.xctestrun"), "test configuration").unwrap();
}
#[test]
fn copied_products_are_independent_and_retain_exact_bytes_and_execution_permissions() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    fixture(&source);
    let manifest = ProductsManifest::capture(&source, "SampleApp.xctestrun").unwrap();
    let copy = temp.path().join("copy");
    manifest.copy_verified(&source, &copy).unwrap();
    manifest.verify(&copy).unwrap();
    std::fs::write(source.join(REQUIRED[1]), "changed").unwrap();
    assert!(manifest.verify(&source).is_err());
    manifest.verify(&copy).unwrap();
    assert!(manifest.copy_verified(&copy, &copy).is_err());
}
#[test]
fn changed_inventory_links_and_missing_injection_libraries_are_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fixture(root);
    let manifest = ProductsManifest::capture(root, "SampleApp.xctestrun").unwrap();
    std::fs::write(root.join("unexpected"), "extra").unwrap();
    assert!(manifest.verify(root).is_err());
    std::fs::remove_file(root.join("unexpected")).unwrap();
    let library = root.join(REQUIRED[2]);
    std::fs::remove_file(&library).unwrap();
    assert!(ProductsManifest::capture(root, "SampleApp.xctestrun").is_err());
    std::os::unix::fs::symlink(root.join(REQUIRED[1]), &library).unwrap();
    assert!(manifest.verify(root).is_err());
}
#[test]
fn forged_manifest_cannot_escape_destination_or_omit_required_parents() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    let manifest = ProductsManifest::capture(temp.path(), "SampleApp.xctestrun").unwrap();
    let mut forged = manifest.clone();
    forged.files.insert(
        "../outside".into(),
        manifest.files.values().next().unwrap().clone(),
    );
    assert!(forged.validate().is_err());
    let mut forged = manifest;
    forged.directories.remove("Debug-iphonesimulator");
    assert!(forged.validate().is_err());
}
#[test]
fn executable_permission_changes_do_not_pass_content_hash_verification() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    let manifest = ProductsManifest::capture(temp.path(), "SampleApp.xctestrun").unwrap();
    std::fs::set_permissions(
        temp.path().join(REQUIRED[1]),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(manifest.verify(temp.path()).is_err());
}

#[test]
fn configured_products_keep_required_control_libraries_and_copy_verification() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    let names = ProductNames {
        app: "ClientApp".into(),
        tests: "ClientAppTests".into(),
    };
    named_fixture(&source, &names);
    assert!(ProductsManifest::capture(&source, "Consumer.xctestrun").is_err());
    let manifest =
        ProductsManifest::capture_named(&source, "Consumer.xctestrun", names.clone()).unwrap();
    let copy = temp.path().join("copy");
    manifest.copy_verified(&source, &copy).unwrap();
    std::fs::remove_file(source.join("Debug-iphonesimulator/ClientApp.app/vclock.dylib")).unwrap();
    assert!(ProductsManifest::capture_named(&source, "Consumer.xctestrun", names).is_err());
    manifest.verify(&copy).unwrap();
    let mut forged = manifest;
    forged.names.app = "../outside".into();
    assert!(forged.validate().is_err());
}
pub(crate) fn named_fixture(root: &Path, names: &ProductNames) {
    for name in names.required().unwrap() {
        let path = root.join(&name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &name).unwrap();
        std::fs::set_permissions(
            &path,
            std::fs::Permissions::from_mode(if name.ends_with("Info.plist") {
                0o600
            } else {
                0o700
            }),
        )
        .unwrap();
    }
    std::fs::write(root.join("Consumer.xctestrun"), "test configuration").unwrap();
}

#[test]
fn external_runner_freezes_uninstrumented_products_without_weakening_clock_apps() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    for name in ["vclock.dylib", "ui_schedule.dylib"] {
        std::fs::remove_file(
            temp.path()
                .join("Debug-iphonesimulator/SampleApp.app")
                .join(name),
        )
        .unwrap();
    }
    assert!(ProductsManifest::capture(temp.path(), "SampleApp.xctestrun").is_err());
    let manifest = ProductsManifest::capture_role(
        temp.path(),
        "SampleApp.xctestrun",
        ProductNames::default(),
        ProductRole::ExternalRunner,
    )
    .unwrap();
    let destination = tempfile::tempdir().unwrap();
    let copy = destination.path().join("products");
    manifest.copy_verified(temp.path(), &copy).unwrap();
    manifest.verify(&copy).unwrap();

    let mut clock = manifest.clone();
    clock.role = ProductRole::ClockApp;
    assert!(clock.validate().is_err());
    std::fs::write(temp.path().join(REQUIRED[7]), "changed runner").unwrap();
    assert!(manifest.verify(temp.path()).is_err());
}

#[test]
fn interrupted_avd_preparation_preserves_source_and_never_publishes_a_receipt() {
    // Losing authority or reaching the deadline after a partial large file must stop copying,
    // even without another file boundary. The group retains ownership of that partial tree.
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source");
    std::fs::create_dir_all(source.join("device.avd")).unwrap();
    std::fs::write(source.join("device.ini"), "path=/original/device.avd\n").unwrap();
    std::fs::write(source.join("device.avd/config.ini"), "disk.dataPartition.path=<temp>\n").unwrap();
    std::fs::write(source.join("device.avd/data.img"), vec![17; 1024 * 1024]).unwrap();
    let inventory = BundleInventory::capture(&source).unwrap();
    for kind in [io::ErrorKind::Interrupted, io::ErrorKind::TimedOut] {
        let owned = tempfile::tempdir_in(temp.path()).unwrap();
        let evidence = tempfile::tempdir_in(temp.path()).unwrap();
        let partial = owned.path().join("avd/device.avd/data.img");
        let error = crate::avd_copy::prepare(&source, "device", &inventory,
            owned.path(), evidence.path(), &mut || {
                if std::fs::metadata(&partial).is_ok_and(|m| m.len() > 0) {
                    Err(io::Error::new(kind, "preparation no longer authorized"))
                } else { Ok(()) }
            }).unwrap_err();
        assert_eq!(error.kind(), kind);
        let copied = std::fs::metadata(&partial).unwrap().len();
        assert!(copied > 0 && copied < inventory.files["device.avd/data.img"].bytes);
        assert!(!evidence.path().join("avd-copy.json").exists());
        inventory.verify(&source).unwrap();
        // Existing cleanup owns the scratch tree; the interrupted copier does not remove source.
        owned.close().unwrap();
        assert!(!partial.exists());
    }
    let copy = temp.path().join("complete");
    inventory.copy_verified(&source, &copy).unwrap();
    inventory.verify(&copy).unwrap();
}

#[test]
fn declared_product_payload_is_admitted_as_a_batch_before_destination_creation() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    let root=tempfile::tempdir().unwrap();let source=root.path().join("source");std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("first"),b"first input").unwrap();std::fs::write(source.join("second"),b"second input").unwrap();
    let inventory=BundleInventory::capture(&source).unwrap();let size=inventory.files.values().map(|f|f.bytes).sum::<u64>();
    let small=WriteBudget::create(&root.path().join("small"),LaunchId::new("g-small").unwrap(),
        Limits {ordinary:size-1,retirement:4096}).unwrap();
    let destination=root.path().join("copy");
    assert!(copy_files_admitted(&source,&destination,&inventory.directories,&inventory.files,&mut ||Ok(()),Some(&small)).is_err());
    assert!(!destination.exists());assert_eq!(small.charged().unwrap(),Limits {ordinary:0,retirement:0});
    let enough=WriteBudget::create(&root.path().join("enough"),LaunchId::new("g-enough").unwrap(),
        Limits {ordinary:size,retirement:4096}).unwrap();
    copy_files_admitted(&source,&destination,&inventory.directories,&inventory.files,&mut ||Ok(()),Some(&enough)).unwrap();
    inventory.verify(&destination).unwrap();inventory.verify(&source).unwrap();
    assert_eq!(enough.charged().unwrap(),Limits {ordinary:size,retirement:0});
    assert!(copy_files_admitted(&source,&destination,&inventory.directories,&inventory.files,&mut ||Ok(()),Some(&enough)).is_err());
    assert_eq!(enough.charged().unwrap().ordinary,size);
}

#[test]
fn growing_product_cannot_write_past_admitted_bytes_and_failure_keeps_charge() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    use std::io::Write;
    let root=tempfile::tempdir().unwrap();let source=root.path().join("source");std::fs::create_dir(&source).unwrap();
    let input=source.join("payload");std::fs::write(&input,vec![7;131072]).unwrap();
    let inventory=BundleInventory::capture(&source).unwrap();let destination=root.path().join("copy");
    let budget=WriteBudget::create(&root.path().join("budget"),LaunchId::new("g-growth").unwrap(),
        Limits {ordinary:131072,retirement:4096}).unwrap();
    let mut grown=false;
    let mut check=|| {
        if !grown && destination.join("payload").metadata().is_ok_and(|m|m.len()>0) {
            std::fs::OpenOptions::new().append(true).open(&input)?.write_all(b"unexpected growth")?;grown=true;
        }
        Ok(())
    };
    let error=copy_files_admitted(&source,&destination,&inventory.directories,&inventory.files,&mut check,Some(&budget)).unwrap_err();
    assert!(grown);assert!(error.to_string().contains("grew beyond admitted"));
    assert_eq!(destination.join("payload").metadata().unwrap().len(),131072);
    assert_eq!(budget.charged().unwrap(),Limits {ordinary:131072,retirement:0});
    assert!(inventory.verify(&source).is_err());
}

#[test]
fn bundle_mismatch_names_the_first_difference_without_dumping_inventory() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("Info.plist"), b"original").unwrap();
    let original = BundleInventory::capture(root.path()).unwrap();
    std::fs::write(root.path().join("Info.plist"), b"changed").unwrap();
    let error = original.verify(root.path()).unwrap_err().to_string();
    assert!(error.contains("changed file") && error.contains("Info.plist"));
    std::fs::remove_file(root.path().join("Info.plist")).unwrap();
    assert!(original.verify(root.path()).unwrap_err().to_string().contains("missing file"));
    std::fs::write(root.path().join("Info.plist"), b"original").unwrap();
    std::fs::write(root.path().join("extra"), b"extra").unwrap();
    assert!(original.verify(root.path()).unwrap_err().to_string().contains("unexpected file"));
    std::fs::remove_file(root.path().join("extra")).unwrap();
    original.verify(root.path()).unwrap();
}
