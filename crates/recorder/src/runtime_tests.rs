use super::*;

#[test]
fn short_runtime_dirs_put_the_socket_inside() {
    let dir = std::path::Path::new("/tmp/fc/runtimes/g-0123456789abcdef");
    assert_eq!(socket_path(dir), dir.join("recorder.sock"));
}

#[test]
fn long_runtime_dirs_use_a_short_derived_socket_path() {
    let long = std::path::PathBuf::from(format!("/tmp/{}/runtimes/g-0123456789abcdef", "d".repeat(120)));
    let p = socket_path(&long);
    assert!(p.as_os_str().len() <= 100, "{}", p.display());
    assert_eq!(p, socket_path(&long), "derived deterministically");
    assert_ne!(p, socket_path(&long.join("x")));
}

#[test]
fn runtime_records_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let rec = RuntimeRecord {
        launch_id: frontcap_core::ids::LaunchId::new("g-1").unwrap(),
        state: RuntimeState::Failed { reason: "x".into(), error: None },
        recorder: frontcap_core::process::ProcessIdentity::current(),
        browser: None,
        endpoint: None,
        socket: tmp.path().join("s"),
    };
    save_runtime(tmp.path(), &rec).unwrap();
    assert_eq!(load_runtime(tmp.path()).unwrap(), Some(rec));
}


#[test]
fn legacy_failed_runtime_state_keeps_its_wire_shape() {
    let legacy = serde_json::json!({"state":"failed","reason":"old failure"});
    let state: RuntimeState = serde_json::from_value(legacy.clone()).unwrap();
    assert!(matches!(&state,RuntimeState::Failed{error:None,..}));
    assert_eq!(serde_json::to_value(state).unwrap(), legacy);
}
