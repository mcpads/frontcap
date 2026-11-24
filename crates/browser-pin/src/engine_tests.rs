use super::*;
use std::io::Write as _;

fn pin(version: &str) -> String {
    format!("version = \"{version}\"\nrevision = 1\n\n[artifacts.mac-arm64]\nurl = \"https://example.invalid/a.zip\"\nsha256 = \"{}\"\n", "0".repeat(64))
}

fn repo(patches: &[(&str, &str)], args: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("browsers.toml"), pin("1.2.3.4")).unwrap();
    std::fs::create_dir_all(root.path().join("engine/patches")).unwrap();
    std::fs::write(root.path().join("engine/args.gn"), args).unwrap();
    for (name, body) in patches {
        std::fs::write(root.path().join("engine/patches").join(name), body).unwrap();
    }
    root
}

#[test]
fn the_patch_set_identity_follows_every_patch_byte_name_and_the_build_arguments() {
    let id = |p: &[(&str, &str)], a: &str| patchset_id(repo(p, a).path()).unwrap();
    let base = id(&[("0001-a.patch", "x"), ("0002-b.patch", "y")], "is_debug = false\n");
    assert_eq!(base, id(&[("0002-b.patch", "y"), ("0001-a.patch", "x")], "is_debug = false\n"), "not the listing order");
    assert_ne!(base, id(&[("0001-a.patch", "x"), ("0002-b.patch", "z")], "is_debug = false\n"));
    assert_ne!(base, id(&[("0001-a.patch", "x"), ("0003-b.patch", "y")], "is_debug = false\n"));
    assert_ne!(base, id(&[("0001-a.patch", "xy"), ("0002-b.patch", "")], "is_debug = false\n"), "boundaries count");
    assert_ne!(base, id(&[("0001-a.patch", "x"), ("0002-b.patch", "y")], "is_debug = true\n"));
    // Files that are not patches are not part of it.
    let r = repo(&[("0001-a.patch", "x"), ("0002-b.patch", "y")], "is_debug = false\n");
    std::fs::write(r.path().join("engine/patches/README"), "notes").unwrap();
    assert_eq!(patchset_id(r.path()).unwrap(), base);
    // A new base is a new patch set: the same patches build another engine.
    std::fs::write(r.path().join("browsers.toml"), pin("1.2.3.5")).unwrap();
    assert_ne!(patchset_id(r.path()).unwrap(), base);
}

#[test]
fn a_build_for_another_platform_names_its_target_and_the_hosts_own_adds_nothing() {
    assert_eq!(target_args("mac-arm64", "mac-arm64").unwrap(), "");
    assert_eq!(target_args("linux-arm64", "linux-arm64").unwrap(), "");
    let cross = target_args("linux64", "linux-arm64").unwrap();
    assert!(cross.contains("target_os = \"linux\"\n") && cross.contains("target_cpu = \"arm64\"\n"), "{cross}");
    assert!(target_args("mac-x64", "mac-arm64").unwrap().contains("target_cpu = \"arm64\""));
    assert!(matches!(target_args("linux64", "win64"), Err(EngineError::UnknownPlatform(p)) if p == "win64"));
}

#[test]
fn a_tree_without_a_patch_set_has_no_patched_build() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("browsers.toml"), pin("1.2.3.4")).unwrap();
    assert!(patched_binary(root.path(), "mac-arm64").unwrap().is_none());
}

