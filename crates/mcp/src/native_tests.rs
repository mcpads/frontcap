use super::*;
#[tokio::test]
async fn storage_inventory_works_without_recorder_and_preserves_evidence() {
    let temp = tempfile::tempdir().unwrap();
    let server = Frontcap { config: crate::config::Config {
        workspace: frontcap_recorder::launcher::Workspace { root: temp.path().into() },
        recorder_bin: "must-not-launch".into(), frontcap_home: "unused".into(),
    }, connections: Default::default() };
    let id = LaunchId::new("g-storage").unwrap();
    let dir = server.config.workspace.runtime_dir(&id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(frontcap_recorder::native::spec_path(&dir), b"{}").unwrap();
    let record = RuntimeRecord {
        launch_id: id.clone(), state: RuntimeState::Stopped,
        recorder: frontcap_core::process::ProcessIdentity::current(),
        browser: None, endpoint: None, socket: dir.join("absent.sock"),
    };
    runtime::save_runtime(&dir, &record).unwrap();
    let before = std::fs::read(runtime::runtime_path(&dir)).unwrap();
    let args: Args = serde_json::from_value(serde_json::json!({
        "op": {"type": "storage", "launch_id": id, "max_entries": 100}
    })).unwrap();
    let result = server.native_run(args).await.unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["response"], "storage");
    assert_eq!(result["usage"]["regular_files"]["logical_bytes"], before.len() as u64 + 2);
    assert_eq!(result["usage"]["limit_reached"], false);
    assert_eq!(std::fs::read(runtime::runtime_path(&dir)).unwrap(), before);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    // The public operation must report separately routed evidence without starting a recorder.
    use std::os::unix::fs::MetadataExt;
    let external = tempfile::tempdir().unwrap();
    let parent = external.path().canonicalize().unwrap();
    let session = parent.join(id.as_str());
    std::fs::create_dir(&session).unwrap();
    std::fs::write(session.join("original"), b"external").unwrap();
    let root_meta = parent.metadata().unwrap();
    let session_meta = session.metadata().unwrap();
    frontcap_core::state::save_atomic(&dir.join("native-session-location.json"),
        &serde_json::json!({"launch_id":id,"session_root":parent})).unwrap();
    for (name, role, path, meta) in [
        ("native-storage-roots.json", "session_storage", &parent, root_meta),
        ("native-storage-acquired-roots.json", "session_evidence", &session, session_meta),
    ] {
        frontcap_core::state::save_atomic(&dir.join(name), &serde_json::json!({"roots":[{
            "role":role,"path":path,"device":meta.dev(),"inode":meta.ino()
        }]})).unwrap();
    }
    let result = server.native_run(Args { op: Op::Storage { launch_id: id.clone(), max_entries: Some(100) } }).await.unwrap();
    let result = serde_json::to_value(result).unwrap();
    assert_eq!(result["usage"]["external_session"]["state"], "measured");
    assert_eq!(result["usage"]["external_session"]["usage"]["regular_files"]["logical_bytes"], 8);
    assert_eq!(std::fs::read(runtime::runtime_path(&dir)).unwrap(), before);
    assert_eq!(std::fs::read(session.join("original")).unwrap(), b"external");
    assert!(server.native_run(Args { op: Op::Storage { launch_id: id, max_entries: Some(0) } }).await.is_err());
}

