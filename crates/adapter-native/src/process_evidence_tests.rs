use super::*;
use frontcap_core::state::save_atomic;
use std::{
    process::Command,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

#[test]
fn file_streams_preserve_binary_bytes_partial_failure_and_legacy_reads() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("capture");
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf '\\377abc'; printf diagnostic >&2; exit 7"]);
    let result = crate::process::capture_recorded(
        &mut command,
        Instant::now() + Duration::from_secs(5),
        &AtomicBool::new(false),
        1024,
        &directory,
    )
    .unwrap();
    assert_eq!(result.stdout, b"\xffabc");
    assert_eq!(result.stderr, b"diagnostic");
    assert_eq!(result.exit_code, Some(7));
    let path = directory.join("exchange.json");
    let original = std::fs::read(&path).unwrap();
    let loaded = load_exchange::<crate::bridge::BridgeReply>(&path)
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&loaded).unwrap(),
        serde_json::to_vec(&result).unwrap()
    );
    validate_exchange_files(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert_eq!(
        std::fs::read(directory.join("stdout.bin")).unwrap(),
        result.stdout
    );
    let legacy = root.path().join("legacy.json");
    save_atomic(&legacy, &result).unwrap();
    assert_eq!(
        serde_json::to_vec(
            &load_exchange::<crate::bridge::BridgeReply>(&legacy)
                .unwrap()
                .unwrap()
        )
        .unwrap(),
        serde_json::to_vec(&result).unwrap()
    );
    std::fs::write(directory.join("stdout.bin"), b"\xffabd").unwrap();
    assert!(load_exchange::<crate::bridge::BridgeReply>(&path).is_err());
    assert!(validate_exchange_files(&path).is_err());
}

#[test]
fn file_streams_refuse_missing_linked_escaped_and_oversized_evidence() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("exchange.json");
    let mut result = crate::process::empty_exchange();
    result.stdout = b"original".to_vec();
    save_exchange(&path, &result).unwrap();
    let original = std::fs::read(&path).unwrap();
    std::fs::remove_file(root.path().join("stdout.bin")).unwrap();
    assert!(load_exchange::<()>(&path).is_err());
    std::fs::write(root.path().join("other"), b"original").unwrap();
    std::os::unix::fs::symlink("other", root.path().join("stdout.bin")).unwrap();
    assert!(load_exchange::<()>(&path).is_err());
    std::fs::remove_file(root.path().join("stdout.bin")).unwrap();
    std::fs::write(root.path().join("stdout.bin"), b"original").unwrap();
    for (name, size) in [("../other", 8), ("stdout.bin", STREAM_LIMIT + 1)] {
        let mut record: Exchange<(), Stream> = serde_json::from_slice(&original).unwrap();
        if let Stream::File(file) = &mut record.stdout {
            file.file = name.into();
            file.bytes = size;
        }
        save_atomic(&path, &record).unwrap();
        assert!(load_exchange::<()>(&path).is_err());
    }
}

#[test]
fn large_diagnostics_are_retained_once_without_numeric_array_expansion() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("exchange.json");
    let mut result = crate::process::empty_exchange();
    result.stdout = vec![b'x'; 1024 * 1024];
    save_exchange(&path, &result).unwrap();
    assert!(path.metadata().unwrap().len() < 1024);
    assert!(root.path().join("stdout.bin.gz").metadata().unwrap().len() < 16 * 1024);
    assert!(!root.path().join("stdout.bin").exists());
    assert!(!root.path().join("stderr.bin").exists());
    assert_eq!(
        load_exchange::<()>(&path).unwrap().unwrap().stdout,
        result.stdout
    );
    validate_exchange_files(&path).unwrap();
    let original = std::fs::read(&path).unwrap();
    let mut record: Exchange<(), Stream> = serde_json::from_slice(&original).unwrap();
    if let Stream::File(file) = &mut record.stdout {
        file.bytes = 16;
    }
    save_atomic(&path, &record).unwrap();
    assert!(load_exchange::<()>(&path).is_err()); // Bounded expansion cannot hide a false size.
    std::fs::write(&path, &original).unwrap();
    let compressed = root.path().join("stdout.bin.gz");
    let mut bytes = std::fs::read(&compressed).unwrap();
    bytes[4] ^= 1; // Header metadata change keeps decoded payload intact; encoded digest must reject it.
    std::fs::write(&compressed, bytes).unwrap();
    assert!(load_exchange::<()>(&path).is_err());
    assert!(validate_exchange_files(&path).is_err());
}

