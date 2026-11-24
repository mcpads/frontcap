use super::*;
use std::io::Write;

#[test]
fn sha_match_is_accepted() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(b"frontcap").unwrap();
    let expected = sha256_hex(f.path()).unwrap();
    verify_sha256(f.path(), &expected).unwrap();
}

#[test]
fn sha_mismatch_is_rejected() {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(b"frontcap").unwrap();
    let wrong = "0".repeat(64);
    assert!(matches!(verify_sha256(f.path(), &wrong), Err(InstallError::ShaMismatch { .. })));
}

#[test]
fn binary_path_layout() {
    let p = binary_path(std::path::Path::new("/r"), "154.0.8037.92", "mac-arm64");
    assert_eq!(
        p,
        std::path::Path::new(
            "/r/browsers/154.0.8037.92/mac-arm64/chrome-headless-shell-mac-arm64/chrome-headless-shell"
        )
    );
}

fn fake_pin(url: String, sha256: String) -> crate::Pin {
    let mut artifacts = std::collections::BTreeMap::new();
    artifacts.insert("mac-arm64".to_string(), crate::Artifact { url, sha256 });
    crate::Pin { version: "0.0.0".into(), revision: 0, artifacts }
}

/// A zip containing the expected binary path, written to `dir/a.zip`.
fn write_zip(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("a.zip");
    let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    zip.start_file("chrome-headless-shell-mac-arm64/chrome-headless-shell", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"#!/bin/sh\n").unwrap();
    zip.finish().unwrap();
    path
}

#[test]
fn partial_install_is_not_trusted() {
    let root = tempfile::tempdir().unwrap();
    // Simulate an interrupted extraction: the binary exists but the install was never completed.
    let bin = binary_path(root.path(), "0.0.0", "mac-arm64");
    std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, b"partial").unwrap();
    let src = tempfile::tempdir().unwrap();
    let zip = write_zip(src.path());
    let pin = fake_pin(format!("file://{}", zip.display()), sha256_hex(&zip).unwrap());
    let installed = install(root.path(), &pin, "mac-arm64").unwrap();
    assert_eq!(std::fs::read(&installed).unwrap(), b"#!/bin/sh\n");
}

#[test]
fn install_with_wrong_sha_extracts_nothing() {
    let root = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let zip = write_zip(src.path());
    let pin = fake_pin(format!("file://{}", zip.display()), "0".repeat(64));
    assert!(matches!(install(root.path(), &pin, "mac-arm64"), Err(InstallError::ShaMismatch { .. })));
    assert!(!binary_path(root.path(), "0.0.0", "mac-arm64").exists());
}