#[test]
fn a_build_is_used_only_for_its_patch_set_and_only_when_complete() {
    let r = repo(&[("0001-a.patch", "x")], "");
    let id = patchset_id(r.path()).unwrap();
    let dir = patched_dir(r.path(), &id, "mac-arm64");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("headless_shell"), "bin").unwrap();
    assert!(patched_binary(r.path(), "mac-arm64").unwrap().is_none(), "no manifest: not a build");
    let digest = crate::install::sha256_hex(&dir.join("headless_shell")).unwrap();
    let manifest = Manifest { base: "1.2.3.4".into(), patchset: id.clone(), platform: "mac-arm64".into(), binary: "headless_shell".into(), files: [("headless_shell".to_string(), digest)].into() };
    std::fs::write(dir.join(MANIFEST), serde_json::to_string(&manifest).unwrap()).unwrap();
    let (bin, m) = patched_binary(r.path(), "mac-arm64").unwrap().unwrap();
    assert_eq!((bin, m), (dir.join("headless_shell"), manifest.clone()));
    std::fs::write(dir.join(MANIFEST), serde_json::to_string(&Manifest { base: "1.2.3.3".into(), ..manifest.clone() }).unwrap()).unwrap();
    assert!(patched_binary(r.path(), "mac-arm64").unwrap().is_none(), "a build of another base is not this pin's");
    std::fs::write(dir.join(MANIFEST), serde_json::to_string(&manifest).unwrap()).unwrap();
    std::fs::write(r.path().join("engine/patches/0001-a.patch"), "changed").unwrap();
    assert!(patched_binary(r.path(), "mac-arm64").unwrap().is_none(), "a changed patch set needs its own build");
}

#[test]
fn a_checkout_not_at_the_base_or_with_changes_is_refused_untouched() {
    let r = repo(&[], "");
    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("chrome")).unwrap();
    std::fs::write(src.path().join("chrome/VERSION"), "MAJOR=154\nMINOR=0\nBUILD=8037\nPATCH=91\n").unwrap();
    assert!(matches!(build(r.path(), src.path(), "154.0.8037.92", "mac-arm64"), Err(EngineError::WrongBase { .. })));
    std::fs::write(src.path().join("chrome/VERSION"), "MAJOR=154\nMINOR=0\nBUILD=8037\nPATCH=92\n").unwrap();
    let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(src.path()).args(args).status().unwrap().success());
    git(&["init", "-q"]);
    git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "base"]);
    git(&["add", "chrome/VERSION"]);
    std::fs::write(src.path().join("notes.txt"), "the user's").unwrap();
    assert!(matches!(build(r.path(), src.path(), "154.0.8037.92", "mac-arm64"), Err(EngineError::Dirty(_))));
    assert!(src.path().join("chrome/VERSION").exists(), "a refused build changes nothing");
    assert!(src.path().join("notes.txt").exists(), "nor any untracked file");
}

/// A build is accepted only as its manifest describes it: each file present with its digest, the executable
/// among them, for this platform.
#[test]
fn a_changed_incomplete_or_foreign_build_is_refused() {
    let r = repo(&[("0001-a.patch", "x")], "");
    let id = patchset_id(r.path()).unwrap();
    let dir = patched_dir(r.path(), &id, "mac-arm64");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("headless_shell"), "bin").unwrap();
    std::fs::write(dir.join("resources.pak"), "pak").unwrap();
    let digest = |n: &str| crate::install::sha256_hex(&dir.join(n)).unwrap();
    let good = Manifest {
        base: "1.2.3.4".into(),
        patchset: id.clone(),
        platform: "mac-arm64".into(),
        binary: "headless_shell".into(),
        files: [("headless_shell".to_string(), digest("headless_shell")), ("resources.pak".to_string(), digest("resources.pak"))].into(),
    };
    let write = |m: &Manifest| std::fs::write(dir.join(MANIFEST), serde_json::to_string(m).unwrap()).unwrap();
    write(&good);
    assert!(patched_binary(r.path(), "mac-arm64").unwrap().is_some());
    let refused = || matches!(patched_binary(r.path(), "mac-arm64"), Err(EngineError::Corrupt { .. }));
    write(&Manifest { platform: "linux-x64".into(), ..good.clone() });
    assert!(refused(), "another platform's");
    write(&Manifest { files: [("resources.pak".to_string(), digest("resources.pak"))].into(), ..good.clone() });
    assert!(refused(), "the executable unlisted");
    write(&good);
    std::fs::write(dir.join("resources.pak"), "changed").unwrap();
    assert!(refused(), "a changed resource");
    std::fs::write(dir.join("resources.pak"), "pak").unwrap();
    assert!(patched_binary(r.path(), "mac-arm64").unwrap().is_some());
    std::fs::write(dir.join("headless_shell"), "other").unwrap();
    assert!(refused(), "a changed executable");
    std::fs::remove_file(dir.join("resources.pak")).unwrap();
    assert!(refused(), "a missing resource");
}

