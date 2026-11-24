use super::*;
#[test]
fn incomplete_confinement_is_refused() {
    for (volume, inventory, supervised) in [(false, true, true), (true, false, true), (true, true, false)] {
        assert!(validate(true, volume, inventory, supervised).is_err());
    }
    assert!(validate(false, false, false, false).is_ok());
    assert_eq!(validate(true, true, true, true).is_ok(), cfg!(target_os = "macos"));
}

#[test]
#[cfg(target_os = "macos")]
fn policy_allows_owned_writes_and_denies_outside_and_symlink_writes() {
    use std::os::unix::fs::symlink;
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("owned \"root\"");
    let outside = directory.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, root.join("escape")).unwrap();
    let script = "printf allowed > \"$1/inside\"; if printf forbidden > \"$2/out\"; then exit 3; fi; if printf forbidden > \"$1/escape/out\"; then exit 4; fi";
    let output = Command::new("/usr/bin/sandbox-exec").args(["-p", &policy(&root.canonicalize().unwrap()).unwrap(), "/bin/sh", "-c", script, "probe"])
        .arg(&root).arg(&outside).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(std::fs::read(root.join("inside")).unwrap(), b"allowed");
    assert!(!outside.join("out").exists());
}
