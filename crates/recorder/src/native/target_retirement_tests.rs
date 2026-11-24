use super::*;
use std::{fs, time::Duration};

#[test]
fn flutter_stop_waits_for_original_retirement_within_its_budget() {
    let root = tempfile::tempdir().unwrap();
    let worker = root.path().join("session/worker");
    fs::create_dir_all(&worker).unwrap();
    let lock = fs::File::create(worker.join("session.lock")).unwrap();
    lock.lock().unwrap();
    let ready: Ready = serde_json::from_value(serde_json::json!({
        "platform":"flutter", "owner": {"platform":"ios",
            "device":"10000000-0000-0000-0000-000000000001",
            "generation":"a".repeat(48), "source_sha256":"b".repeat(64)}
    })).unwrap();
    // Expiration must not manufacture a terminal result or release the original lease.
    assert!(inspect_stop(root.path(), &ready, Instant::now()).is_err());
    assert!(!worker.join("result.json").exists());
    let original = worker.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        fs::write(original.join("result.json"), br#"{"state":"interrupted","retirement":{"passed":true,"errors":[],"signal_error":null},"error":null}"#).unwrap();
        drop(lock);
    });
    let retired = inspect_stop(root.path(), &ready, Instant::now() + Duration::from_secs(5)).unwrap();
    writer.join().unwrap();
    assert!(retired.passed);
    let original = fs::read(worker.join("result.json")).unwrap();
    assert!(inspect_stop(root.path(), &ready, Instant::now()).unwrap().passed);
    assert_eq!(fs::read(worker.join("result.json")).unwrap(), original);
}
