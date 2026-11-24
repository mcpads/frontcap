use super::*;

fn record(dir: &Path, state: RuntimeState) -> RuntimeRecord {
    RuntimeRecord {
        launch_id: frontcap_core::ids::LaunchId::new("g-terminal").unwrap(),
        state,
        recorder: ProcessIdentity::current(),
        browser: None,
        endpoint: None,
        socket: dir.join("recorder.sock"),
    }
}

#[tokio::test]
async fn terminal_execution_rejects_launch_and_recovery_without_changing_evidence() {
    for state in [RuntimeState::Stopped, RuntimeState::Failed { reason: "termination refused".into(), error: None }] {
        let dir = tempfile::tempdir().unwrap();
        let original = record(dir.path(), state);
        runtime::save_runtime(dir.path(), &original).unwrap();
        let bytes = std::fs::read(runtime::runtime_path(dir.path())).unwrap();
        // No spec exists: terminal refusal must happen before any launch/adoption preparation.
        for recover in [false, true] {
            let error = if recover { run_recover(dir.path()).await } else { run_launch(dir.path()).await }.unwrap_err();
            let expected = match &original.state {
                RuntimeState::Failed { reason, .. } => reason.as_str(),
                _ => "a stopped generation cannot resume",
            };
            assert_eq!(error.to_string(), expected);
            assert_eq!(std::fs::read(runtime::runtime_path(dir.path())).unwrap(), bytes);
        }
    }
}

#[tokio::test]
async fn an_owned_active_execution_still_records_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    runtime::save_runtime(dir.path(), &record(dir.path(), RuntimeState::Starting)).unwrap();
    as_owner(dir.path(), || async { Err(RecorderError::Missing("adoption failed".into())) }).await.unwrap_err();
    assert_eq!(runtime::load_runtime(dir.path()).unwrap().unwrap().state,
        RuntimeState::Failed { reason: "adoption failed".into(), error: None });
}


#[tokio::test]
async fn typed_startup_refusal_survives_terminal_launch_and_recovery() {
    use frontcap_recorder_protocol::ErrorCode;
    let dir = tempfile::tempdir().unwrap();
    runtime::save_runtime(dir.path(), &record(dir.path(), RuntimeState::Starting)).unwrap();
    as_owner(dir.path(), || async {
        Err(RecorderError::Chromium(frontcap_chromium::ChromiumError::InvalidArgument("scope alias".into())))
    }).await.unwrap_err();
    let saved = runtime::load_runtime(dir.path()).unwrap().unwrap();
    let RuntimeState::Failed { reason, error: Some(error) } = &saved.state else { panic!("{saved:?}") };
    assert_eq!(error.code, ErrorCode::InvalidArguments);
    assert_eq!(&error.message, reason);
    assert!(error.recovery.contains("new request_id"));
    let bytes = std::fs::read(runtime::runtime_path(dir.path())).unwrap();
    assert!(run_launch(dir.path()).await.is_err());
    assert!(run_recover(dir.path()).await.is_err());
    assert_eq!(std::fs::read(runtime::runtime_path(dir.path())).unwrap(), bytes);
}
