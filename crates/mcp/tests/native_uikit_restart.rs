mod support;
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};
use support::Mcp;

async fn native(mcp: &mut Mcp, op: Value) -> Value {
    eprintln!("native {op}");
    mcp.call_tool("native", json!({"op":op})).await
}
fn catalog(result: &Value) -> &Value {
    &result["result"]["body"]["outcome"]["completion"]["receipt"]["outcome"]["catalog"]
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
async fn image(mcp: &mut Mcp, id: &str, request: &str, saved: bool) -> Value {
    if saved {
        let reply = native(
            mcp,
            json!({"type":"result","launch_id":id,"request_id":request}),
        )
        .await;
        let artifact = &reply["result"]["result"]["answer"]["body"]["body"]["outcome"]["completion"]
            ["receipt"]["outcome"]["artifacts"][0];
        let root = frontcap_recorder::native::ios_observation_directory(
            &mcp.workspace.join("runtimes").join(id),
            &frontcap_core::ids::RequestId::new(request).unwrap(),
        )
        .unwrap();
        let bytes = std::fs::read(root.join(artifact["path"].as_str().unwrap())).unwrap();
        assert_eq!(
            artifact["sha256"],
            frontcap_core::canonical::sha256_bytes_hex(&bytes)
        );
        return artifact["sha256"].clone();
    }
    let result = mcp
        .request(
            "tools/call",
            json!({"name":"native","arguments":{"op":{
        "type":"observe","launch_id":id,"request_id":request}}}),
        )
        .await;
    assert_eq!(result["result"]["isError"], false, "{result}");
    assert!(
        result["result"]["content"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["type"] == "image")
    );
    result["result"]["structuredContent"]["result"]["body"]["outcome"]["completion"]["receipt"]["outcome"]["artifacts"][0]["sha256"].clone()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uikit_restart_preserves_history_and_invalidates_both_input_channels() {
    restart_recovery(false, None).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uikit_restart_recovers_original_proof_after_recorder_death_without_adoption() {
    restart_recovery(true, None).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uikit_cold_entry_binds_url_and_preserves_original_input_invalidation() {
    restart_recovery(false, Some("client-test://item/1")).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uikit_cold_entry_recovers_original_proof_after_recorder_death() {
    restart_recovery(true, Some("client-test://item/1")).await;
}
async fn restart_recovery(recorder_dies: bool, url: Option<&str>) {
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
    std::fs::write(
        ws.join("native-ios.json"),
        json!({"target":{"platform":"ios","config":config},
        "session_executable":worker,"acquisition_budget_ms":5000})
        .to_string(),
    )
    .unwrap();
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
            .contains(&json!("uikit_restart"))
    );
    let runtime = ws.join("runtimes").join(id);
    let target = runtime.join("native-session/target");
    let journal = runtime.join("journal.jsonl");
    let history = target.join("restarts/history.json");
    let old_image = image(&mut mcp, id, "old-image", false).await;
    native(
        &mut mcp,
        json!({"type":"advance","launch_id":id,"request_id":"old-clock","by_ms":1234}),
    )
    .await;
    native(
        &mut mcp,
        json!({"type":"observe_system_alerts","launch_id":id,"request_id":"old-prompt"}),
    )
    .await;
    let old = native(
        &mut mcp,
        json!({"type":"observe","launch_id":id,"request_id":"before-restart"}),
    )
    .await;
    let mut restart = json!({"type":"restart_uikit","launch_id":id,"request_id":"restart","generation":owner["app"]["generation"],"budget_ms":5000});
    if let Some(url)=url { restart["url"]=json!(url); }
    assert!(launch["result"]["body"]["status"]["capabilities"].as_array().unwrap().contains(&json!("uikit_entry")));
    std::fs::write(ws.join("hold-restart"), b"").unwrap();
    if recorder_dies {
        std::fs::write(ws.join("leave-history-pending"), b"").unwrap();
    }
    let unread = mcp
        .send(
            "tools/call",
            json!({"name":"native","arguments":{"op":restart}}),
        )
        .await;
    published(&ws.join("restart-published")).await;
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
    let original_history = std::fs::read(&history).unwrap();
    let mut mcp = Mcp::start_in(&ws).await;
    if recorder_dies {
        // A missing post-installation check cannot be replaced by the raw runner receipt alone.
        let proof = target.join("restarts/restart/installation-after.json");
        std::fs::rename(&proof, proof.with_extension("held")).unwrap();
        let pending = native(
            &mut mcp,
            json!({"type":"result","launch_id":id,"request_id":"restart"}),
        )
        .await;
        assert_eq!(
            pending["result"]["result"],
            json!({"state":"pending","dispatched":true})
        );
        std::fs::rename(proof.with_extension("held"), proof).unwrap();
    }
    std::fs::remove_file(ws.join("hold-restart")).unwrap();
    let saved = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let result = native(
                &mut mcp,
                json!({"type":"result","launch_id":id,"request_id":"restart"}),
            )
            .await;
            if result["result"]["result"]["state"] == "completed" {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let replacement =
        &saved["result"]["result"]["answer"]["body"]["body"]["outcome"]["replacement"];
    assert_eq!(replacement["entry"]["request"]["url"].as_str(),url);
    if let Some(url)=url {
        assert_eq!(replacement["entry"]["request"]["generation"],owner["app"]["generation"]);
        assert_eq!(replacement["entry"]["destination"],"unobserved");
        let original=std::fs::read(&journal).unwrap();
        let mut conflicting=restart.clone();conflicting["url"]=json!(format!("{url}0"));
        assert!(mcp.try_tool("native",json!({"op":conflicting})).await.0);
        assert_eq!(std::fs::read(&journal).unwrap(),original);
    }
    assert_eq!(&replacement["previous"], owner);
    assert_eq!(replacement["owner"]["runner"], owner["runner"]);
    assert_ne!(replacement["owner"]["app"], owner["app"]);
    assert_eq!(replacement["completion"]["outcome"]["now_ns"], 0);
    if recorder_dies {
        published(&runtime.join("native-session/result.json")).await;
        assert_eq!(std::fs::read(&journal).unwrap(), original_journal);
        assert_eq!(std::fs::read(&history).unwrap(), original_history);
        assert!(!target.join("epochs").exists());
        assert!(
            mcp.try_tool(
                "native",
                json!({"op":{"type":"reattach","launch_id":id,"request_id":"no-adoption"}})
            )
            .await
            .0
        );
    } else {
        let attached = native(
            &mut mcp,
            json!({"type":"reattach","launch_id":id,"request_id":"reattach"}),
        )
        .await;
        assert_eq!(
            attached["result"]["body"]["status"]["ready"]["owner"],
            replacement["owner"]
        );
        native(&mut mcp, restart.clone()).await;
        let before = std::fs::read(&journal).unwrap();
        let mut stale = restart;
        stale["request_id"] = json!("stale-generation");
        assert!(mcp.try_tool("native", json!({"op":stale})).await.0);
        assert!(mcp.try_tool("native",json!({"op":{"type":"tap_target","launch_id":id,"request_id":"stale-app",
            "observation":catalog(&old)["observation"],"reference":catalog(&old)["targets"][0]["reference"]}})).await.0);
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        let clock = native(
            &mut mcp,
            json!({"type":"read_clock","launch_id":id,"request_id":"new-clock"}),
        )
        .await;
        assert_eq!(
            clock["result"]["body"]["outcome"]["completion"]["receipt"]["now_ns"],
            0
        );
        let fresh = native(
            &mut mcp,
            json!({"type":"observe","launch_id":id,"request_id":"fresh"}),
        )
        .await;
        for request in ["old-image", "old-clock", "old-prompt", "restart"] {
            native(
                &mut mcp,
                json!({"type":"result","launch_id":id,"request_id":request}),
            )
            .await;
        }
        native(&mut mcp,json!({"type":"tap_target","launch_id":id,"request_id":"new-input",
            "observation":catalog(&fresh)["observation"],"reference":catalog(&fresh)["targets"][0]["reference"]})).await;
        let prompt = native(
            &mut mcp,
            json!({"type":"observe_system_alerts","launch_id":id,"request_id":"new-prompt"}),
        )
        .await;
        native(
            &mut mcp,
            json!({"type":"observe_system_alerts","launch_id":id,"request_id":"old-prompt"}),
        )
        .await;
        native(&mut mcp,json!({"type":"tap_system_button","launch_id":id,"request_id":"new-system",
            "observation":catalog(&prompt)["observation"],"reference":catalog(&prompt)["alerts"][0]["buttons"][1]["reference"]})).await;
        let prompt = native(
            &mut mcp,
            json!({"type":"observe_system_alerts","launch_id":id,"request_id":"before-second"}),
        )
        .await;
        native(
            &mut mcp,
            json!({"type":"restart_uikit","launch_id":id,"request_id":"second-restart",
            "generation":replacement["owner"]["app"]["generation"],"budget_ms":5000}),
        )
        .await;
        let before = std::fs::read(&journal).unwrap();
        assert!(mcp.try_tool("native",json!({"op":{"type":"tap_system_button","launch_id":id,"request_id":"stale-system",
            "observation":catalog(&prompt)["observation"],"reference":catalog(&prompt)["alerts"][0]["buttons"][1]["reference"]}})).await.0);
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        assert_eq!(image(&mut mcp, id, "old-image", false).await, old_image);
        native(
            &mut mcp,
            json!({"type":"stop","launch_id":id,"request_id":"stop"}),
        )
        .await;
        image(&mut mcp, id, "fresh", true).await;
    }
    assert_eq!(
        native(
            &mut mcp,
            json!({"type":"result","launch_id":id,"request_id":"restart"})
        )
        .await,
        saved
    );
    assert_eq!(image(&mut mcp, id, "old-image", true).await, old_image);
    assert_eq!(
        std::fs::read_to_string(target.join("restart-effects.jsonl"))
            .unwrap()
            .lines()
            .count(),
        if recorder_dies { 1 } else { 2 }
    );
    mcp.kill().await;
}