#[test]
fn external_session_image_uses_registered_root_and_rejects_replacement() {
    use frontcap_core::state::save_atomic;
    use std::os::unix::fs::MetadataExt;
    let workspace = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let parent = external.path().join("evidence");
    std::fs::create_dir(&parent).unwrap();
    let parent = parent.canonicalize().unwrap();
    let server = Frontcap { config: crate::config::Config {
        workspace: frontcap_recorder::launcher::Workspace { root: workspace.path().into() },
        recorder_bin: "unused".into(), frontcap_home: "unused".into(),
    }, connections: Default::default() };
    let launch = LaunchId::new("g-external").unwrap();
    let runtime = server.config.workspace.runtime_dir(&launch);
    std::fs::create_dir_all(&runtime).unwrap();
    let config: serde_json::Value = serde_json::from_str(include_str!("../../recorder/tests/fixtures/native/webview-launch.json")).unwrap();
    let spec = serde_json::json!({"request_id":"launch", "launch_id":launch, "run_id":"r-test",
        "target":{"platform":"ios", "runtime":"webview", "config":config},
        "session_executable":"/unused", "acquisition_budget_ms":5000, "session_root":parent});
    save_atomic(&frontcap_recorder::native::spec_path(&runtime), &spec).unwrap();
    save_atomic(&runtime.join("native-session-location.json"), &serde_json::json!({"launch_id":launch,"session_root":parent})).unwrap();
    let info = parent.metadata().unwrap();
    save_atomic(&runtime.join("native-storage-roots.json"), &serde_json::json!({"roots":[{
        "role":"session_storage", "path":parent, "device":info.dev(), "inode":info.ino()}]})).unwrap();
    let session = parent.join(launch.as_str());
    let path = session.join("target/webview/capture-capture/screen.png");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let data = b"\x89PNG\r\n\x1a\nretained";
    std::fs::write(&path, data).unwrap();
    let info = session.metadata().unwrap();
    save_atomic(&runtime.join("native-storage-acquired-roots.json"), &serde_json::json!({"roots":[{
        "role":"session_evidence", "path":session, "device":info.dev(), "inode":info.ino()}]})).unwrap();
    let request = RequestId::new("capture").unwrap();
    assert_eq!(server.native_webview_png(&launch, &request, &path, data.len() as u64, &sha256_bytes_hex(data)).unwrap(), data);
    std::fs::rename(&session, parent.join("original")).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, data).unwrap();
    assert!(server.native_webview_png(&launch, &request, &path, data.len() as u64, &sha256_bytes_hex(data)).is_err());
    assert_eq!(std::fs::read(path).unwrap(), data);
}

