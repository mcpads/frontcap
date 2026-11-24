mod support;
use serde_json::{Value, json};
use std::os::unix::fs::PermissionsExt;
use support::Mcp;

fn fixture(ws: &std::path::Path) {
    std::fs::create_dir_all(ws).unwrap();
    let worker = ws.join("session.py");
    std::fs::write(
        &worker,
        include_str!("../../recorder/tests/fixtures/native/session.py"),
    )
    .unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(ws.join("native-android.json"), json!({
        "target":{"platform":"android","config":{"emulator":"unused","adb":"unused","avd":"fixture","port":5580,"apk":"unused","apk_sha256":"b".repeat(64),"python":"python3","bridge_script":"unused","source_sha256":"a".repeat(64)}},
        "session_executable":worker,"acquisition_budget_ms":5000
    }).to_string()).unwrap();
}
async fn native(mcp: &mut Mcp, op: Value) -> Value {
    eprintln!("native {op}");
    mcp.call_tool("native", json!({"op":op})).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_entry_mcp_preserves_original_requests_across_controller_loss_and_uncertainty() {
    external_entry_mcp_recovery(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uikit_external_entry_mcp_preserves_original_requests_across_controller_loss_and_uncertainty() {
    external_entry_mcp_recovery(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uikit_system_alerts_expose_images_and_admit_only_fresh_observed_buttons() {
    let workspace = support::workspace();
    let ws = workspace.root();
    std::fs::create_dir_all(&ws).unwrap();
    let worker = ws.join("ios-worker.py");
    std::fs::write(&worker, include_str!("../../recorder/tests/fixtures/native/ios-session.py")).unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(ws.join("system-alerts.json"), include_bytes!("../../core/tests/fixtures/ios-system-ui.json")).unwrap();
    let mut config: Value = serde_json::from_str(include_str!("../../recorder/tests/fixtures/native/ios-launch.json")).unwrap();
    config["surface"] = json!("uikit");
    std::fs::write(ws.join("native-ios.json"), json!({"target":{"platform":"ios","config":config},
        "session_executable":worker,"acquisition_budget_ms":5000}).to_string()).unwrap();
    let mut mcp = Mcp::start_in(&ws).await;
    let launch = native(&mut mcp, json!({"type":"launch","platform":"ios","request_id":"launch"})).await;
    let id = launch["launch_id"].as_str().unwrap();
    assert!(launch["result"]["body"]["status"]["capabilities"].as_array().unwrap().contains(&json!("observed_system_alerts")));
    let observed = mcp.request("tools/call", json!({"name":"native","arguments":{"op":{
        "type":"observe_system_alerts","launch_id":id,"request_id":"prompt"}}})).await;
    assert_eq!(observed["result"]["isError"], false, "{observed}");
    assert!(observed["result"]["content"].as_array().unwrap().iter().any(|row| row["type"] == "image"));
    let outcome = &observed["result"]["structuredContent"]["result"]["body"]["outcome"]["completion"]["receipt"]["outcome"];
    assert_eq!(outcome["outcome"], "system_alerts_observed");
    assert!(outcome.get("app").is_none() && outcome.get("now_ns").is_none());
    let input = json!({"type":"tap_system_button","launch_id":id,"request_id":"choose",
        "observation":outcome["catalog"]["observation"],"reference":outcome["catalog"]["alerts"][0]["buttons"][1]["reference"]});
    let tapped = native(&mut mcp, input.clone()).await;
    assert_eq!(tapped["result"]["body"]["outcome"]["completion"]["receipt"]["outcome"]["outcome"], "system_button_tapped");
    assert_eq!(native(&mut mcp, input.clone()).await, tapped);
    let runtime = ws.join("runtimes").join(id);
    let journal = std::fs::read(runtime.join("journal.jsonl")).unwrap();
    let mut stale = input; stale["request_id"] = json!("stale");
    let (bad, rejected) = mcp.try_tool("native", json!({"op":stale})).await;
    assert!(bad); assert_eq!(rejected["code"], "invalid_arguments");
    assert_eq!(std::fs::read(runtime.join("journal.jsonl")).unwrap(), journal);
    assert_eq!(std::fs::read_to_string(runtime.join("native-session/calls.jsonl")).unwrap().lines().count(), 2);
    native(&mut mcp, json!({"type":"stop","launch_id":id,"request_id":"stop"})).await;
    let saved = native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"choose"})).await;
    assert_eq!(saved["result"]["result"]["answer"]["body"], tapped["result"]);
    mcp.kill().await;
}

async fn external_entry_mcp_recovery(ios: bool) {
    let workspace = support::workspace();
    let ws = workspace.root();
    if ios {
        std::fs::create_dir_all(&ws).unwrap();
        let worker = ws.join("ios-worker.py");
        std::fs::write(&worker, include_str!("../../recorder/tests/fixtures/native/ios-session.py")).unwrap();
        std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut config: Value = serde_json::from_str(include_str!("../../recorder/tests/fixtures/native/ios-launch.json")).unwrap();
        config["surface"] = json!("uikit");
        std::fs::write(ws.join("native-ios.json"), json!({"target":{"platform":"ios","config":config},
            "session_executable":worker,"acquisition_budget_ms":5000}).to_string()).unwrap();
    } else {
        fixture(&ws);
        let profile = ws.join("native-android.json");
        let mut config: Value = serde_json::from_slice(&std::fs::read(&profile).unwrap()).unwrap();
        config["target"]["config"]["view_app"] = json!({
            "package":"org.example.client", "activity":"org.example.client.Main", "capture_script":"/unused",
            "clock_script":"/unused", "edit_script":"/unused", "tap_script":"/unused", "scroll_script":"/unused"
        });
        std::fs::write(&profile, serde_json::to_vec(&config).unwrap()).unwrap();
    }
    let mut mcp = Mcp::start_in(&ws).await;
    let launch = native(&mut mcp, json!({"type":"launch","request_id":"launch","platform":if ios {"ios"} else {"android"}})).await;
    let id = launch["launch_id"].as_str().unwrap().to_string();
    let status = &launch["result"]["body"]["status"];
    assert!(status["capabilities"].as_array().unwrap().contains(&json!("external_url_dispatch")));
    let generation = if ios {
        assert_eq!(status["ready"]["external_url_bundle"], "org.example.client");
        &status["ready"]["owner"]["app"]["generation"]
    } else { &status["ready"]["owner"]["generation"] };
    let op = json!({"type":"dispatch_url","launch_id":id,"request_id":"entry",
        "generation":generation,"url":"client-test://item/0?edit=true#missing"});
    let runtime = ws.join("runtimes").join(&id);
    let journal = runtime.join("journal.jsonl");
    let before = std::fs::read(&journal).unwrap();
    let mut stale = op.clone(); stale["generation"] = json!("abcd0000-0000-0000-0000-000000000009");
    let (bad, rejected) = mcp.try_tool("native", json!({"op":stale})).await;
    assert!(bad); assert_eq!(rejected["code"], "invalid_arguments");
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    let hold = runtime.join("hold"); std::fs::write(&hold, b"").unwrap();
    let unread = mcp.send("tools/call", json!({"name":"native","arguments":{"op":op}})).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !runtime.join("native-session/dispatched").exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    mcp.kill().await; drop(unread);
    let mut mcp = Mcp::start_in(&ws).await;
    let saved = native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"entry"})).await;
    assert_eq!(saved["result"]["result"], json!({"state":"pending","dispatched":true}));
    native(&mut mcp, json!({"type":"reattach","launch_id":id,"request_id":"reattach"})).await;
    assert_eq!(native(&mut mcp, op.clone()).await["result"]["result"], saved["result"]["result"]);
    std::fs::remove_file(hold).unwrap();
    let saved = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let saved = native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"entry"})).await;
            if saved["result"]["result"]["state"] == "completed" { break saved; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let answer = &saved["result"]["result"]["answer"]["body"];
    let outcome = &answer["body"]["outcome"];
    assert_eq!(outcome["outcome"], "external_url_accepted");
    assert_eq!(outcome["acceptance"]["confirmation"], "unobserved");
    assert_eq!(outcome["acceptance"]["destination"], "unobserved");
    assert_eq!(outcome["acceptance"]["request"]["target"], if ios {
        json!({"platform":"ios","simulator":"abcd0000-0000-0000-0000-000000000001","bundle":"org.example.client"})
    } else { json!({"platform":"android","serial":"emulator-5580","package":"org.example.client"}) });
    assert_eq!(outcome["acceptance"]["request"]["url"], op["url"]);
    assert_eq!(&native(&mut mcp, op.clone()).await["result"], answer);
    let mut changed = op.clone(); changed["url"] = json!("client-test://item/1");
    let (bad, conflict) = mcp.try_tool("native", json!({"op":changed})).await;
    assert!(bad); assert_eq!(conflict["code"], "request_id_conflict");
    let calls = runtime.join("native-session/calls.jsonl");
    assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);

    std::fs::write(runtime.join("fail"), b"").unwrap();
    let mut lost = op.clone(); lost["request_id"] = json!("lost-entry");
    let (bad, uncertain) = mcp.try_tool("native", json!({"op":lost})).await;
    assert!(bad); assert_eq!(uncertain["code"], "recorder_fault");
    let pending = native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"lost-entry"})).await;
    assert_eq!(pending["result"]["result"], json!({"state":"pending","dispatched":true}));
    let mut again = op.clone(); again["request_id"] = json!("another-entry");
    let (bad, fenced) = mcp.try_tool("native", json!({"op":again})).await;
    assert!(bad); assert_eq!(fenced["code"], "recorder_fault");
    assert_eq!(&native(&mut mcp, op).await["result"], answer);
    assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 2);
    native(&mut mcp, json!({"type":"stop","launch_id":id,"request_id":"stop"})).await;
    assert_eq!(native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"entry"})).await, saved);
    mcp.kill().await;
}




