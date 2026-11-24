use super::*;

#[test]
fn the_profile_names_only_the_root_owned_engine_tree() {
    assert!(profile().contains("profile frontcap-engine /opt/frontcap/engines/*/*/{headless_shell,chrome-headless-shell-*/chrome-headless-shell} \
flags=(unconfined) {\n  userns,\n"), "{}", profile());
}

#[test]
fn a_path_a_user_can_change_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    // This test runs as an ordinary user, who owns the directory.
    assert!(matches!(check_root_owned(dir.path()), Err(SandboxError::NotRootOwned { .. })));
}

#[test]
fn an_installed_engine_replaces_its_earlier_copy_and_other_patch_sets() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("build");
    std::fs::create_dir_all(from.join("locales")).unwrap();
    std::fs::write(from.join("headless_shell"), "new").unwrap();
    std::fs::set_permissions(from.join("headless_shell"), std::fs::Permissions::from_mode(0o775)).unwrap();
    std::fs::write(from.join("locales/en-US.pak"), "pak").unwrap();
    std::fs::write(from.join("manifest.json"), "{}").unwrap();
    let engines = dir.path().join("engines");
    let old = engines.join("old/linux64");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(old.join("headless_shell"), "old").unwrap();
    let other_platform = engines.join("old/linux-arm64");
    std::fs::create_dir_all(&other_platform).unwrap();
    let to = engines.join("new/linux64");
    std::fs::create_dir_all(&to).unwrap();
    std::fs::write(to.join("stale"), "").unwrap();
    let files = ["headless_shell", "locales/en-US.pak", "manifest.json"].map(String::from);
    install_engine(&from, &files, &to).unwrap();
    assert_eq!(std::fs::read_to_string(to.join("headless_shell")).unwrap(), "new");
    assert_eq!(std::fs::metadata(to.join("headless_shell")).unwrap().permissions().mode() & 0o777, 0o755);
    assert!(to.join("locales/en-US.pak").is_file() && !to.join("stale").exists());
    assert!(!old.exists() && other_platform.exists());
    std::fs::remove_dir_all(&other_platform).unwrap();
    install_engine(&from, &files, &to).unwrap();
    assert!(!engines.join("old").exists(), "a patch set with no platform left is removed");
}

#[test]
fn a_file_name_outside_the_engine_directory_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["../escape", "/etc/passwd"] {
        let err = install_engine(dir.path(), &[name.to_string()], &dir.path().join("engines/x/linux64")).unwrap_err();
        assert!(matches!(err, SandboxError::Name(_)), "{name}: {err}");
    }
}

#[test]
fn the_pinned_browser_and_patched_engines_replace_only_their_own_kind() {
    let dir = tempfile::tempdir().unwrap();
    let from = dir.path().join("build");
    std::fs::create_dir_all(from.join("chrome-headless-shell-linux64")).unwrap();
    std::fs::write(from.join("chrome-headless-shell-linux64/chrome-headless-shell"), "base").unwrap();
    std::fs::write(from.join(".installed"), "sha").unwrap();
    let engines = dir.path().join("engines");
    let patched = engines.join("abc/linux64");
    std::fs::create_dir_all(&patched).unwrap();
    let older = engines.join("base-1.0/linux64");
    std::fs::create_dir_all(&older).unwrap();
    let to = engines.join("base-2.0/linux64");
    install_engine(&from, &files_under(&from).unwrap(), &to).unwrap();
    assert_eq!(files_under(&to).unwrap(), [".installed", "chrome-headless-shell-linux64/chrome-headless-shell"]);
    assert!(patched.exists() && !older.exists());
}
