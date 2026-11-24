use super::*;
use frontcap_core::process::ProcessIdentity;

#[tokio::test]
async fn spec_admission_is_shared_across_retries_and_never_resets_missing_budget() {
    let root = tempfile::tempdir().unwrap();
    let mut spec: LaunchSpec = serde_json::from_value(serde_json::json!({
        "request_id":"launch", "launch_id":"g-spec", "run_id":"r-spec",
        "session_executable":"unused", "acquisition_budget_ms":1000,
        "write_budget":{"ordinary":65536,"retirement":1024},
        "target":{"platform":"android","config":{
            "emulator":"unused","adb":"unused","avd":"unused","port":5580,
            "apk":"unused","apk_sha256":"b".repeat(64),"python":"python3",
            "bridge_script":"unused","source_sha256":"a".repeat(64)
        }}
    })).unwrap();
    std::thread::scope(|scope| {
        for _ in 0..4 {
            let spec = &spec;
            let dir = root.path();
            scope.spawn(move || publish_spec(dir, spec).unwrap());
        }
    });
    let path = super::super::spec_path(root.path());
    let original = std::fs::read(&path).unwrap();
    let budget_path = root.path().join("native-write-budget");
    let budget = WriteBudget::open(&budget_path, spec.launch_id.clone(), spec.write_budget.unwrap()).unwrap();
    assert_eq!(budget.charged().unwrap().ordinary, original.len() as u64);
    spec.acquisition_budget_ms += 1;
    assert!(publish_spec(root.path(), &spec).is_err());
    spec.acquisition_budget_ms -= 1;
    std::fs::rename(&budget_path, root.path().join("retained-budget")).unwrap();
    assert!(publish_spec(root.path(), &spec).is_err());
    let error = crate::run_launch(root.path()).await.unwrap_err();
    assert!(error.to_string().contains("lost its write allowance"));
    assert!(!budget_path.exists());
    assert_eq!(std::fs::read(path).unwrap(), original);

    let small = tempfile::tempdir().unwrap();
    spec.write_budget.as_mut().unwrap().ordinary = 1;
    assert!(publish_spec(small.path(), &spec).is_err());
    assert!(!super::super::spec_path(small.path()).exists());
    let small_budget = WriteBudget::open(&small.path().join("native-write-budget"),
        spec.launch_id.clone(), spec.write_budget.unwrap()).unwrap();
    assert_eq!(small_budget.charged().unwrap().ordinary, 0);
    assert!(publish_spec(small.path(), &spec).is_err());
    assert_eq!(small_budget.charged().unwrap().ordinary, 0);
}

#[tokio::test]
async fn failed_launch_retry_returns_original_cause_without_starting_or_connecting() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let ws = Workspace { root: temp.path().into() };
    let request: LaunchRequest = serde_json::from_value(serde_json::json!({
        "request_id":"launch", "session_executable":"unused", "acquisition_budget_ms":1000,
        "target":{"platform":"android","config":{
            "emulator":"unused","adb":"unused","avd":"unused","port":5580,
            "apk":"unused","apk_sha256":"b".repeat(64),"python":"python3",
            "bridge_script":"unused","source_sha256":"a".repeat(64)
        }}
    })).unwrap();
    let id = register(&ws, &request).unwrap();
    let dir = ws.runtime_dir(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let reason = "TimeoutError: original initial bridge failure";
    runtime::save_runtime(&dir, &runtime::RuntimeRecord {
        launch_id: id.clone(), state: RuntimeState::Failed { reason: reason.into(), error: None },
        recorder: ProcessIdentity::current(), browser: None, endpoint: None,
        socket: dir.join("must-not-connect.sock"),
    }).unwrap();
    let listener = tokio::net::UnixListener::bind(dir.join("must-not-connect.sock")).unwrap();
    let before = std::fs::read(runtime::runtime_path(&dir)).unwrap();
    let original = launcher::wait_ready(&dir, Duration::from_secs(1)).await.unwrap_err();
    for _ in 0..2 {
        let error = launch(&ws, request.clone(), Path::new("must-not-spawn"), &[])
            .await.err().expect("failed launch must stay failed");
        assert_eq!(error.0.code, original.0.code);
        assert_eq!(error.0.message, original.0.message);
        assert_eq!(error.0.message, reason);
        assert_eq!(std::fs::read(runtime::runtime_path(&dir)).unwrap(), before);
        assert!(!dir.join("native-session").exists());
        assert!(!super::super::spec_path(&dir).exists());
        assert_eq!(register(&ws, &request).unwrap(), id);
    }
    assert!(tokio::time::timeout(Duration::from_millis(20), listener.accept()).await.is_err());
}