#[tokio::test]
async fn native_describe_exposes_complete_arguments_without_a_host_profile() {
    let workspace = support::workspace();
    let mut mcp = Mcp::start_in(&workspace.root()).await;
    let listed = mcp.request("tools/list", json!({})).await;
    let native_tool = listed["result"]["tools"].as_array().unwrap().iter()
        .find(|t| t["name"] == "native").unwrap();
    let tags = native_tool["inputSchema"]["properties"]["op"]["properties"]["type"]["enum"].as_array().unwrap();
    let described = native(&mut mcp, json!({"type":"describe"})).await;
    assert_eq!(described["response"], "description");
    let branches = described["operations"]["oneOf"].as_array().unwrap();
    let actual: Vec<_> = branches.iter().map(|v| v["properties"]["type"]["const"].clone()).collect();
    assert_eq!(&actual, tags);
    let text = branches.iter().find(|v| v["properties"]["type"]["const"] == "replace_text").unwrap();
    for required in ["launch_id", "request_id", "reference", "observation", "start", "end", "text"] {
        assert!(text["required"].as_array().unwrap().contains(&json!(required)));
    }
    assert_eq!(text["additionalProperties"], false);
    let entry = branches.iter().find(|v| v["properties"]["type"]["const"] == "dispatch_url").unwrap();
    for required in ["launch_id", "request_id", "generation", "url"] {
        assert!(entry["required"].as_array().unwrap().contains(&json!(required)));
    }
    assert_eq!(entry["additionalProperties"], false);
    let system = branches.iter().find(|v| v["properties"]["type"]["const"] == "tap_system_button").unwrap();
    for required in ["launch_id", "request_id", "observation", "reference"] {
        assert!(system["required"].as_array().unwrap().contains(&json!(required)));
    }
    assert_eq!(system["additionalProperties"], false);
    assert!(described["guidance"].as_str().unwrap().contains("native-android.json"));
    assert!(described["guidance"].as_str().unwrap().contains("unchanged request IDs"));
    let (bad, error) = mcp.try_tool("native", json!({"op":{"type":"describe","text":"unexpected"}})).await;
    assert!(bad);
    assert_eq!(error["code"], "invalid_arguments");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webview_mcp_routes_clock_recovery_and_bound_native_image() {
    let workspace = support::workspace(); let ws=workspace.root();
    std::fs::create_dir_all(&ws).unwrap();
    let worker=ws.join("webview-worker.py");
    std::fs::write(&worker,include_str!("../../recorder/tests/fixtures/native/webview-session.py")).unwrap();
    std::fs::set_permissions(&worker,std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut config:Value=serde_json::from_str(include_str!("../../recorder/tests/fixtures/native/webview-launch.json")).unwrap();
    config["contacts"]=json!(true);
    config["lifecycle"]=json!({"xcodebuild":"/unused", "products":"/unused", "manifest":{"names":{"app":"Client","tests":"ClientTests"},"xctestrun":"unused.xctestrun","directories":[],"files":{}},"app":{"bundle":"dev.frontcap.consumer","runner_bundle":"dev.frontcap.runner","test_identifier":"Tests/test","ui_descriptor":"Documents/ui.json","clock_descriptor":"Documents/clock.json"}});
    std::fs::write(ws.join("webview-ios.json"),json!({"target":{"runtime":"webview","platform":"ios","config":config},"session_executable":worker,"acquisition_budget_ms":5000}).to_string()).unwrap();
    let mut mcp=Mcp::start_in(&ws).await;
    let launch=native(&mut mcp,json!({"type":"launch","runtime":"webview","platform":"ios","request_id":"launch"})).await;
    let id=launch["launch_id"].as_str().unwrap().to_owned();
    assert_eq!(launch["result"]["body"]["status"]["ready"]["platform"],"webview");
    let (is_error, uncertain)=mcp.try_tool("native",json!({"op":{"type":"advance","launch_id":id,"request_id":"uncertain","by_ms":50}})).await;
    assert!(is_error, "unresolved execution must be an MCP error");
    assert_eq!(uncertain["result"]["body"]["outcome"]["outcome"],"webview_unresolved");
    let progress=native(&mut mcp,json!({"type":"inspect_webview_progress","launch_id":id,"request_id":"progress"})).await;
    let observation=&progress["result"]["body"]["outcome"]["completion"]["observation"];
    assert_eq!(observation["basis"],"retained_receipts");
    assert_eq!(observation["service"]["state"],"unconfigured");
    assert!(observation["contacts"].is_null());
    assert!(observation["unobserved"].as_array().unwrap().contains(&json!("service_delivery")));
    let recovered=native(&mut mcp,json!({"type":"reconcile_webview","launch_id":id,"request_id":"recovered","original_request_id":"uncertain"})).await;
    assert_eq!(recovered["result"]["body"]["outcome"]["completion"]["reply"]["now_ns"],50_000_000);
    let navigation=&recovered["result"]["body"]["outcome"]["completion"]["reply"]["navigation"];
    assert_eq!(navigation["document"],"abcd0000-0000-0000-0000-000000000003");
    assert_eq!(navigation["current"]["href"],"http://localhost/game");
    assert_eq!(navigation["events"][0]["kind"],"pageshow");
    assert_eq!(navigation["events"][0]["persisted"],false);
    assert_eq!(recovered["result"]["body"]["outcome"]["completion"]["reply"]["state"]["timerQueue"],
        json!({"timers":[],"omitted":0,"failure":null}));
    let observed=mcp.request("tools/call",json!({"name":"native","arguments":{"op":{"type":"observe","launch_id":id,"request_id":"screen"}}})).await;
    assert_eq!(observed["result"]["isError"],false);
    assert!(observed["result"]["content"].as_array().unwrap().iter().any(|row|row["type"]=="image"));
    assert!(launch["result"]["body"]["status"]["capabilities"].as_array().unwrap().contains(&json!("observed_webview_contacts")));
    assert!(launch["result"]["body"]["status"]["capabilities"].as_array().unwrap().contains(&json!("webview_lifecycle")));
    let compared=native(&mut mcp,json!({"type":"compare_webview_progress","launch_id":id,"request_id":"compare","before":"progress","after":"progress","before_capture":"screen","after_capture":"screen"})).await;
    let comparison=&compared["result"]["body"]["outcome"]["comparison"];
    assert_eq!(comparison["changes"],json!([]));
    assert_eq!(comparison["capture_content_changed"],false);
    assert_eq!(comparison["before"]["capture"]["request_id"],"screen");
    assert!(comparison["before"]["capture"].get("input_basis").is_none());
    let press=json!({"type":"webview_contacts","launch_id":id,"request_id":"press","observation":"screen","operation":{"operation":"set","expected_revision":0,"contacts":[{"id":1,"x":0.5,"y":0.5}]}});
    let pressed=native(&mut mcp,press.clone()).await;
    assert_eq!(pressed["result"]["body"]["outcome"]["completion"]["receipt"]["revision"],1);
    let (is_error, home)=mcp.try_tool("native",json!({"op":{"type":"webview_background","launch_id":id,"request_id":"home-uncertain"}})).await;
    assert!(is_error, "unresolved lifecycle execution must be an MCP error");
    assert_eq!(home["result"]["body"]["outcome"]["outcome"],"webview_unresolved");
    mcp.kill().await;
    let mut mcp=Mcp::start_in(&ws).await;
    native(&mut mcp,json!({"type":"reattach","launch_id":id,"request_id":"reattach"})).await;
    assert_eq!(native(&mut mcp,json!({"type":"reconcile_webview","launch_id":id,"request_id":"recovered","original_request_id":"uncertain"})).await,recovered);
    assert_eq!(native(&mut mcp,press).await,pressed);
    let home_result=native(&mut mcp,json!({"type":"reconcile_webview_lifecycle","launch_id":id,"request_id":"home-result","original_request_id":"home-uncertain"})).await;
    assert_eq!(home_result["result"]["body"]["outcome"]["completion"]["reply"]["result"]["app_state"],3);
    let returned=native(&mut mcp,json!({"type":"webview_foreground","launch_id":id,"request_id":"return"})).await;
    assert_eq!(returned["result"]["body"]["outcome"]["completion"]["reply"]["result"]["app_state"],4);
    let input_result=native(&mut mcp,json!({"type":"reconcile_webview_contacts","launch_id":id,"request_id":"input-result","original_request_id":"press"})).await;
    assert_eq!(input_result["result"]["body"]["outcome"]["completion"]["receipt"],pressed["result"]["body"]["outcome"]["completion"]["receipt"]);
    let read=native(&mut mcp,json!({"type":"read_clock","launch_id":id,"request_id":"read"})).await;
    assert_eq!(read["result"]["body"]["outcome"]["completion"]["reply"]["now_ns"],50_000_000);
    let generation=launch["result"]["body"]["status"]["ready"]["owner"]["endpoint"]["generation"].clone();
    let restart_op=json!({"type":"restart_webview","launch_id":id,"request_id":"restart","generation":generation});
    let restarted=native(&mut mcp,restart_op.clone()).await;
    assert_eq!(restarted["result"]["body"]["outcome"]["completion"]["kind"],"restarted");
    assert_eq!(restarted["result"]["body"]["outcome"]["completion"]["page_ready"],false);
    assert_eq!(native(&mut mcp,restart_op).await,restarted);
    assert_eq!(native(&mut mcp,json!({"type":"reconcile_webview","launch_id":id,"request_id":"recovered","original_request_id":"uncertain"})).await,recovered);
    let current=native(&mut mcp,json!({"type":"status","launch_id":id})).await;
    assert_eq!(current["result"]["body"]["status"]["ready"]["owner"]["process"]["pid"],44);
    for request in ["replacement-screen", "screen"] {
        let image=mcp.request("tools/call",json!({"name":"native","arguments":{"op":{"type":"observe","launch_id":id,"request_id":request}}})).await;
        assert_eq!(image["result"]["isError"],false,"{image}");
        assert!(image["result"]["content"].as_array().unwrap().iter().any(|row|row["type"]=="image"));
        if request == "screen" { assert_eq!(image["result"],observed["result"]); }
    }
    native(&mut mcp,json!({"type":"stop","launch_id":id,"request_id":"stop"})).await;
    assert_eq!(std::fs::read_to_string(ws.join("runtimes").join(id).join("native-session/calls.jsonl")).unwrap().lines().count(),12);
    mcp.kill().await;
}