/// A checkout of the pinned base with one tracked file.
fn checkout() -> tempfile::TempDir {
    let src = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(src.path().join("chrome")).unwrap();
    std::fs::write(src.path().join("chrome/VERSION"), "MAJOR=1\nMINOR=2\nBUILD=3\nPATCH=4\n").unwrap();
    std::fs::write(src.path().join("user.cc"), "original\n").unwrap();
    let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(src.path()).args(args).status().unwrap().success());
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "base"]);
    src
}

/// An interrupted build's marker covers only the changes it applied: edits made since are refused rather
/// than discarded, and its own changes alone are reverted.
#[test]
fn an_interrupted_build_never_discards_edits_made_since() {
    let r = repo(&[("0001-a.patch", "not a patch")], "");
    let src = checkout();
    // An external diff tool that prints nothing would make every checkout look alike.
    let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(src.path()).args(args).status().unwrap().success());
    git(&["config", "diff.external", "true"]);
    // An interrupted build: one change applied and recorded, then a developer's edit.
    std::fs::write(src.path().join("user.cc"), "applied by the build\n").unwrap();
    std::fs::create_dir_all(src.path().join("out/frontcap")).unwrap();
    mark_applied(src.path()).unwrap();
    std::fs::write(src.path().join("user.cc"), "the developer's work\n").unwrap();
    assert!(matches!(build(r.path(), src.path(), "1.2.3.4", "mac-arm64"), Err(EngineError::Mixed(_))));
    assert_eq!(std::fs::read_to_string(src.path().join("user.cc")).unwrap(), "the developer's work\n");
    // With only the build's own change left, it is reverted, and the (invalid) patch then fails.
    std::fs::write(src.path().join("user.cc"), "applied by the build\n").unwrap();
    assert!(matches!(build(r.path(), src.path(), "1.2.3.4", "mac-arm64"), Err(EngineError::Step { .. })));
    assert_eq!(std::fs::read_to_string(src.path().join("user.cc")).unwrap(), "original\n");
}