#[test]
fn failed_stream_publication_keeps_partial_bytes_without_publishing_success() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("exchange.json");
    std::fs::create_dir(root.path().join("stderr.bin")).unwrap();
    let mut result = crate::process::empty_exchange();
    result.stdout = b"partial output".to_vec();
    result.stderr = b"failure detail".to_vec();
    result.failure = Some("original failure".into());
    assert!(save_exchange(&path, &result).is_err());
    assert!(!path.exists());
    assert_eq!(
        std::fs::read(root.path().join("stdout.bin")).unwrap(),
        result.stdout
    );
    assert_eq!(result.stderr, b"failure detail");
    assert_eq!(result.failure.as_deref(), Some("original failure"));
}

#[test]
fn shared_allowance_admits_encoded_bundle_before_any_output_and_reads_without_spending() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let root = tempfile::tempdir().unwrap();
    let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::new("g-exchange").unwrap(),
        Limits { ordinary: 16384, retirement: 512 }).unwrap();
    let mut result = crate::process::empty_exchange();
    result.stdout = vec![b'x'; 1024 * 1024];
    let path = root.path().join("exchange.json");
    save_exchange_with_budget(&path, &result, Some(&budget)).unwrap();
    let spent = budget.charged().unwrap().ordinary;
    let retained = path.metadata().unwrap().len() + root.path().join("stdout.bin.gz").metadata().unwrap().len();
    assert_eq!(spent, retained);
    assert_eq!(load_exchange::<()>(&path).unwrap().unwrap().stdout, result.stdout);
    assert_eq!(budget.charged().unwrap().ordinary, spent);
    budget.charge(Class::Ordinary, 16384 - spent).unwrap();
    let refused = root.path().join("refused");std::fs::create_dir(&refused).unwrap();
    assert!(save_exchange_with_budget(&refused.join("exchange.json"), &result, Some(&budget)).is_err());
    assert_eq!(std::fs::read_dir(&refused).unwrap().count(), 0);
    assert_eq!(load_exchange::<()>(&path).unwrap().unwrap().stdout, result.stdout);
    budget.charge(Class::Retirement, 512).unwrap();
}

#[test]
fn bridge_budget_refuses_before_spawn_or_preserves_uncertainty_after_effect() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let root = tempfile::tempdir().unwrap();
    for (name, ordinary) in [("before", 1), ("after", 4096)] {
        let budget = WriteBudget::create(&root.path().join(name), LaunchId::new(format!("g-{name}")).unwrap(),
            Limits { ordinary, retirement: 512 }).unwrap();
        let marker = root.path().join(format!("{name}-effect"));
        let mut command = Command::new("python3");
        command.args(["-c", "import pathlib,sys,json; pathlib.Path(sys.argv[1]).write_text('once'); print(json.dumps('x'*8192))"]).arg(&marker);
        let evidence = root.path().join(format!("{name}-exchange"));
        let result: io::Result<Exchange<String>> = crate::process::exchange_typed_recorded_with_budget(
            &mut command, &(), Instant::now()+Duration::from_secs(5), &AtomicBool::new(false), 16384, &evidence, Some(&budget));
        if name == "before" {
            assert!(result.is_err());assert!(!marker.exists());
        } else {
            let result = result.unwrap();
            assert_eq!(std::fs::read_to_string(marker).unwrap(), "once");
            assert!(result.evidence_failure.is_some());assert!(result.reply.is_none());
            assert!(result.cleanup_failure.is_none());assert_eq!(result.exit_code, Some(0));
            assert!(!evidence.join("exchange.json").exists());
            assert!(!evidence.join("stdout.bin").exists());
            assert!(result.stdout.len() > 8192);
        }
    }
}

#[test]
fn native_worker_budget_entry() {
    let Some(root) = std::env::var_os("FRONTCAP_EXCHANGE_TEST_ROOT") else { return; };
    let root = std::path::PathBuf::from(root);
    let mut command = Command::new("/bin/cat");
    let result: Exchange<String> = crate::process::exchange_typed_recorded(
        &mut command, &"original", Instant::now()+Duration::from_secs(5),
        &AtomicBool::new(false), 1024, &root.join("exchange")).unwrap();
    assert_eq!(result.reply.as_deref(), Some("original"));
    assert!(result.evidence_failure.is_none());
}

#[test]
fn native_typed_exchange_reopens_inherited_allowance_in_real_worker() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let root = tempfile::tempdir().unwrap();
    let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::new("g-native-exchange").unwrap(),
        Limits { ordinary: 16384, retirement: 512 }).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "process::evidence::tests::native_worker_budget_entry", "--nocapture"])
        .env("FRONTCAP_EXCHANGE_TEST_ROOT", root.path());
    WriteBudget::configure_child(&mut command, Some(&budget)).unwrap();
    assert!(command.status().unwrap().success());
    let bytes: u64 = std::fs::read_dir(root.path().join("exchange")).unwrap()
        .map(|e| e.unwrap().metadata().unwrap().len()).sum();
    assert_eq!(budget.charged().unwrap().ordinary, bytes);
    assert_eq!(budget.charged().unwrap().retirement, 0);
}
