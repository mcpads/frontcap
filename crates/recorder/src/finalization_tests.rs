use super::*;
use frontcap_core::{ids::{RequestId, RunId}, process::ProcessIdentity};

struct Fixture {
    child: std::process::Child,
    _tmp: tempfile::TempDir,
    dir: PathBuf,
    bundle: Bundle,
    id: LaunchId,
}
impl Drop for Fixture {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}
impl Fixture {
    fn new() -> Self {
        let child = std::process::Command::new("sleep").arg("120").spawn().unwrap();
        let browser = ProcessIdentity::of(child.id()).unwrap().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("runtime");
        std::fs::create_dir_all(&dir).unwrap();
        let id = LaunchId::new("g-finalize").unwrap();
        let run = RunId::new("run-finalize").unwrap();
        let runs = tmp.path().join("runs");
        let profile = Default::default();
        runtime::save_spec(&dir, &runtime::LaunchSpec {
            request_id: RequestId::new("launch").unwrap(), launch_id: id.clone(), run_id: run.clone(),
            url: "about:blank".into(), profile, rules: Default::default(), passthrough_origins: None,
            watchdog_ms: 20_000, repo_root: tmp.path().to_path_buf(), runs_root: runs.clone(), origin: None, restore: None,
        }).unwrap();
        runtime::save_runtime(&dir, &runtime::RuntimeRecord {
            launch_id: id.clone(), state: RuntimeState::Failed { reason: "original failure".into(), error: None },
            recorder: ProcessIdentity::current(), browser: Some(browser), endpoint: None, socket: dir.join("s"),
        }).unwrap();
        frontcap_core::state::save_atomic(&runtime::control_path(&dir), &ControlState::default()).unwrap();
        std::fs::write(runtime::journal_path(&dir), b"unfinished journal tail").unwrap();
        let bundle = Bundle::create(&runs, &run).unwrap();
        frontcap_core::state::save_atomic(&bundle.root().join("execution.json"), &ExecutionMetadata {
            run, launch: id.clone(), engine: "captured browser product".into(), engine_revision: "captured revision".into(),
            profile: Default::default(),
        }).unwrap();
        std::fs::write(bundle.trace(), b"partial trace tail").unwrap();
        Self { child, _tmp: tmp, dir, bundle, id }
    }
    fn stop(&mut self) { self.child.kill().unwrap(); self.child.wait().unwrap(); }
}

#[test]
fn offline_finalization_preserves_failed_evidence_and_retries_without_rewriting() {
    let mut f = Fixture::new();
    assert!(finalize(&f.dir, &f.id).unwrap_err().contains("still alive"));
    assert!(!f.bundle.manifest_path().exists());
    f.stop();
    let runtime_before = std::fs::read(runtime::runtime_path(&f.dir)).unwrap();
    // A filesystem error cannot publish success. Removing the obstruction allows the same operation.
    std::fs::create_dir(f.bundle.manifest_path()).unwrap();
    assert!(finalize(&f.dir, &f.id).is_err());
    let state: Finalization = frontcap_core::state::load(&f.dir.join("finalization.json")).unwrap().unwrap();
    assert!(matches!(state, Finalization::Failed { .. }));
    std::fs::remove_dir(f.bundle.manifest_path()).unwrap();
    let manifest = finalize(&f.dir, &f.id).unwrap();
    assert_eq!(manifest.finalization, Finalization::Finalized);
    assert!(!manifest.replayable);
    assert_eq!(manifest.determinism.level, Level::None);
    assert_eq!(manifest.engine, "captured browser product");
    assert!(manifest.determinism.taints.iter().any(|t| t.detail == "original failure"));
    assert_eq!(std::fs::read(f.bundle.trace()).unwrap(), b"partial trace tail");
    assert_eq!(std::fs::read(f.bundle.root().join(manifest.recovery_snapshot.as_ref().unwrap()).join("journal.jsonl")).unwrap(), b"unfinished journal tail");
    assert_eq!(std::fs::read(runtime::runtime_path(&f.dir)).unwrap(), runtime_before);
    let bytes = std::fs::read(f.bundle.manifest_path()).unwrap();
    assert_eq!(finalize(&f.dir, &f.id).unwrap(), manifest);
    assert_eq!(std::fs::read(f.bundle.manifest_path()).unwrap(), bytes);
    std::fs::write(f.bundle.trace(), b"changed evidence").unwrap();
    assert!(finalize(&f.dir, &f.id).unwrap_err().contains("integrity"));
    assert_eq!(std::fs::read(f.bundle.manifest_path()).unwrap(), bytes);
}

