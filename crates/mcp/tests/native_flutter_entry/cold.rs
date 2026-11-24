use super::*;
use std::path::Path;

async fn wait_marker(path: &Path) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn original(mcp: &mut Mcp, launch: &str, request: &str) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let value = native(
                mcp,
                json!({"type":"result","launch_id":launch,"request_id":request}),
            )
            .await;
            if value["result"]["result"]["state"] == "completed" {
                break value;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_ios_entry_preserves_pending_authority_and_frontend_recovery() {
    run("frontend").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_ios_begin_recovers_original_acceptance_after_recorder_loss() {
    run("begin-death").await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_ios_poll_recovers_original_readiness_without_publishing_an_epoch() {
    run("poll-death").await;
}

async fn run(mode: &str) {
    let workspace = support::workspace();
    let ws = workspace.root();
    std::fs::create_dir_all(&ws).unwrap();
    let worker = ws.join("worker.py");
    let http = ws.join("http.py");
    std::fs::write(
        &worker,
        include_str!("../../../recorder/tests/fixtures/native/flutter-worker.py"),
    )
    .unwrap();
    std::fs::write(
        &http,
        include_str!("../../../recorder/tests/fixtures/native/flutter-http.py"),
    )
    .unwrap();
    std::fs::write(
        ws.join("system-catalog.json"),
        include_str!("../../../core/tests/fixtures/ios-system-ui.json"),
    )
    .unwrap();
    let python =
        std::env::var("FRONTCAP_TEST_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    let settings = json!({"flutter":"/unused","project":ws,"platform":"ios","source_sha256":"b".repeat(64),
        "acquisition_timeout_s":5,"lifetime_s":60,"installed_app":standalone_product(&ws),"system_ui":system_products(&ws)});
    std::fs::write(ws.join("flutter-ios.json"),json!({"session_executable":"/unused","acquisition_budget_ms":5000,
        "target":{"runtime":"flutter","platform":"ios","config":{"worker":{"python":python,"worker_script":worker,
            "config":settings},"http_script":http}}}).to_string()).unwrap();
    let mut mcp = Mcp::start_in(&ws).await;
    let launch = native(
        &mut mcp,
        json!({"type":"launch","runtime":"flutter","platform":"ios","request_id":"launch"}),
    )
    .await;
    let id = launch["launch_id"].as_str().unwrap().to_owned();
    assert!(
        launch["result"]["body"]["status"]["capabilities"]
            .as_array()
            .unwrap()
            .contains(&json!("flutter_entry"))
    );
    let runtime = ws.join("runtimes").join(&id);
    let target = runtime.join("native-session");
    let journal = runtime.join("journal.jsonl");
    let generation = "a".repeat(48);
    let old = native(
        &mut mcp,
        json!({"type":"observe","launch_id":id,"request_id":"old"}),
    )
    .await;
    let old_surface =
        old["result"]["body"]["outcome"]["completion"]["surface"]["observation"].clone();
    let begin = json!({"type":"begin_flutter_entry","launch_id":id,"request_id":"entry","generation":generation,"url":"client-test://item/1","budget_ms":5000});
    if mode != "poll-death" {
        std::fs::write(ws.join("hold-entry"), b"").unwrap();
        let unread = mcp
            .send(
                "tools/call",
                json!({"name":"native","arguments":{"op":begin}}),
            )
            .await;
        wait_marker(&ws.join("entry-dispatched")).await;
        if mode == "begin-death" {
            frontcap_recorder::runtime::load_runtime(&runtime)
                .unwrap()
                .unwrap()
                .recorder
                .kill()
                .unwrap();
        }
        mcp.kill().await;
        drop(unread);
        mcp = Mcp::start_in(&ws).await;
        if mode == "begin-death" {
            recover_dead(
                &mut mcp,
                &id,
                "entry",
                &target,
                &journal,
                &ws.join("hold-entry"),
                false,
            )
            .await;
            assert_eq!(
                std::fs::read_to_string(target.join("session/worker/entry-effects.jsonl"))
                    .unwrap()
                    .lines()
                    .count(),
                1
            );
            mcp.kill().await;
            return;
        }
        std::fs::remove_file(ws.join("hold-entry")).unwrap();
        original(&mut mcp, &id, "entry").await;
        let status = native(
            &mut mcp,
            json!({"type":"reattach","launch_id":id,"request_id":"reattach"}),
        )
        .await;
        assert_eq!(
            status["result"]["body"]["status"]["ready"]["platform"],
            "flutter_entering"
        );
    } else {
        native(&mut mcp, begin.clone()).await;
    }
    let before = std::fs::read(&journal).unwrap();
    for op in [
        json!({"type":"observe","request_id":"no-observe"}),
        json!({"type":"read_clock","request_id":"no-clock"}),
        json!({"type":"advance","request_id":"no-advance","by_ms":1}),
        json!({"type":"restart_flutter","request_id":"no-restart","generation":generation}),
        json!({"type":"begin_flutter_entry","request_id":"no-begin","generation":generation,"url":"client-test://item/2"}),
        json!({"type":"poll_flutter_entry","request_id":"wrong-entry","entry_request_id":"foreign","generation":generation}),
    ] {
        let mut op = op;
        op["launch_id"] = json!(id);
        assert!(mcp.try_tool("native", json!({"op":op})).await.0);
    }
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    let waiting=native(&mut mcp,json!({"type":"poll_flutter_entry","launch_id":id,"request_id":"waiting","entry_request_id":"entry","generation":generation})).await;
    assert_eq!(
        waiting["result"]["body"]["outcome"]["progress"]["state"],
        "waiting"
    );
    let prompt = native(
        &mut mcp,
        json!({"type":"observe_system_alerts","launch_id":id,"request_id":"prompt"}),
    )
    .await;
    let catalog =
        &prompt["result"]["body"]["outcome"]["completion"]["receipt"]["outcome"]["catalog"];
    native(&mut mcp,json!({"type":"tap_system_button","launch_id":id,"request_id":"open","observation":catalog["observation"],
        "reference":catalog["alerts"][0]["buttons"][1]["reference"]})).await;
    let before_ready = native(
        &mut mcp,
        json!({"type":"observe_system_alerts","launch_id":id,"request_id":"before-ready"}),
    )
    .await;
    let poll = json!({"type":"poll_flutter_entry","launch_id":id,"request_id":"ready","entry_request_id":"entry","generation":generation,"budget_ms":5000});
    if mode == "poll-death" {
        std::fs::remove_file(ws.join("entry-polled")).unwrap();
        std::fs::write(ws.join("hold-entry-poll"), b"").unwrap();
        let unread = mcp
            .send(
                "tools/call",
                json!({"name":"native","arguments":{"op":poll}}),
            )
            .await;
        wait_marker(&ws.join("entry-polled")).await;
        frontcap_recorder::runtime::load_runtime(&runtime)
            .unwrap()
            .unwrap()
            .recorder
            .kill()
            .unwrap();
        mcp.kill().await;
        drop(unread);
        mcp = Mcp::start_in(&ws).await;
        recover_dead(
            &mut mcp,
            &id,
            "ready",
            &target,
            &journal,
            &ws.join("hold-entry-poll"),
            true,
        )
        .await;
        mcp.kill().await;
        return;
    }
    let finished = native(&mut mcp, poll.clone()).await;
    let replacement = &finished["result"]["body"]["outcome"]["progress"]["replacement"];
    assert_eq!(replacement["previous"]["generation"], generation);
    let next = replacement["owner"]["generation"].as_str().unwrap();
    assert_ne!(next, generation);
    let before = std::fs::read(&journal).unwrap();
    let obsolete =
        &before_ready["result"]["body"]["outcome"]["completion"]["receipt"]["outcome"]["catalog"];
    assert!(mcp.try_tool("native",json!({"op":{"type":"tap_system_button","launch_id":id,"request_id":"old-system",
        "observation":obsolete["observation"],"reference":obsolete["alerts"][0]["buttons"][1]["reference"]}})).await.0);
    for g in [generation.as_str(), next] {
        assert!(mcp.try_tool("native",json!({"op":{"type":"flutter","launch_id":id,"request_id":format!("old-{g}"),"generation":g,
            "operation":{"operation":"tap","observation":old_surface,"target":"s1"}}})).await.0);
    }
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    let fresh = native(
        &mut mcp,
        json!({"type":"observe","launch_id":id,"request_id":"fresh"}),
    )
    .await;
    native(&mut mcp, begin.clone()).await;
    native(&mut mcp, poll).await;
    for request in ["old", "waiting", "entry", "ready"] {
        original(&mut mcp, &id, request).await;
    }
    let mut changed = begin;
    changed["url"] = json!("client-test://item/2");
    assert!(mcp.try_tool("native", json!({"op":changed})).await.0);
    native(&mut mcp,json!({"type":"flutter","launch_id":id,"request_id":"fresh-input","generation":next,
        "operation":{"operation":"tap","observation":fresh["result"]["body"]["outcome"]["completion"]["surface"]["observation"],"target":"s1"}})).await;
    let ordinary=native(&mut mcp,json!({"type":"restart_flutter","launch_id":id,"request_id":"ordinary-after-entry","generation":next})).await;
    let last = &ordinary["result"]["body"]["outcome"]["replacement"]["owner"]["generation"];
    native(&mut mcp,json!({"type":"begin_flutter_entry","launch_id":id,"request_id":"pending-stop","generation":last,"url":"client-test://item/2"})).await;
    native(
        &mut mcp,
        json!({"type":"stop","launch_id":id,"request_id":"stop"}),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(target.join("session/worker/entry-effects.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(
        std::fs::read_to_string(target.join("session/worker/entry-polls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(
        native(
            &mut mcp,
            json!({"type":"result","launch_id":id,"request_id":"ready"})
        )
        .await["result"]["result"]["answer"]["body"]["body"]["outcome"],
        finished["result"]["body"]["outcome"]
    );
    mcp.kill().await;
}
async fn recover_dead(
    mcp: &mut Mcp,
    id: &str,
    request: &str,
    target: &Path,
    journal: &Path,
    hold: &Path,
    poll: bool,
) {
    let history = target.join("restarts/history.json");
    let before_journal = std::fs::read(journal).unwrap();
    let before_history = std::fs::read(&history).unwrap();
    let state: Value = serde_json::from_slice(&before_history).unwrap();
    let entry = state["entries"].as_array().unwrap().last().unwrap();
    let number = if poll {
        entry["cold"]["polls"].as_array().unwrap().last().unwrap()["command_number"]
            .as_u64()
            .unwrap()
    } else {
        entry["command_number"].as_u64().unwrap()
    };
    let path = target
        .join("session/worker/commands")
        .join(number.to_string())
        .join("command.log");
    std::fs::rename(&path, path.with_extension("held")).unwrap();
    let pending = native(
        mcp,
        json!({"type":"result","launch_id":id,"request_id":request}),
    )
    .await;
    assert_eq!(
        pending["result"]["result"],
        json!({"state":"pending","dispatched":true})
    );
    std::fs::rename(path.with_extension("held"), &path).unwrap();
    std::fs::remove_file(hold).unwrap();
    let result = original(mcp, id, request).await;
    let outcome = &result["result"]["result"]["answer"]["body"]["body"]["outcome"];
    assert_eq!(
        outcome["outcome"],
        if poll {
            "flutter_entry_polled"
        } else {
            "flutter_entry_started"
        }
    );
    if poll {
        assert_eq!(outcome["progress"]["state"], "ready");
    }
    let admission = target.join("session/worker/entry-admissions").join(format!(
        "{}.json",
        entry["command_number"].as_u64().unwrap()
    ));
    let original_admission = std::fs::read(&admission).unwrap();
    std::fs::write(&admission, b"changed declaration evidence").unwrap();
    assert!(
        mcp.try_tool(
            "native",
            json!({"op":{"type":"result","launch_id":id,"request_id":request}})
        )
        .await
        .0
    );
    std::fs::write(&admission, original_admission).unwrap();
    let binding = path.with_file_name("request.json");
    let original_binding = std::fs::read(&binding).unwrap();
    let mut changed: Value = serde_json::from_slice(&original_binding).unwrap();
    if poll {
        changed["begin_command"] = json!(999);
    } else {
        changed["url"] = json!("client-test://item/2");
    }
    std::fs::write(&binding, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(
        mcp.try_tool(
            "native",
            json!({"op":{"type":"result","launch_id":id,"request_id":request}})
        )
        .await
        .0
    );
    std::fs::write(&binding, original_binding).unwrap();
    assert_eq!(std::fs::read(journal).unwrap(), before_journal);
    assert_eq!(std::fs::read(&history).unwrap(), before_history);
    assert!(!target.join("epochs").exists());
    assert!(
        mcp.try_tool(
            "native",
            json!({"op":{"type":"reattach","launch_id":id,"request_id":"no-adoption"}})
        )
        .await
        .0
    );
    assert_eq!(original(mcp, id, request).await, result);
}
