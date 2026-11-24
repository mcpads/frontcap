use super::*;

#[test]
fn binary_output_is_retained_once_and_bound_to_its_file() {
    let root = tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..=255).cycle().take(1024 * 1024).collect();
    let source = root.path().join("input");
    std::fs::write(&source, &bytes).unwrap();
    let mut command = Command::new("/bin/cat"); command.arg(&source);
    let directory = root.path().join("capture");
    let result = capture_artifact_recorded(&mut command, Instant::now()+Duration::from_secs(5),
        &AtomicBool::new(false), CaptureLimits { stdout: 2*1024*1024, stderr: 2*1024*1024 }, &directory, "screen.png").unwrap();
    assert!(result.exchange.failure.is_none(), "{result:?}");
    let path = directory.canonicalize().unwrap().join("screen.png");
    assert_eq!(result.read_stdout(&path).unwrap(), bytes);
    let record = std::fs::read(directory.join("exchange.json")).unwrap();
    assert!(record.len() < 1024, "binary payload must not enter the JSON record");
    let loaded: ArtifactExchange = serde_json::from_slice(&record).unwrap();
    assert_eq!(loaded.read_stdout(&path).unwrap(), bytes);
    assert!(loaded.read_stdout(&source).is_err());
    let mut changed = bytes.clone(); changed[0] ^= 1;
    std::fs::write(&path, changed).unwrap();
    assert!(loaded.read_stdout(&path).is_err());
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&source, &path).unwrap();
    assert!(loaded.read_stdout(&path).is_err());
}

#[test]
fn bounded_partial_output_and_failed_command_remain_evidence_not_success() {
    let root = tempfile::tempdir().unwrap();
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf 12345678901234567890; printf err >&2; exit 7"]);
    let directory = root.path().join("capture");
    let result = capture_artifact_recorded(&mut command, Instant::now()+Duration::from_secs(5),
        &AtomicBool::new(false), CaptureLimits { stdout: 10, stderr: 10 }, &directory, "screen.png").unwrap();
    assert!(result.exchange.failure.is_some());
    assert!(result.exchange.cleanup_failure.is_none());
    let bytes = result.read_stdout(&directory.canonicalize().unwrap().join("screen.png")).unwrap();
    assert!(bytes.len() <= 10);
    assert!(b"12345678901234567890".starts_with(&bytes));
    let process: frontcap_core::process::ProcessIdentity =
        frontcap_core::state::load(&directory.join("process.json")).unwrap().unwrap();
    assert!(process.has_exited().unwrap());
    let mut invalid = Command::new("/usr/bin/true");
    assert!(capture_artifact_recorded(&mut invalid, Instant::now()+Duration::from_secs(5),
        &AtomicBool::new(false), CaptureLimits { stdout: 10, stderr: 10 }, &root.path().join("invalid"), "../escape").is_err());
    assert!(!root.path().join("invalid").exists());
}

#[test]
fn worker_binary_artifacts_share_durable_parent_allowance() {
    use frontcap_core::write_budget::{Limits, WriteBudget};
    const PAYLOAD: &[u8] = b"artifact-payload";
    if let Some(directory) = std::env::var_os("FRONTCAP_TEST_ARTIFACT_DIRECTORY") {
        let directory = PathBuf::from(directory);
        let mode = std::env::var("FRONTCAP_TEST_ARTIFACT_MODE").unwrap();
        let mut command = Command::new("/usr/bin/printf");
        command.arg("artifact-payload");
        let result = capture_artifact_recorded(&mut command, Instant::now()+Duration::from_secs(5),
            &AtomicBool::new(false), CaptureLimits { stdout: 1024, stderr: 1024 }, &directory, "screen.png");
        if mode == "missing-ledger" {
            assert!(result.is_err());
            assert!(!directory.exists());
            return;
        }
        let result = result.unwrap();
        assert!(result.exchange.failure.is_none());
        assert!(result.exchange.cleanup_failure.is_none());
        let identity: frontcap_core::process::ProcessIdentity = frontcap_core::state::load(&directory.join("process.json")).unwrap().unwrap();
        assert!(identity.has_exited().unwrap());
        if mode == "accepted" {
            assert_eq!(result.read_stdout(&directory.canonicalize().unwrap().join("screen.png")).unwrap(), PAYLOAD);
            assert!(result.exchange.evidence_failure.is_none());
        } else {
            assert!(result.stdout_file.is_none());
            assert!(result.exchange.evidence_failure.as_ref().unwrap().contains("write allowance exhausted"));
            assert!(!directory.join("screen.png").exists());
        }
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let limits = Limits { ordinary: 2 * PAYLOAD.len() as u64, retirement: 1024 };
    let budget = WriteBudget::create(&root.path().join("budget"), frontcap_core::ids::LaunchId::generate(), limits).unwrap();
    let child = |name: &str, mode: &str| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "process::artifact::tests::worker_binary_artifacts_share_durable_parent_allowance"])
            .env("FRONTCAP_TEST_ARTIFACT_DIRECTORY", root.path().join(name))
            .env("FRONTCAP_TEST_ARTIFACT_MODE", mode);
        WriteBudget::configure_child(&mut command, Some(&budget)).unwrap();
        command
    };
    let mut first = child("first", "accepted").spawn().unwrap();
    let mut second = child("second", "accepted").spawn().unwrap();
    let first_status = first.wait().unwrap();
    let second_status = second.wait().unwrap();
    assert!(first_status.success() && second_status.success());
    assert_eq!(budget.charged().unwrap().ordinary, limits.ordinary);
    assert!(child("refused", "refused").status().unwrap().success());
    // Freeze the binding before losing its ledger; the child must not silently become unbudgeted.
    let mut missing = child("missing", "missing-ledger");
    std::fs::rename(root.path().join("budget/ledger.json"), root.path().join("ledger.saved")).unwrap();
    assert!(missing.status().unwrap().success());
    assert!(!root.path().join("budget/ledger.json").exists());
    for name in ["first", "second"] {
        assert_eq!(std::fs::read(root.path().join(name).join("screen.png")).unwrap(), PAYLOAD);
    }
}
