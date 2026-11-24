mod support;
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};
use support::Mcp;

async fn native(mcp: &mut Mcp, op: Value) -> Value {
    mcp.call_tool("native", json!({"op":op})).await
}
async fn published(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_host_state_blocks_background_app_work_and_recovers_after_frontend_loss() {
    lifecycle(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn android_host_state_recovers_only_original_process_evidence_after_recorder_death() {
    lifecycle(true).await;
}
async fn lifecycle(recorder_dies: bool) {
    let workspace = support::workspace();
    let ws = workspace.root();
    std::fs::create_dir_all(&ws).unwrap();
    let worker = ws.join("session.py");
    std::fs::write(
        &worker,
        include_str!("../../recorder/tests/fixtures/native/android-state-session.py"),
    )
    .unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(ws.join("native-android.json"), json!({
        "target":{"platform":"android","config":{"emulator":"unused","adb":"/fixture/adb","avd":"fixture","port":5580,
        "apk":"unused","apk_sha256":"b".repeat(64),"python":"python3","bridge_script":"unused","source_sha256":"a".repeat(64),
        "view_app":{"package":"org.example.client","activity":"org.example.client.Main","capture_script":"/unused",
        "clock_script":"/unused","edit_script":"/unused","tap_script":"/unused","scroll_script":"/unused"}}},
        "session_executable":worker,"acquisition_budget_ms":5000}).to_string()).unwrap();
    let mut mcp = Mcp::start_in(&ws).await;
    let launch = native(
        &mut mcp,
        json!({"type":"launch","platform":"android","request_id":"launch"}),
    )
    .await;
    let id = launch["launch_id"].as_str().unwrap();
    let owner = &launch["result"]["body"]["status"]["ready"]["owner"];
    assert!(
        launch["result"]["body"]["status"]["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("app_lifecycle"))
    );
    let runtime = ws.join("runtimes").join(id);
    let session = runtime.join("native-session");
    let journal = runtime.join("journal.jsonl");
    let background = json!({"type":"set_app_state","launch_id":id,"request_id":"background","generation":owner["generation"],"state":"background","budget_ms":5000});
    std::fs::write(ws.join("hold-state"), b"").unwrap();
    if recorder_dies {
        std::fs::write(ws.join("leave-state-pending"), b"").unwrap();
    }
    let unread = mcp
        .send(
            "tools/call",
            json!({"name":"native","arguments":{"op":background}}),
        )
        .await;
    published(&ws.join("state-published")).await;
    if recorder_dies {
        frontcap_recorder::runtime::load_runtime(&runtime)
            .unwrap()
            .unwrap()
            .recorder
            .kill()
            .unwrap();
    }
    mcp.kill().await;
    drop(unread);
    let original_journal = std::fs::read(&journal).unwrap();
    let history_path = session.join("app-state/history.json");
    let original_history = std::fs::read(&history_path).unwrap();
    let lookup = json!({"type":"result","launch_id":id,"request_id":"background"});
    let mut mcp = Mcp::start_in(&ws).await;
    if recorder_dies {
        let proof_path = session.join("device/view-app-state/background/receipt.json");
        let original_proof = std::fs::read(&proof_path).unwrap();
        std::fs::remove_file(&proof_path).unwrap();
        assert_eq!(
            native(&mut mcp, lookup.clone()).await["result"]["result"],
            json!({"state":"pending","dispatched":true})
        );
        let mut changed: Value = serde_json::from_slice(&original_proof).unwrap();
        changed["transition"]["process_after"]["start_ticks"] = json!(999);
        std::fs::write(&proof_path, changed.to_string()).unwrap();
        assert!(mcp.try_tool("native", json!({"op":lookup})).await.0);
        std::fs::write(&proof_path, &original_proof).unwrap();
        let proof: Value = serde_json::from_slice(&original_proof).unwrap();
        let number = proof["samples"]["dispatch"].as_u64().unwrap();
        let source_path = session.join(format!("device/command-{number}/exchange.json"));
        let original_source = std::fs::read(&source_path).unwrap();
        std::fs::write(&source_path, b"changed SDK command").unwrap();
        assert!(mcp.try_tool("native", json!({"op":lookup})).await.0);
        std::fs::write(source_path, original_source).unwrap();
    }
    std::fs::remove_file(ws.join("hold-state")).unwrap();
    let saved = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let result = native(&mut mcp, lookup.clone()).await;
            if result["result"]["result"]["state"] == "completed" {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let transition = &saved["result"]["result"]["answer"]["body"]["body"]["outcome"]["transition"];
    assert_eq!(&transition["owner"], owner);
    assert_eq!(transition["process_before"], transition["process_after"]);
    assert_eq!(transition["now_ns"], Value::Null);
    if recorder_dies {
        published(&session.join("result.json")).await;
        assert_eq!(std::fs::read(&journal).unwrap(), original_journal);
        assert_eq!(std::fs::read(&history_path).unwrap(), original_history);
        assert!(
            mcp.try_tool(
                "native",
                json!({"op":{"type":"reattach","launch_id":id,"request_id":"no-adoption"}})
            )
            .await
            .0
        );
    } else {
        native(
            &mut mcp,
            json!({"type":"reattach","launch_id":id,"request_id":"attach"}),
        )
        .await;
        native(&mut mcp, background).await;
        let before = std::fs::read(&journal).unwrap();
        for mut operation in [
            json!({"type":"observe"}),
            json!({"type":"read_clock"}),
            json!({"type":"advance","by_ms":1}),
            json!({"type":"restart_android_views","generation":owner["generation"]}),
            json!({"type":"dispatch_url","generation":owner["generation"],"url":"client-test://item/1"}),
            json!({"type":"set_app_state","generation":owner["generation"],"state":"background"}),
            json!({"type":"set_app_state","generation":"abcd0000-0000-0000-0000-000000000099","state":"foreground"}),
        ] {
            operation["launch_id"] = json!(id);
            operation["request_id"] = json!("rejected");
            assert!(mcp.try_tool("native", json!({"op":operation})).await.0);
        }
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        let foreground = native(&mut mcp, json!({"type":"set_app_state","launch_id":id,"request_id":"foreground","generation":owner["generation"],"state":"foreground"})).await;
        assert_eq!(
            foreground["result"]["body"]["outcome"]["transition"]["owner"],
            *owner
        );
        assert_eq!(
            foreground["result"]["body"]["outcome"]["transition"]["now_ns"],
            500000000
        );
        native(
            &mut mcp,
            json!({"type":"stop","launch_id":id,"request_id":"stop"}),
        )
        .await;
    }
    assert_eq!(native(&mut mcp, lookup).await, saved);
    assert_eq!(
        std::fs::read_to_string(session.join("state-effects.jsonl"))
            .unwrap()
            .lines()
            .count(),
        if recorder_dies { 1 } else { 2 }
    );
    mcp.kill().await;
}
