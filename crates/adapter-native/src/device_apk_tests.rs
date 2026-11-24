use super::*;
use std::os::unix::fs::{MetadataExt, symlink};
use frontcap_core::canonical::sha256_bytes_hex;

#[test]
fn frozen_apk_is_independent_in_both_directions_and_keeps_expected_bytes() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.apk");
    let alias = root.path().join("input.apk");
    let destination = root.path().join("app.apk");
    let bytes = b"original APK bytes";
    fs::write(&source, bytes).unwrap(); symlink(&source, &alias).unwrap();
    let digest = sha256_bytes_hex(bytes);
    verify(&alias, &digest).unwrap(); snapshot(&alias, &destination, &digest).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), bytes);
    assert_ne!(source.metadata().unwrap().ino(), destination.metadata().unwrap().ino());
    assert_eq!(destination.metadata().unwrap().nlink(), 1);
    assert_eq!(destination.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    fs::write(&source, b"source edited").unwrap();
    assert_eq!(fs::read(&destination).unwrap(), bytes);
    fs::write(&destination, b"snapshot edited").unwrap();
    assert_eq!(fs::read(&source).unwrap(), b"source edited");
    assert!(!root.path().join(".apk-copy").exists());
}

#[test]
fn changed_input_or_existing_destination_cannot_publish_a_replacement() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source.apk");
    let destination = root.path().join("app.apk");
    fs::write(&source, b"original").unwrap();
    let digest = sha256_bytes_hex(b"original");
    verify(&source, &digest).unwrap();
    fs::write(&source, b"changed between admission and copy").unwrap();
    assert!(snapshot(&source, &destination, &digest).is_err());
    assert!(!destination.exists()); assert!(!root.path().join(".apk-copy").exists());
    fs::write(&destination, b"existing evidence").unwrap();
    assert!(snapshot(&source, &destination, &sha256_bytes_hex(b"changed between admission and copy")).is_err());
    assert_eq!(fs::read(&destination).unwrap(), b"existing evidence");
    fs::remove_file(&destination).unwrap(); symlink(&source, &destination).unwrap();
    assert!(snapshot(&source, &destination, &digest).is_err());
    assert!(destination.is_symlink());
}

#[test]
fn apk_payload_is_admitted_before_staging_and_spent_only_once_for_existing_evidence() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    let root=tempfile::tempdir().unwrap();let source=root.path().join("source.apk");
    let bytes=b"frozen installable input";fs::write(&source,bytes).unwrap();let digest=sha256_bytes_hex(bytes);
    let destination=root.path().join("app.apk");
    let small=WriteBudget::create(&root.path().join("small"),LaunchId::new("g-small").unwrap(),
        Limits {ordinary:bytes.len() as u64-1,retirement:4096}).unwrap();
    assert!(snapshot_with_budget(&source,&destination,&digest,Some(&small)).is_err());
    assert!(!destination.exists());assert!(!root.path().join(".apk-copy").exists());
    assert_eq!(small.charged().unwrap(),Limits {ordinary:0,retirement:0});
    let enough=WriteBudget::create(&root.path().join("enough"),LaunchId::new("g-enough").unwrap(),
        Limits {ordinary:bytes.len() as u64,retirement:4096}).unwrap();
    snapshot_with_budget(&source,&destination,&digest,Some(&enough)).unwrap();
    assert_eq!(fs::read(&destination).unwrap(),bytes);
    assert_eq!(enough.charged().unwrap(),Limits {ordinary:bytes.len() as u64,retirement:0});
    assert!(snapshot_with_budget(&source,&destination,&digest,Some(&enough)).is_err());
    assert_eq!(enough.charged().unwrap().ordinary,bytes.len() as u64);
    assert_eq!(fs::read(&source).unwrap(),bytes);
}

#[test]
fn apk_staging_failure_retains_charge_without_overwriting_unresolved_copy() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    let root=tempfile::tempdir().unwrap();let source=root.path().join("source.apk");
    fs::write(&source,b"apk").unwrap();fs::create_dir(root.path().join(".apk-copy")).unwrap();
    fs::write(root.path().join(".apk-copy/app.apk"),b"unresolved").unwrap();
    let budget=WriteBudget::create(&root.path().join("budget"),LaunchId::new("g-failure").unwrap(),
        Limits {ordinary:100,retirement:4096}).unwrap();
    assert!(snapshot_with_budget(&source,&root.path().join("app.apk"),&sha256_bytes_hex(b"apk"),Some(&budget)).is_err());
    assert_eq!(budget.charged().unwrap(),Limits {ordinary:3,retirement:0});
    assert_eq!(fs::read(root.path().join(".apk-copy/app.apk")).unwrap(),b"unresolved");
    assert!(!root.path().join("app.apk").exists());
}