fn mtime(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

fn set_mtime(path: &Path, t: std::time::SystemTime) {
    std::fs::File::options().write(true).open(path).unwrap().set_modified(t).unwrap();
}

/// A checkout after a build: `user.cc` patched, built at `built`, with a build log.
fn built_tree() -> (tempfile::TempDir, std::time::SystemTime) {
    let src = checkout();
    std::fs::create_dir_all(src.path().join("out/frontcap")).unwrap();
    std::fs::write(src.path().join("out/frontcap/.ninja_log"), "# ninja log v6\n1\t2\t0\tobj/user.o\tab\n").unwrap();
    std::fs::write(src.path().join("user.cc"), "patched\n").unwrap();
    let built = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    set_mtime(&src.path().join("user.cc"), built);
    (src, built)
}

/// The checkout is reset after a build and patched again before the next: a file whose content is what
/// the last build compiled gets back the time it had then, so the build does not redo its work.
#[test]
fn a_reapplied_unchanged_file_gets_back_the_time_it_was_built_at() {
    let (src, built) = built_tree();
    record_tree(src.path()).unwrap();
    std::fs::write(src.path().join("user.cc"), "original\n").unwrap();
    std::fs::write(src.path().join("user.cc"), "patched\n").unwrap();
    assert_ne!(mtime(&src.path().join("user.cc")), built);
    assert_eq!(restore_tree(src.path()).unwrap(), 1);
    assert_eq!(mtime(&src.path().join("user.cc")), built);
}

/// A file whose content differs from what was built keeps its new time, so it is rebuilt.
#[test]
fn a_changed_file_keeps_its_new_time() {
    let (src, built) = built_tree();
    record_tree(src.path()).unwrap();
    std::fs::write(src.path().join("user.cc"), "patched differently\n").unwrap();
    let fresh = mtime(&src.path().join("user.cc"));
    assert_eq!(restore_tree(src.path()).unwrap(), 0);
    assert_eq!(mtime(&src.path().join("user.cc")), fresh);
    assert_ne!(fresh, built);
}

/// Any build since the record (an in-tree debug build of other content, say) voids it: restoring times
/// then could make objects of that other content look current.
#[test]
fn a_build_since_the_record_voids_it() {
    let (src, _) = built_tree();
    record_tree(src.path()).unwrap();
    std::fs::OpenOptions::new().append(true).open(src.path().join("out/frontcap/.ninja_log")).unwrap()
        .write_all(b"3\t4\t0\tobj/user.o\tcd\n").unwrap();
    std::fs::write(src.path().join("user.cc"), "patched\n").unwrap();
    assert_eq!(restore_tree(src.path()).unwrap(), 0);
}

/// A record is used once: a build that is interrupted after restoring must not leave it for the next.
#[test]
fn a_record_is_used_once() {
    let (src, built) = built_tree();
    record_tree(src.path()).unwrap();
    std::fs::write(src.path().join("user.cc"), "patched\n").unwrap();
    assert_eq!(restore_tree(src.path()).unwrap(), 1);
    std::fs::write(src.path().join("user.cc"), "patched\n").unwrap();
    assert_eq!(restore_tree(src.path()).unwrap(), 0, "no record left");
    assert_ne!(mtime(&src.path().join("user.cc")), built);
}

/// Failed compilation must still return failure, while retaining reusable input times.
#[cfg(unix)]
#[test]
fn a_failed_build_preserves_identical_inputs_without_becoming_successful() {
    let (src, built) = built_tree();
    let result = run_build(src.path(), "compile", Command::new("sh").args(["-c", "exit 1"]));
    assert!(matches!(result, Err(EngineError::Step { .. })));
    std::fs::write(src.path().join("user.cc"), "patched\n").unwrap();
    assert_eq!(restore_tree(src.path()).unwrap(), 1);
    assert_eq!(mtime(&src.path().join("user.cc")), built);
}

#[cfg(unix)]
#[test]
fn a_missing_build_tool_preserves_reusable_inputs_and_reports_failure() {
    let (src, built) = built_tree();
    let result = run_build(src.path(), "gn gen", &mut Command::new(src.path().join("missing-gn")));
    assert!(matches!(result, Err(EngineError::Step { step, .. }) if step == "gn gen"));
    std::fs::write(src.path().join("user.cc"), "patched\n").unwrap();
    assert_eq!(restore_tree(src.path()).unwrap(), 1);
    assert_eq!(mtime(&src.path().join("user.cc")), built);
}

#[cfg(unix)]
#[test]
fn initial_generation_needs_no_previous_ninja_log() {
    let src = checkout();
    run_build(src.path(), "gn gen", Command::new("sh").args(["-c", "exit 0"])).unwrap();
    assert!(!src.path().join(TREE).exists());
}

#[cfg(unix)]
#[test]
fn a_signaled_build_does_not_publish_input_times() {
    let (src, _) = built_tree();
    run_build(src.path(), "gn gen", Command::new("sh").args(["-c", "exit 0"])).unwrap();
    assert!(src.path().join(TREE).exists());
    let result = run_build(src.path(), "compile", Command::new("sh").args(["-c", "kill -TERM $$"]));
    assert!(result.is_err());
    assert!(!src.path().join(TREE).exists());
}

const SOURCE_PATCH: &str = "diff --git a/user.cc b/user.cc\n--- a/user.cc\n+++ b/user.cc\n@@ -1 +1 @@\n-original\n+patched\n";

fn checkout_with_v8() -> tempfile::TempDir {
    let src = checkout();
    let v8 = checkout();
    std::fs::rename(v8.path(), src.path().join("v8")).unwrap();
    src
}

fn configure_v8(root: &Path, src: &Path, patch: &str) {
    let revision = run("V8 test commit", Command::new("git").arg("-C").arg(src.join("v8")).args(["rev-parse", "HEAD"])).unwrap();
    let dir = root.join("engine/v8");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("base-revision"), revision).unwrap();
    std::fs::write(dir.join("frontcap.patch"), patch).unwrap();
}

