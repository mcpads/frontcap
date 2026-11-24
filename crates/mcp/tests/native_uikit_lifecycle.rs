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
async fn uikit_host_roundtrip_blocks_app_work_and_recovers_after_frontend_loss() {
    lifecycle(false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uikit_host_recovery_requires_original_process_and_source_evidence_after_recorder_death() {
    lifecycle(true).await;
}
async fn lifecycle(recorder_dies: bool) {
    let workspace = support::workspace();
    let ws = workspace.root();
    std::fs::create_dir_all(&ws).unwrap();
    let worker = ws.join("session.py");
    std::fs::write(
        &worker,
        include_str!("../../recorder/tests/fixtures/native/uikit-session.py"),
    )
    .unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        ws.join("system-alerts.json"),
        include_bytes!("../../core/tests/fixtures/ios-system-ui.json"),
    )
    .unwrap();
    let mut config: Value = serde_json::from_str(include_str!(
        "../../recorder/tests/fixtures/native/ios-launch.json"
    ))
    .unwrap();
    config["surface"] = json!("uikit");
    std::fs::write(ws.join("native-ios.json"), json!({"target":{"platform":"ios","config":config},"session_executable":worker,"acquisition_budget_ms":5000}).to_string()).unwrap();
    let mut mcp = Mcp::start_in(&ws).await;
    let launch = native(
        &mut mcp,
        json!({"type":"launch","platform":"ios","request_id":"launch"}),
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
    let target = runtime.join("native-session/target");
    let journal = runtime.join("journal.jsonl");
    let old = native(
        &mut mcp,
        json!({"type":"observe","launch_id":id,"request_id":"before"}),
    )
    .await;
    let catalog = &old["result"]["body"]["outcome"]["completion"]["receipt"]["outcome"]["catalog"];
    let background = json!({"type":"set_app_state","launch_id":id,"request_id":"background","generation":owner["app"]["generation"],"state":"background","budget_ms":5000});
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
    let original_store = std::fs::read(target.join("requests/requests.json")).unwrap();
    let mut mcp = Mcp::start_in(&ws).await;
    let lookup = json!({"type":"result","launch_id":id,"request_id":"background"});
    let proof = target.join("app-state/background/after.json");
    if recorder_dies {
        let original = std::fs::read(&proof).unwrap();
        std::fs::remove_file(&proof).unwrap();
        assert_eq!(
            native(&mut mcp, lookup.clone()).await["result"]["result"],
            json!({"state":"pending","dispatched":true})
        );
        let mut changed: Value = serde_json::from_slice(&original).unwrap();
        changed["started"] = json!("replacement process with the same PID");
        std::fs::write(&proof, changed.to_string()).unwrap();
        assert!(mcp.try_tool("native", json!({"op":lookup})).await.0);
        std::fs::write(&proof, original).unwrap();
        let store: Value = serde_json::from_slice(&original_store).unwrap();
        let digest =
            store["bindings"]["background"]["progress"]["receipt"]["receipt"]["receipt_sha256"]
                .as_str()
                .unwrap();
        let raw_path = target
            .join("ui-client/receipts")
            .join(format!("{digest}.json"));
        let raw = std::fs::read(&raw_path).unwrap();
        std::fs::write(&raw_path, b"changed native source").unwrap();
        assert!(mcp.try_tool("native", json!({"op":lookup})).await.0);
        std::fs::write(raw_path, raw).unwrap();
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
    assert_eq!(
        transition["completion"]["outcome"]["change"]["now_ns"],
        Value::Null
    );
    if recorder_dies {
        published(&runtime.join("native-session/result.json")).await;
        assert_eq!(std::fs::read(&journal).unwrap(), original_journal);
        assert_eq!(
            std::fs::read(target.join("requests/requests.json")).unwrap(),
            original_store
        );
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
        for operation in [
            json!({"type":"read_clock"}),
            json!({"type":"observe"}),
            json!({"type":"advance","by_ms":1}),
            json!({"type":"restart_uikit","generation":owner["app"]["generation"]}),
            json!({"type":"set_app_state","generation":owner["runner"]["generation"],"state":"foreground"}),
        ] {
            let mut operation = operation;
            operation["launch_id"] = json!(id);
            operation["request_id"] = json!("rejected");
            assert!(mcp.try_tool("native", json!({"op":operation})).await.0);
        }
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        native(
            &mut mcp,
            json!({"type":"observe","launch_id":id,"request_id":"before"}),
        )
        .await;
        native(
            &mut mcp,
            json!({"type":"observe_system_alerts","launch_id":id,"request_id":"background-system"}),
        )
        .await;
        let foreground = native(&mut mcp, json!({"type":"set_app_state","launch_id":id,"request_id":"foreground","generation":owner["app"]["generation"],"state":"foreground"})).await;
        assert_eq!(
            foreground["result"]["body"]["outcome"]["transition"]["owner"],
            *owner
        );
        let before = std::fs::read(&journal).unwrap();
        assert!(mcp.try_tool("native", json!({"op":{"type":"tap_target","launch_id":id,"request_id":"stale","observation":catalog["observation"],"reference":catalog["targets"][0]["reference"]}})).await.0);
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        native(
            &mut mcp,
            json!({"type":"observe","launch_id":id,"request_id":"fresh"}),
        )
        .await;
        native(
            &mut mcp,
            json!({"type":"stop","launch_id":id,"request_id":"stop"}),
        )
        .await;
    }
    assert_eq!(native(&mut mcp, lookup).await, saved);
    assert_eq!(
        std::fs::read_to_string(target.join("state-effects.jsonl"))
            .unwrap()
            .lines()
            .count(),
        if recorder_dies { 1 } else { 2 }
    );
    mcp.kill().await;
}