#[test]
fn offline_finalization_requires_original_identity_and_parseable_complete_evidence() {
    let mut f = Fixture::new();
    f.stop();
    let path = f.bundle.root().join("execution.json");
    let bytes = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(finalize(&f.dir, &f.id).unwrap_err().contains("missing execution.json"));
    let mut metadata: ExecutionMetadata = serde_json::from_slice(&bytes).unwrap();
    metadata.launch = LaunchId::new("g-other").unwrap();
    frontcap_core::state::save_atomic(&path, &metadata).unwrap();
    assert!(finalize(&f.dir, &f.id).unwrap_err().contains("does not match"));
    std::fs::write(path, bytes).unwrap();
    std::fs::write(f.bundle.trace(), b"invalid complete event\n").unwrap();
    assert!(finalize(&f.dir, &f.id).is_err());
    assert!(!f.bundle.manifest_path().exists());
}

#[cfg(feature = "fault-injection")]
#[test]
#[ignore = "subprocess fixture invoked by offline finalization recovery"]
fn finalization_owner() {
    let dir = PathBuf::from(std::env::var_os("FRONTCAP_FINALIZATION_DIR").unwrap());
    let result = finalize(&dir, &LaunchId::new("g-finalize").unwrap());
    if std::env::var_os("FRONTCAP_EXPECT_STORAGE_ERROR").is_some() {
        let reason = result.unwrap_err();
        assert!(reason.contains("os error 28"), "{reason}");
    } else { result.unwrap(); }
}