#[test]
fn v8_identity_includes_the_commit_patch_name_and_content() {
    let r = repo(&[], "");
    let src = checkout_with_v8();
    let original = patchset_id(r.path()).unwrap();
    configure_v8(r.path(), src.path(), SOURCE_PATCH);
    let patched = patchset_id(r.path()).unwrap();
    assert_ne!(original, patched);
    let dir = r.path().join("engine/v8");
    std::fs::write(dir.join("frontcap.patch"), "different").unwrap();
    assert_ne!(patched, patchset_id(r.path()).unwrap());
    configure_v8(r.path(), src.path(), SOURCE_PATCH);
    std::fs::rename(dir.join("frontcap.patch"), dir.join("renamed.patch")).unwrap();
    assert_ne!(patched, patchset_id(r.path()).unwrap());
    std::fs::rename(dir.join("renamed.patch"), dir.join("frontcap.patch")).unwrap();
    std::fs::write(dir.join("base-revision"), "0".repeat(40)).unwrap();
    assert_ne!(patched, patchset_id(r.path()).unwrap());
    std::fs::write(dir.join("base-revision"), "main").unwrap();
    assert!(patchset_id(r.path()).is_err(), "a moving revision is not an engine input");
    std::fs::remove_dir_all(dir).unwrap();
    assert_eq!(original, patchset_id(r.path()).unwrap());
}

#[test]
fn both_checkouts_are_patched_and_restored_even_after_a_v8_patch_failure() {
    let r = repo(&[("frontcap.patch", SOURCE_PATCH)], "");
    let src = checkout_with_v8();
    configure_v8(r.path(), src.path(), SOURCE_PATCH);
    for patch in [SOURCE_PATCH, "invalid patch"] {
        std::fs::write(r.path().join("engine/v8/frontcap.patch"), patch).unwrap();
        let result = apply_patches(r.path(), src.path());
        if patch == SOURCE_PATCH {
            let applied = result.unwrap();
            for name in ["user.cc", "v8/user.cc"] {
                assert_eq!(std::fs::read_to_string(src.path().join(name)).unwrap(), "patched\n");
            }
            drop(applied);
        } else { assert!(result.is_err()); }
        for name in ["user.cc", "v8/user.cc"] {
            assert_eq!(std::fs::read_to_string(src.path().join(name)).unwrap(), "original\n");
        }
        assert!(!src.path().join(APPLIED).exists());
        assert!(!src.path().join("v8").join(APPLIED).exists());
    }
}

#[test]
fn a_v8_edit_or_wrong_commit_refuses_recovery_of_either_checkout() {
    let r = repo(&[("frontcap.patch", SOURCE_PATCH)], "");
    let src = checkout_with_v8();
    configure_v8(r.path(), src.path(), SOURCE_PATCH);
    // Simulate a killed builder without retaining its file locks.
    for dir in [src.path().to_path_buf(), src.path().join("v8")] {
        std::fs::create_dir_all(dir.join("out/frontcap")).unwrap();
        std::fs::write(dir.join("user.cc"), "patched\n").unwrap();
        mark_applied(&dir).unwrap();
    }
    std::fs::write(src.path().join("v8/user.cc"), "developer edit\n").unwrap();
    assert!(matches!(apply_patches(r.path(), src.path()), Err(EngineError::Mixed(_))));
    assert_eq!(std::fs::read_to_string(src.path().join("user.cc")).unwrap(), "patched\n");
    assert_eq!(std::fs::read_to_string(src.path().join("v8/user.cc")).unwrap(), "developer edit\n");
    std::fs::write(src.path().join("v8/user.cc"), "patched\n").unwrap();
    std::fs::write(r.path().join("engine/v8/base-revision"), "0".repeat(40)).unwrap();
    assert!(matches!(apply_patches(r.path(), src.path()), Err(EngineError::WrongBase { .. })));
    assert_eq!(std::fs::read_to_string(src.path().join("user.cc")).unwrap(), "patched\n");
    configure_v8(r.path(), src.path(), SOURCE_PATCH);
    drop(apply_patches(r.path(), src.path()).unwrap());
    assert_eq!(std::fs::read_to_string(src.path().join("v8/user.cc")).unwrap(), "original\n");
}