#[tokio::test]
async fn foreign_runtime_is_rejected_before_launch_retry_or_reattach_connects() {
    let temp = tempfile::tempdir_in("/tmp").unwrap();
    let ws = Workspace {
        root: temp.path().into(),
    };
    let request: LaunchRequest = serde_json::from_value(serde_json::json!({
        "request_id":"launch", "session_executable":"unused", "acquisition_budget_ms":1000,
        "target":{"platform":"android","config":{
            "emulator":"unused","adb":"unused","avd":"unused","port":5580,
            "apk":"unused","apk_sha256":"b".repeat(64),"python":"python3",
            "bridge_script":"unused","source_sha256":"a".repeat(64)
        }}
    }))
    .unwrap();
    let id = register(&ws, &request).unwrap();
    let dir = ws.runtime_dir(&id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(super::super::spec_path(&dir), b"{}").unwrap();
    runtime::save_runtime(
        &dir,
        &runtime::RuntimeRecord {
            launch_id: LaunchId::new("g-foreign").unwrap(),
            state: RuntimeState::Ready,
            recorder: ProcessIdentity::current(),
            browser: None,
            endpoint: None,
            socket: dir.join("must-not-connect.sock"),
        },
    )
    .unwrap();
    let listener = tokio::net::UnixListener::bind(dir.join("must-not-connect.sock")).unwrap();
    let before = std::fs::read(runtime::runtime_path(&dir)).unwrap();
    let error = tokio::time::timeout(
        Duration::from_millis(100),
        reattach(&ws, &id, RequestId::new("attach").unwrap()),
    )
    .await
    .expect("reject foreign runtime before waiting for socket hello")
    .err()
    .expect("foreign runtime must be rejected");
    assert_eq!(error.0.code, ErrorCode::RecorderFault);
    assert!(error.0.message.contains("identity differs"));
    let error = launch(&ws, request, Path::new("must-not-spawn"), &[])
        .await
        .err()
        .expect("launch retry must reject foreign runtime");
    assert_eq!(error.0.code, ErrorCode::RecorderFault);
    assert!(error.0.message.contains("identity differs"));
    assert_eq!(std::fs::read(runtime::runtime_path(&dir)).unwrap(), before);
    assert!(!dir.join("native-session").exists());
    assert!(
        tokio::time::timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err(),
        "neither request may connect to the foreign runtime socket"
    );
}
#[tokio::test]
async fn attach_reply_must_bind_the_requested_live_lease() {
    use frontcap_recorder_protocol::{
        Answer, ClientFrame, ServerFrame,
        frame::{read_frame, write_frame},
    };
    for mode in ["valid", "foreign", "stopped", "no-lease"] {
        let temp = tempfile::tempdir_in("/tmp").unwrap();
        let ws = Workspace {
            root: temp.path().into(),
        };
        let id = LaunchId::new("g-expected").unwrap();
        let dir = ws.runtime_dir(&id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(super::super::spec_path(&dir), b"{}").unwrap();
        let socket = dir.join("peer.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        runtime::save_runtime(
            &dir,
            &runtime::RuntimeRecord {
                launch_id: id.clone(),
                state: RuntimeState::Ready,
                recorder: ProcessIdentity::current(),
                browser: None,
                endpoint: None,
                socket,
            },
        )
        .unwrap();
        let status: Status = serde_json::from_value(serde_json::json!({
            "launch_id":if mode=="foreign" {"g-foreign"} else {"g-expected"}, "run_id":"r-test",
            "ready":{"platform":"android_views","restart":false,"app_lifecycle":false,"owner":{
                "package":"org.example.client","activity":"org.example.client.Main","serial":"emulator-5580",
                "generation":"58e8003c-c32d-49f2-a4d3-628724d76b41","pid":42,
                "source_sha256":"a".repeat(64),"apk_sha256":"b".repeat(64)}},
            "capabilities":[],"lease_held":mode!="no-lease","running":null,
            "last_completed_request_id":null,"failure":null,"stopped":mode=="stopped","evidence":"unused"
        })).unwrap();
        let expected = id.clone();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut read = tokio::io::BufReader::new(read);
            let hello: ClientFrame = read_frame(&mut read).await.unwrap().unwrap();
            assert_eq!(hello.call, Call::Hello);
            write_frame(
                &mut write,
                &ServerFrame {
                    id: hello.id,
                    answer: Answer::Ok(Reply::Hello {
                        launch_id: expected,
                    }),
                },
            )
            .await
            .unwrap();
            let attach: ClientFrame = read_frame(&mut read).await.unwrap().unwrap();
            assert!(matches!(attach.call, Call::Reattach { .. }));
            write_frame(
                &mut write,
                &ServerFrame {
                    id: attach.id,
                    answer: Answer::Ok(Reply::Native {
                        body: NativeReply::Attached {
                            lease: LeaseToken::generate(),
                            status: Box::new(status),
                            completed_while_detached: vec![],
                        },
                    }),
                },
            )
            .await
            .unwrap();
            let next: Option<ClientFrame> =
                tokio::time::timeout(Duration::from_secs(2), read_frame(&mut read))
                    .await
                    .unwrap()
                    .unwrap();
            assert!(
                next.is_none(),
                "no additional operation may follow this test attachment"
            );
        });
        let result = reattach(&ws, &id, RequestId::new("attach").unwrap()).await;
        if mode == "valid" {
            assert!(result.is_ok());
            drop(result);
        } else {
            assert_eq!(
                result
                    .err()
                    .expect("invalid attachment must be rejected")
                    .0
                    .code,
                ErrorCode::RecorderFault
            );
        }
        peer.await.unwrap();
    }
}