#[cfg(feature = "fault-injection")]
#[test]
fn offline_finalization_recovers_process_loss_and_keeps_changed_attempts() {
    for point in ["finalize_after_runtime.json", "finalize_after_control.json", "finalize_after_journal.jsonl",
        "finalize_before_manifest", "finalize_after_manifest", "finalize_after_status"] {
        let mut f = Fixture::new();
        f.stop();
        let original = std::fs::read(runtime::runtime_path(&f.dir)).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "finalization::tests::finalization_owner", "--nocapture"])
            .env("FRONTCAP_FINALIZATION_DIR", &f.dir)
            .env("FRONTCAP_FAULT", format!("g-finalize@{point}")).output().unwrap();
        assert!(!result.status.success(), "fault did not interrupt {point}");
        let published = matches!(point, "finalize_after_manifest" | "finalize_after_status");
        assert_eq!(f.bundle.manifest_path().exists(), published, "{point}");
        let before = std::fs::read(f.bundle.manifest_path()).ok();
        let changed = point == "finalize_before_manifest";
        if changed {
            std::fs::write(runtime::journal_path(&f.dir), b"unfinished journal tail plus later evidence").unwrap();
        }
        let m = finalize(&f.dir, &f.id).unwrap();
        assert_eq!(std::fs::read(runtime::runtime_path(&f.dir)).unwrap(), original);
        if let Some(before) = before { assert_eq!(std::fs::read(f.bundle.manifest_path()).unwrap(), before); }
        let selected = f.bundle.root().join(m.recovery_snapshot.as_ref().unwrap());
        assert_eq!(std::fs::read(selected.join("journal.jsonl")).unwrap(), std::fs::read(runtime::journal_path(&f.dir)).unwrap());
        let groups: Vec<_> = std::fs::read_dir(f.bundle.root().join("recovery")).unwrap().map(|e| e.unwrap().path()).collect();
        assert_eq!(groups.len(), if changed { 2 } else { 1 });
        if changed {
            let previous = groups.into_iter().find(|p| *p != selected).unwrap();
            assert_eq!(std::fs::read(previous.join("journal.jsonl")).unwrap(), b"unfinished journal tail");
        }
        let inspection = f.bundle.inspect().unwrap();
        assert!(inspection.finalized && inspection.missing.is_empty() && inspection.mismatched.is_empty() && inspection.unexpected.is_empty());
        assert_eq!(finalize(&f.dir, &f.id).unwrap(), m);
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn offline_finalization_recovers_interrupted_and_failed_atomic_writes() {
    for name in ["runtime.json", "control.json", "journal.jsonl", "manifest.json", "finalization.json"] {
        for point in ["after_partial_write", "after_sync", "after_rename", "no_space_after_partial_write"] {
            let mut f = Fixture::new();
            f.stop();
            let runtime_before = std::fs::read(runtime::runtime_path(&f.dir)).unwrap();
            let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
            cmd.args(["--ignored", "--exact", "finalization::tests::finalization_owner", "--nocapture"])
                .env("FRONTCAP_FINALIZATION_DIR", &f.dir)
                .env("FRONTCAP_STATE_FAULT", format!("{name}@{point}"));
            let error = point == "no_space_after_partial_write";
            if error { cmd.env("FRONTCAP_EXPECT_STORAGE_ERROR", "1"); }
            let result = cmd.output().unwrap();
            assert_eq!(result.status.success(), error, "{name}@{point}: {}", String::from_utf8_lossy(&result.stderr));
            let published = name == "finalization.json" || (name == "manifest.json" && point == "after_rename");
            assert_eq!(f.bundle.manifest_path().exists(), published, "{name}@{point}");
            let original_manifest = std::fs::read(f.bundle.manifest_path()).ok();
            let m = finalize(&f.dir, &f.id).unwrap();
            let snapshot = f.bundle.root().join(m.recovery_snapshot.as_ref().unwrap());
            for source in ["runtime.json", "control.json", "journal.jsonl"] {
                assert_eq!(std::fs::read(snapshot.join(source)).unwrap(), std::fs::read(f.dir.join(source)).unwrap());
            }
            if name != "finalization.json" && matches!(point, "after_partial_write" | "after_sync") {
                assert!(m.missing_inputs.iter().any(|s| s.contains("unpublished atomic write")), "unpublished bytes not classified: {name}@{point}");
            }
            if let Some(bytes) = original_manifest { assert_eq!(std::fs::read(f.bundle.manifest_path()).unwrap(), bytes); }
            assert_eq!(std::fs::read(runtime::runtime_path(&f.dir)).unwrap(), runtime_before);
            let state: Finalization = frontcap_core::state::load(&f.dir.join("finalization.json")).unwrap().unwrap();
            assert_eq!(state, Finalization::Finalized);
            let inspection = f.bundle.inspect().unwrap();
            assert!(inspection.finalized && inspection.missing.is_empty() && inspection.mismatched.is_empty() && inspection.unexpected.is_empty());
        }
    }
}

#[cfg(unix)]
#[test]
fn offline_finalization_after_owner_release_does_not_wait_for_inherited_descriptors() {
    use std::{io::Write, os::{fd::AsRawFd, unix::net::UnixStream}, time::Duration};
    let mut f = Fixture::new();
    f.stop();
    let owner = crate::take_lock(&f.dir).unwrap().unwrap();
    let (reader, writer) = UnixStream::pair().unwrap();
    reader.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let read_fd = reader.as_raw_fd(); let write_fd = writer.as_raw_fd();
    let pid = unsafe { libc::fork() };
    assert!(pid >= 0);
    if pid == 0 {
        unsafe {
            // No Rust allocation/destruction in the forked multithreaded test process.
            libc::close(write_fd);
            let mut byte = 0u8;
            libc::read(read_fd, (&mut byte as *mut u8).cast(), 1);
            libc::_exit(0);
        }
    }
    drop(reader);
    struct Fork { pid: libc::pid_t, writer: UnixStream }
    impl Drop for Fork {
        fn drop(&mut self) {
            let _ = self.writer.write_all(&[1]);
            loop {
                let waited = unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), 0) };
                if waited >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted { break; }
            }
        }
    }
    let inherited = Fork { pid, writer };
    // A live owner still excludes offline mutation; no finalization record may be written.
    assert!(finalize(&f.dir, &f.id).unwrap_err().contains("still owns"));
    assert!(!f.dir.join("finalization.json").exists());
    drop(owner);
    let result = finalize(&f.dir, &f.id);
    drop(inherited);
    assert_eq!(result.unwrap().finalization, Finalization::Finalized);
    assert_eq!(std::fs::read(f.bundle.trace()).unwrap(), b"partial trace tail");
}
