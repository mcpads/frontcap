use super::*;

#[test]
fn external_scratch_retains_evidence_outside_and_only_removes_its_child() {
    let evidence = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let unrelated = storage.path().join("unrelated");
    fs::write(&unrelated, b"keep").unwrap();
    let owned = TemporaryDisks::create(evidence.path(), Some(storage.path())).unwrap();
    assert!(owned.path.starts_with(storage.path().canonicalize().unwrap()));
    fs::write(owned.path.join("overlay"), b"temporary disk").unwrap();
    assert!(evidence.path().join("emulator-temporary.json").exists());
    assert!(!evidence.path().join("emulator-temporary").exists());
    owned.retire().unwrap();
    assert!(!owned.path.exists());
    assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
    let receipt: serde_json::Value = serde_json::from_slice(
        &fs::read(evidence.path().join("emulator-temporary-cleanup.json")).unwrap()).unwrap();
    assert_eq!(receipt["removed"], true);
    assert_eq!(receipt["entries"][0]["path"], "overlay");
}

#[test]
fn scratch_replacement_and_symlink_roots_do_not_grant_cleanup_authority() {
    let evidence = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let alias = evidence.path().join("alias");
    std::os::unix::fs::symlink(storage.path(), &alias).unwrap();
    assert!(TemporaryDisks::create(evidence.path(), Some(&alias)).is_err());
    let owned = TemporaryDisks::create(evidence.path(), Some(storage.path())).unwrap();
    let original = storage.path().join("original");
    fs::rename(&owned.path, &original).unwrap();
    fs::create_dir(&owned.path).unwrap();
    fs::write(owned.path.join("unrelated"), b"keep").unwrap();
    assert!(owned.retire().is_err());
    assert!(original.is_dir());
    assert_eq!(fs::read(owned.path.join("unrelated")).unwrap(), b"keep");
    assert!(!evidence.path().join("emulator-temporary-cleanup.json").exists());
}

#[test]
fn default_scratch_layout_keeps_original_retirement_location() {
    let evidence = tempfile::tempdir().unwrap();
    let owned = TemporaryDisks::create(evidence.path(), None).unwrap();
    assert_eq!(owned.path, evidence.path().join("emulator-temporary"));
    owned.retire().unwrap();
    assert!(evidence.path().join("emulator-temporary-cleanup.json").exists());
}