#[test]
fn edits_during_a_build_are_kept_in_the_affected_checkout() {
    let r = repo(&[("frontcap.patch", SOURCE_PATCH)], "");
    let src = checkout_with_v8();
    configure_v8(r.path(), src.path(), SOURCE_PATCH);
    let applied = apply_patches(r.path(), src.path()).unwrap();
    std::fs::write(src.path().join("v8/user.cc"), "developer edit\n").unwrap();
    drop(applied);
    assert_eq!(std::fs::read_to_string(src.path().join("user.cc")).unwrap(), "original\n");
    assert_eq!(std::fs::read_to_string(src.path().join("v8/user.cc")).unwrap(), "developer edit\n");
    assert!(src.path().join("v8").join(APPLIED).is_file());
}

#[test]
fn removing_v8_patches_recovers_owned_changes_but_refuses_unowned_changes() {
    let r = repo(&[], "");
    let src = checkout_with_v8();
    let v8 = src.path().join("v8");
    std::fs::write(v8.join("user.cc"), "unexpected\n").unwrap();
    assert!(matches!(apply_patches(r.path(), src.path()), Err(EngineError::Dirty(_))));
    std::fs::create_dir_all(v8.join("out/frontcap")).unwrap();
    mark_applied(&v8).unwrap();
    drop(apply_patches(r.path(), src.path()).unwrap());
    assert_eq!(std::fs::read_to_string(v8.join("user.cc")).unwrap(), "original\n");
}

#[test]
fn v8_input_times_follow_the_shared_ninja_build_log() {
    let r = repo(&[("frontcap.patch", SOURCE_PATCH)], "");
    let src = checkout_with_v8();
    configure_v8(r.path(), src.path(), SOURCE_PATCH);
    let applied = apply_patches(r.path(), src.path()).unwrap();
    let at = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
    set_mtime(&src.path().join("user.cc"), at);
    set_mtime(&src.path().join("v8/user.cc"), at);
    std::fs::write(src.path().join(BUILD_LOG), "first build\n").unwrap();
    record_tree(src.path()).unwrap();
    drop(applied);
    let applied = apply_patches(r.path(), src.path()).unwrap();
    assert_eq!(restore_tree(src.path()).unwrap(), 2);
    assert_eq!(mtime(&src.path().join("v8/user.cc")), at);
    record_tree(src.path()).unwrap();
    drop(applied);
    let _applied = apply_patches(r.path(), src.path()).unwrap();
    std::fs::write(src.path().join(BUILD_LOG), "another build\n").unwrap();
    assert_eq!(restore_tree(src.path()).unwrap(), 0);
    assert_ne!(mtime(&src.path().join("v8/user.cc")), at);
}

#[test]
fn a_v8_directory_without_its_own_repository_is_refused_before_patching() {
    let r = repo(&[("frontcap.patch", SOURCE_PATCH)], "");
    let src = checkout_with_v8();
    configure_v8(r.path(), src.path(), SOURCE_PATCH);
    std::fs::rename(src.path().join("v8/.git"), src.path().join("saved-v8-git")).unwrap();
    assert!(matches!(apply_patches(r.path(), src.path()), Err(EngineError::Step { .. })));
    assert_eq!(std::fs::read_to_string(src.path().join("user.cc")).unwrap(), "original\n");
}

#[test]
fn cleanup_removes_patch_added_v8_files_and_preserves_untracked_files() {
    let r = repo(&[], "");
    let src = checkout_with_v8();
    configure_v8(r.path(), src.path(), "diff --git a/new.cc b/new.cc\nnew file mode 100644\n--- /dev/null\n+++ b/new.cc\n@@ -0,0 +1 @@\n+added\n");
    let note = src.path().join("v8/notes.txt");
    std::fs::write(&note, "untracked notes\n").unwrap();
    let applied = apply_patches(r.path(), src.path()).unwrap();
    assert!(src.path().join("v8/new.cc").is_file());
    drop(applied);
    assert!(!src.path().join("v8/new.cc").exists());
    assert_eq!(std::fs::read_to_string(note).unwrap(), "untracked notes\n");
}