#[test]
fn webview_image_is_bound_to_its_request_and_completed_digest() {
    let temp = tempfile::tempdir().unwrap();
    let server = Frontcap { config: crate::config::Config {
        workspace: frontcap_recorder::launcher::Workspace { root: temp.path().into() },
        recorder_bin: "unused".into(), frontcap_home: "unused".into(),
    }, connections: Default::default() };
    let launch = LaunchId::new("g-webview").unwrap();
    let request = RequestId::new("capture").unwrap();
    let path = server.config.workspace.runtime_dir(&launch).join("native-session/target/webview/capture-capture/screen.png");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let data = frontcap_core::encode::unbase64("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=").unwrap();
    std::fs::write(&path, &data).unwrap();
    let digest = sha256_bytes_hex(&data);
    let canonical = path.canonicalize().unwrap();
    let hash = sha256_bytes_hex(request.as_str().as_bytes());
    for relative in [format!("native-session/page/request-{}-screen/screen.png", &hash[..32]), format!("native-session/target/contact-driver/capture-request-{}-basis/screen.png", &hash[..32]), format!("native-session/target/host-restarts/restart/contact-driver/capture-request-{}-basis/screen.png", &hash[..32])] {
        let android = server.config.workspace.runtime_dir(&launch).join(relative);
        std::fs::create_dir_all(android.parent().unwrap()).unwrap();
        std::fs::write(&android, &data).unwrap();
        let android = android.canonicalize().unwrap();
        assert_eq!(server.native_page_png(&launch, &request, &android, data.len() as u64, &digest, true).unwrap(), data);
        assert!(server.native_page_png(&launch, &RequestId::new("other").unwrap(), &android, data.len() as u64, &digest, true).is_err());
        assert!(server.native_webview_png(&launch, &request, &android, data.len() as u64, &digest).is_err());
        std::fs::write(&android, b"changed").unwrap();
        assert!(server.native_page_png(&launch, &request, &android, data.len() as u64, &digest, true).is_err());
    }

    assert_eq!(server.native_webview_png(&launch,&request,&canonical,data.len() as u64,&digest).unwrap(),data);
    assert!(server.native_webview_png(&launch,&RequestId::new("another").unwrap(),&canonical,data.len() as u64,&digest).is_err());
    let epoch_path = server.config.workspace.runtime_dir(&launch).join("native-session/epochs/restart/capture-capture/screen.png");
    std::fs::create_dir_all(epoch_path.parent().unwrap()).unwrap();
    std::fs::write(&epoch_path, &data).unwrap();
    let epoch = epoch_path.canonicalize().unwrap();
    assert_eq!(server.native_webview_png(&launch,&request,&epoch,data.len() as u64,&digest).unwrap(),data);
    // A newer epoch does not prevent reading the retained original result.
    assert_eq!(server.native_webview_png(&launch,&request,&canonical,data.len() as u64,&digest).unwrap(),data);
    assert!(server.native_webview_png(&launch,&RequestId::new("another").unwrap(),&epoch,data.len() as u64,&digest).is_err());
    assert!(server.native_webview_png(&LaunchId::new("g-other").unwrap(),&request,&epoch,data.len() as u64,&digest).is_err());
    let arbitrary = temp.path().join("screen.png");
    std::fs::write(&arbitrary, &data).unwrap();
    assert!(server.native_webview_png(&launch,&request,&arbitrary.canonicalize().unwrap(),data.len() as u64,&digest).is_err());
    std::fs::remove_file(&epoch_path).unwrap();
    std::os::unix::fs::symlink(&canonical, &epoch_path).unwrap();
    assert!(server.native_webview_png(&launch,&request,&epoch,data.len() as u64,&digest).is_err());
    std::fs::write(&path, b"changed").unwrap();
    assert!(server.native_webview_png(&launch,&request,&canonical,data.len() as u64,&digest).is_err());
}
#[tokio::test]
async fn stop_result_and_status_survive_socket_removal_before_runtime_finalization() {
    use frontcap_core::{
        journal::Journal,
        native::ownership::Retirement,
        process::ProcessIdentity,
        state::save_atomic,
    };
    let temp = tempfile::tempdir().unwrap();
    let server = Frontcap {
        config: crate::config::Config {
            workspace: frontcap_recorder::launcher::Workspace {
                root: temp.path().into(),
            },
            recorder_bin: "unused".into(),
            frontcap_home: "unused".into(),
        },
        connections: Default::default(),
    };
    let id = LaunchId::new("g-test").unwrap();
    let dir = server.config.workspace.runtime_dir(&id);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(frontcap_recorder::native::spec_path(&dir), b"{}").unwrap();
    let record = RuntimeRecord {
        launch_id: id.clone(),
        state: RuntimeState::Ready,
        recorder: ProcessIdentity::current(),
        browser: None,
        endpoint: None,
        socket: dir.join("removed.sock"),
    };
    runtime::save_runtime(&dir, &record).unwrap();
    let status = Status {
        launch_id: id.clone(),
        run_id: frontcap_core::ids::RunId::new("r-test").unwrap(),
        ready: frontcap_recorder_protocol::native::Ready::AndroidViews {
            owner:frontcap_core::native::android_app::Owner { package:"org.example.client".into(), activity:"org.example.client.Main".into(), serial:"emulator-5580".into(), generation:"58e8003c-c32d-49f2-a4d3-628724d76b41".into(), pid:42, source_sha256:"a".repeat(64), apk_sha256:"b".repeat(64) },
            restart: false, app_lifecycle: false,
        },
        capabilities: vec![],
        lease_held: false,
        running: None,
        last_completed_request_id: Some(RequestId::new("stop").unwrap()),
        failure: None,
        stopped: true,
        evidence: dir.join("native-session"),
    };
    save_atomic(&dir.join("native-status.json"), &status).unwrap();
    let request_id = RequestId::new("stop").unwrap();
    let answer = Answer::Ok(Reply::Native {
        body: NativeReply::Done {
            request_id: request_id.clone(),
            outcome: Box::new(Outcome::Stopped {
                retirement: Retirement {
                    passed: true,
                    errors: vec![],
                    signal_error: None,
                },
            }),
        },
    });
    let mut journal = Journal::open(&runtime::journal_path(&dir)).unwrap();
    journal.begin(&request_id, "hash").unwrap();
    journal.dispatched(&request_id).unwrap();
    journal
        .complete(&request_id, serde_json::to_value(&answer).unwrap())
        .unwrap();
    assert_eq!(
        server
            .native_call(
                &id,
                Call::Result {
                    request_id: request_id.clone()
                }
            )
            .await
            .unwrap(),
        Reply::Result {
            request_id,
            result: RequestResult::Completed {
                answer: Box::new(answer)
            }
        }
    );
    assert!(matches!(
        server
            .native_run(Args {
                op: Op::Status {
                    launch_id: id.clone()
                }
            })
            .await
            .unwrap(),
        Response::RecordedStatus { .. }
    ));
    // Retained worker failure remains visible without the target directory; it is not a
    // completion result and does not require the worker lifetime lock to still be readable.
    let journal_before = std::fs::read(runtime::journal_path(&dir)).unwrap();
    let missing_session = dir.canonicalize().unwrap().join("removed-target/session");
    let control = dir.canonicalize().unwrap().join(format!("flutter-control-{}",
        sha256_bytes_hex(missing_session.as_os_str().as_encoded_bytes())));
    std::fs::create_dir(&control).unwrap();
    use std::os::unix::fs::MetadataExt;
    let metadata = control.metadata().unwrap();
    save_atomic(&dir.join("flutter-worker-control.json"), &serde_json::json!({
        "session":missing_session,"location":{"directory":control,"device":metadata.dev(),"inode":metadata.ino()}
    })).unwrap();
    save_atomic(&control.join("result.json"), &serde_json::json!({"state":"failed",
        "retirement":{"passed":false,"errors":["original status failure"],"signal_error":null},"error":null})).unwrap();
    let snapshot = server.native_run(Args { op:Op::Status {launch_id:id.clone()} }).await.unwrap();
    let value = serde_json::to_value(snapshot).unwrap();
    assert_eq!(value["worker_terminal"]["inspection"]["result"]["retirement"]["errors"][0], "original status failure");
    assert_eq!(value["worker_terminal"]["inspection"]["worker_lifetime_verified"], false);
    std::fs::rename(&control,dir.join("original-control")).unwrap();
    std::fs::create_dir(&control).unwrap();
    let value = serde_json::to_value(server.native_run(Args { op:Op::Status {launch_id:id.clone()} }).await.unwrap()).unwrap();
    assert_eq!(value["response"], "recorded_status");
    assert_eq!(value["worker_terminal"]["state"], "unavailable");
    assert_eq!(std::fs::read(runtime::journal_path(&dir)).unwrap(),journal_before);
    for state in [RuntimeState::Ready, RuntimeState::Stopped] {
        let mut terminal = record.clone();
        terminal.state = state;
        runtime::save_runtime(&dir, &terminal).unwrap();
        let error = server
            .native_run(Args {
                op: Op::Reattach {
                    launch_id: id.clone(),
                    request_id: RequestId::new("reattach-after-stop").unwrap(),
                },
            })
            .await
            .err()
            .expect("stopped launch must refuse reattach");
        assert_eq!(error.code, "stopped");
        assert!(error.recovery.contains("saved outcomes"));
        assert_eq!(runtime::load_runtime(&dir).unwrap().unwrap(), terminal);
        assert_eq!(
            std::fs::read(runtime::journal_path(&dir)).unwrap(),
            journal_before
        );
    }
    // Acquisition can fail before native-status.json exists. Still expose the failed
    // runtime and independent worker diagnosis without inventing a ready target.
    let mut failed = record.clone();
    failed.state = RuntimeState::Failed { reason: "worker exited before readiness".into(), error: None };
    failed.recorder.started.push_str("-other-generation");
    runtime::save_runtime(&dir, &failed).unwrap();
    std::fs::remove_file(dir.join("native-status.json")).unwrap();
    let value = serde_json::to_value(server.native_run(Args { op:Op::Status {launch_id:id.clone()} }).await.unwrap()).unwrap();
    assert_eq!(value["response"], "recorded_status");
    assert!(value["status"].is_null());
    assert_eq!(value["runtime"]["state"]["reason"], "worker exited before readiness");
    assert_eq!(value["worker_terminal"]["state"], "unavailable");
    assert_eq!(std::fs::read(runtime::journal_path(&dir)).unwrap(),journal_before);
    for wrong_file in ["runtime", "status"] {
        let mut foreign_record = record.clone();
        let mut foreign_status = status.clone();
        let foreign = LaunchId::new("g-foreign").unwrap();
        if wrong_file == "runtime" {
            foreign_record.launch_id = foreign;
        } else {
            foreign_status.launch_id = foreign;
        }
        runtime::save_runtime(&dir, &foreign_record).unwrap();
        save_atomic(&dir.join("native-status.json"), &foreign_status).unwrap();
        let runtime_before = std::fs::read(runtime::runtime_path(&dir)).unwrap();
        let status_before = std::fs::read(dir.join("native-status.json")).unwrap();
        for kind in ["status", "result", "reattach"] {
            let mut op = serde_json::json!({"type":kind,"launch_id":id});
            if kind != "status" {
                op["request_id"] = serde_json::json!("stop");
            }
            let error = server
                .native_run(serde_json::from_value(serde_json::json!({"op":op})).unwrap())
                .await
                .err()
                .expect("foreign saved evidence must be rejected");
            assert_eq!(error.code, "recorder_fault", "{wrong_file}/{kind}");
            assert_eq!(
                std::fs::read(runtime::runtime_path(&dir)).unwrap(),
                runtime_before
            );
            assert_eq!(
                std::fs::read(dir.join("native-status.json")).unwrap(),
                status_before
            );
            assert_eq!(
                std::fs::read(runtime::journal_path(&dir)).unwrap(),
                journal_before
            );
        }
    }
}
