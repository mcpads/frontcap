#[path = "native_flutter_entry/host_state.rs"]
mod host_state;
#[path = "native_flutter_entry/cold.rs"]
mod cold;
mod support;
use serde_json::{json, Value};
use support::Mcp;

fn worker_result(runtime: &std::path::Path) -> std::path::PathBuf {
    let registration: Value = serde_json::from_slice(&std::fs::read(runtime.join("flutter-worker-control.json")).unwrap()).unwrap();
    std::path::PathBuf::from(registration["location"]["directory"].as_str().unwrap()).join("result.json")
}

async fn native(mcp: &mut Mcp, op: Value) -> Value { mcp.call_tool("native", json!({"op":op})).await }

fn standalone_product(root: &std::path::Path) -> Value {
    let product = root.join("Client.app");std::fs::create_dir(&product).unwrap();
    std::fs::write(product.join("Info.plist"), b"fixture").unwrap();
    json!({"product":product,"inventory":{"directories":[],"files":{"Info.plist":{
        "bytes":7,"sha256":frontcap_core::canonical::sha256_bytes_hex(b"fixture"),"executable":false}}}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flutter_restart_preserves_old_results_and_invalidates_both_input_channels() {
    restart_recovery(false, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flutter_restart_recovers_worker_publication_after_recorder_death_without_adoption() {
    restart_recovery(true, false).await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flutter_restart_android_cold_url_keeps_results_and_replaces_owner() { restart_recovery(false, true).await; }
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flutter_restart_android_cold_url_recovers_unpublished_result_without_adoption() { restart_recovery(true, true).await; }
async fn restart_recovery(recorder_dies: bool, android: bool) {
    let workspace = support::workspace();let ws = workspace.root();std::fs::create_dir_all(&ws).unwrap();
    let worker = ws.join("worker.py");let http = ws.join("http.py");
    std::fs::write(&worker, include_str!("../../recorder/tests/fixtures/native/flutter-worker.py")).unwrap();
    std::fs::write(&http, include_str!("../../recorder/tests/fixtures/native/flutter-http.py")).unwrap();
    std::fs::write(ws.join("system-catalog.json"), include_str!("../../core/tests/fixtures/ios-system-ui.json")).unwrap();
    let python = std::env::var("FRONTCAP_TEST_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    let platform = if android { "android" } else { "ios" };
    let product = if android {
        let path = ws.join("client.apk");std::fs::write(&path,b"fixture APK").unwrap();
        json!({"product":path,"apk_sha256":frontcap_core::canonical::sha256_bytes_hex(b"fixture APK"),
            "package":"org.example.client","activity":"org.example.client.Main"})
    } else { standalone_product(&ws) };
    let mut settings = json!({"flutter":"/unused","project":ws,"platform":platform,"source_sha256":"b".repeat(64),
        "acquisition_timeout_s":5,"lifetime_s":60,"installed_app":product});
    if !android { settings["system_ui"] = system_products(&ws); }
    std::fs::write(ws.join(format!("flutter-{platform}.json")), json!({"session_executable":"/unused","acquisition_budget_ms":5000,
        "target":{"runtime":"flutter","platform":platform,"config":{"worker":{"python":python,"worker_script":worker,
            "config":settings},"http_script":http}}}).to_string()).unwrap();
    let mut mcp = Mcp::start_in(&ws).await;
    let launch = native(&mut mcp, json!({"type":"launch","runtime":"flutter","platform":platform,"request_id":"launch"})).await;
    let id = launch["launch_id"].as_str().unwrap();
    assert!(launch["result"]["body"]["status"]["capabilities"].as_array().unwrap().contains(&json!("flutter_restart")));
    let runtime = ws.join("runtimes").join(id);let target = runtime.join("native-session");let journal = runtime.join("journal.jsonl");
    let old = native(&mut mcp, json!({"type":"observe","launch_id":id,"request_id":"old-observe"})).await;
    let old_surface = &old["result"]["body"]["outcome"]["completion"]["surface"]["observation"];
    native(&mut mcp, json!({"type":"flutter","launch_id":id,"request_id":"old-capture","generation":"a".repeat(48),
        "operation":{"operation":"capture","observation":old_surface}})).await;
    let old_image = std::fs::read(target.join("captures/old-capture.png")).unwrap();
    let prompt = if android { Value::Null } else { native(&mut mcp, json!({"type":"observe_system_alerts","launch_id":id,"request_id":"old-prompt"})).await };
    // Keep the app reference actionable immediately before replacement; system observation consumes it.
    let before_restart = native(&mut mcp, json!({"type":"observe","launch_id":id,"request_id":"before-restart"})).await;
    let old_surface = &before_restart["result"]["body"]["outcome"]["completion"]["surface"]["observation"];
    let mut restart = json!({"type":"restart_flutter","launch_id":id,"request_id":"restart","generation":"a".repeat(48),"budget_ms":5000});
    let before_invalid = std::fs::read(&journal).unwrap();
    let mut invalid = restart.clone();invalid["request_id"]=json!("invalid-entry");
    invalid["url"]=json!(if android { "relative" } else { "client-test://item/1" });
    assert!(mcp.try_tool("native",json!({"op":invalid})).await.0);
    assert_eq!(std::fs::read(&journal).unwrap(),before_invalid);
    if android { restart["url"]=json!("client-test://item/1"); }
    std::fs::write(ws.join("hold-restart"), b"").unwrap();
    let unread = mcp.send("tools/call", json!({"name":"native","arguments":{"op":restart}})).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !ws.join("restart-dispatched").exists() { tokio::time::sleep(std::time::Duration::from_millis(10)).await; }
    }).await.unwrap();
    if recorder_dies { frontcap_recorder::runtime::load_runtime(&runtime).unwrap().unwrap().recorder.kill().unwrap(); }
    mcp.kill().await;drop(unread);
    let original_journal = std::fs::read(&journal).unwrap();let history = target.join("restarts/history.json");
    let original_history = std::fs::read(&history).unwrap();
    let mut mcp = Mcp::start_in(&ws).await;
    let binding: Value = serde_json::from_slice(&original_history).unwrap();
    let number = binding["entries"].as_array().unwrap().last().unwrap()["command_number"].as_u64().unwrap();
    let command = target.join("session/worker/commands").join(number.to_string()).join("command.log");
    if recorder_dies {
        std::fs::rename(&command, command.with_extension("held")).unwrap();
        let pending = native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"restart"})).await;
        assert_eq!(pending["result"]["result"],json!({"state":"pending","dispatched":true}));
        std::fs::rename(command.with_extension("held"), &command).unwrap();
    }
    std::fs::remove_file(ws.join("hold-restart")).unwrap();
    let saved = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let result = native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"restart"})).await;
            if result["result"]["result"]["state"]=="completed" {break result;}
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let replacement = &saved["result"]["result"]["answer"]["body"]["body"]["outcome"]["replacement"];
    assert_eq!(replacement["previous"]["generation"],json!("a".repeat(48)));
    assert_ne!(replacement["owner"]["generation"],replacement["previous"]["generation"]);
    assert_eq!(replacement["installation"]["data_inode"],2);
    if android {
        assert_eq!(replacement["entry"]["request"]["url"],"client-test://item/1");
        assert_eq!(replacement["entry"]["request"]["generation"],"a".repeat(48));
        assert_eq!(replacement["entry"]["confirmation"],"unobserved");
        assert_eq!(replacement["entry"]["destination"],"unobserved");
        let mut changed=restart.clone();changed["url"]=json!("client-test://item/2");
        assert!(mcp.try_tool("native",json!({"op":changed})).await.0);
    }
    if recorder_dies {
        assert_eq!(std::fs::read(&journal).unwrap(), original_journal);
        assert_eq!(std::fs::read(&history).unwrap(), original_history);
        assert!(!target.join("epochs").exists());
        assert!(mcp.try_tool("native",json!({"op":{"type":"reattach","launch_id":id,"request_id":"no-adoption"}})).await.0);
    } else {
        let attached = native(&mut mcp,json!({"type":"reattach","launch_id":id,"request_id":"reattach"})).await;
        assert_eq!(attached["result"]["body"]["status"]["ready"]["owner"],replacement["owner"]);
        let generation = replacement["owner"]["generation"].as_str().unwrap();
        native(&mut mcp,restart).await; // Identical retry returns the completed replacement, not another effect.
        let before = std::fs::read(&journal).unwrap();
        for (name,request_generation) in [("old-generation","a".repeat(48)),("old-reference",generation.to_string())] {
            assert!(mcp.try_tool("native",json!({"op":{"type":"flutter","launch_id":id,"request_id":name,"generation":request_generation,
                "operation":{"operation":"tap","observation":old_surface,"target":"s1"}}})).await.0);
        }
        if !android {
            let catalog = &prompt["result"]["body"]["outcome"]["completion"]["receipt"]["outcome"]["catalog"];
            assert!(mcp.try_tool("native",json!({"op":{"type":"tap_system_button","launch_id":id,"request_id":"old-system",
            "observation":catalog["observation"],"reference":catalog["alerts"][0]["buttons"][1]["reference"]}})).await.0);
        }
        assert_eq!(std::fs::read(&journal).unwrap(),before);
        let fresh = native(&mut mcp,json!({"type":"observe","launch_id":id,"request_id":"new-observe"})).await;
        for request_id in if android { vec!["old-observe","old-capture"] } else { vec!["old-observe","old-capture","old-prompt"] } {
            native(&mut mcp,json!({"type":"result","launch_id":id,"request_id":request_id})).await;
        }
        native(&mut mcp,json!({"type":"flutter","launch_id":id,"request_id":"new-input","generation":generation,
            "operation":{"operation":"tap","observation":fresh["result"]["body"]["outcome"]["completion"]["surface"]["observation"],"target":"s1"}})).await;
        let prompt = if !android {
        let prompt = native(&mut mcp,json!({"type":"observe_system_alerts","launch_id":id,"request_id":"new-prompt"})).await;
        native(&mut mcp,json!({"type":"observe_system_alerts","launch_id":id,"request_id":"old-prompt"})).await;
        let catalog = &prompt["result"]["body"]["outcome"]["completion"]["receipt"]["outcome"]["catalog"];
        native(&mut mcp,json!({"type":"tap_system_button","launch_id":id,"request_id":"new-system",
            "observation":catalog["observation"],"reference":catalog["alerts"][0]["buttons"][1]["reference"]})).await;
        native(&mut mcp,json!({"type":"observe_system_alerts","launch_id":id,"request_id":"before-second-restart"})).await
        } else { Value::Null };
        native(&mut mcp,json!({"type":"restart_flutter","launch_id":id,"request_id":"second-restart","generation":generation,"budget_ms":5000})).await;
        if !android {
        let before = std::fs::read(&journal).unwrap();
        let catalog = &prompt["result"]["body"]["outcome"]["completion"]["receipt"]["outcome"]["catalog"];
        assert!(mcp.try_tool("native",json!({"op":{"type":"tap_system_button","launch_id":id,"request_id":"second-stale-system",
            "observation":catalog["observation"],"reference":catalog["alerts"][0]["buttons"][1]["reference"]}})).await.0);
        assert_eq!(std::fs::read(&journal).unwrap(),before);
        }
        native(&mut mcp,json!({"type":"stop","launch_id":id,"request_id":"stop"})).await;
    }
    assert_eq!(native(&mut mcp,json!({"type":"result","launch_id":id,"request_id":"restart"})).await,saved);
    native(&mut mcp,json!({"type":"result","launch_id":id,"request_id":"old-capture"})).await;
    assert_eq!(std::fs::read(target.join("captures/old-capture.png")).unwrap(),old_image);
    let effects = std::fs::read_to_string(target.join("session/worker/restart-effects.jsonl")).unwrap();
    assert_eq!(effects.lines().count(),if recorder_dies {1} else {2});
    mcp.kill().await;
}

fn system_products(root: &std::path::Path) -> Value {
    use std::{collections::{BTreeMap, BTreeSet}, os::unix::fs::PermissionsExt};
    let products = root.join("products");
    let mut files = BTreeMap::new(); let mut directories = BTreeSet::new();
    for name in ["SystemUI.xctestrun", "Debug-iphonesimulator/SystemUI.app/Info.plist",
        "Debug-iphonesimulator/SystemUI.app/SystemUI", "Debug-iphonesimulator/SystemUITests-Runner.app/Info.plist",
        "Debug-iphonesimulator/SystemUITests-Runner.app/SystemUITests-Runner",
        "Debug-iphonesimulator/SystemUITests-Runner.app/PlugIns/SystemUITests.xctest/Info.plist",
        "Debug-iphonesimulator/SystemUITests-Runner.app/PlugIns/SystemUITests.xctest/SystemUITests"] {
        let path = products.join(name); std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"fixture").unwrap();
        let executable = !name.ends_with(".plist") && !name.ends_with(".xctestrun");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 })).unwrap();
        files.insert(name, json!({"bytes":7,"sha256":frontcap_core::canonical::sha256_bytes_hex(b"fixture"),"executable":executable}));
        let mut parent = std::path::Path::new(name).parent();
        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) { directories.insert(p.to_str().unwrap()); parent = p.parent(); }
    }
    json!({"xcodebuild":"/unused", "products":products, "manifest":{"role":"external_runner", "xctestrun":"SystemUI.xctestrun",
        "names":{"app":"SystemUI","tests":"SystemUITests"},"files":files,"directories":directories}})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flutter_system_input_invalidates_both_channels_and_recovers_one_original_effect() {
    system_input_recovery(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flutter_system_result_survives_recorder_death_without_reacquiring_input_authority() {
    system_input_recovery(true).await;
}

async fn system_input_recovery(recorder_dies: bool) {
    let workspace = support::workspace(); let ws = workspace.root(); std::fs::create_dir_all(&ws).unwrap();
    let worker = ws.join("worker.py"); let http = ws.join("http.py");
    std::fs::write(&worker, include_str!("../../recorder/tests/fixtures/native/flutter-worker.py")).unwrap();
    std::fs::write(&http, include_str!("../../recorder/tests/fixtures/native/flutter-http.py")).unwrap();
    std::fs::write(ws.join("system-catalog.json"), include_str!("../../core/tests/fixtures/ios-system-ui.json")).unwrap();
    let python = std::env::var("FRONTCAP_TEST_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
    std::fs::write(ws.join("flutter-ios.json"), json!({"session_executable":"/unused", "acquisition_budget_ms":5000,
        "target":{"runtime":"flutter","platform":"ios","config":{"worker":{"python":python,"worker_script":worker,
        "config":{"flutter":"/unused","project":ws,"platform":"ios","source_sha256":"b".repeat(64),
            "acquisition_timeout_s":5,"lifetime_s":60,"system_ui":system_products(&ws)}},"http_script":http}}}).to_string()).unwrap();
    let mut mcp = Mcp::start_in(&ws).await;
    let launch = native(&mut mcp, json!({"type":"launch","runtime":"flutter","platform":"ios","request_id":"launch"})).await;
    let id = launch["launch_id"].as_str().unwrap();
    assert!(launch["result"]["body"]["status"]["capabilities"].as_array().unwrap().contains(&json!("observed_system_alerts")));
    let runtime = ws.join("runtimes").join(id); let journal = runtime.join("journal.jsonl");
    let app = native(&mut mcp, json!({"type":"observe","launch_id":id,"request_id":"app"})).await;
    let prompt = native(&mut mcp, json!({"type":"observe_system_alerts","launch_id":id,"request_id":"prompt"})).await;
    let tap = |request_id: &str, response: &Value| {
        let catalog = &response["result"]["body"]["outcome"]["completion"]["receipt"]["outcome"]["catalog"];
        json!({"type":"tap_system_button","launch_id":id,"request_id":request_id,
            "observation":catalog["observation"],"reference":catalog["alerts"][0]["buttons"][1]["reference"]})
    };
    let original = std::fs::read(&journal).unwrap();
    let stale_app = json!({"type":"flutter","launch_id":id,"request_id":"stale-app","generation":"a".repeat(48),
        "operation":{"operation":"tap","observation":app["result"]["body"]["outcome"]["completion"]["surface"]["observation"],"target":"s1"}});
    assert!(mcp.try_tool("native", json!({"op":stale_app})).await.0);
    assert_eq!(std::fs::read(&journal).unwrap(), original);
    native(&mut mcp, json!({"type":"observe","launch_id":id,"request_id":"new-app"})).await;
    assert!(mcp.try_tool("native", json!({"op":tap("stale-system", &prompt)})).await.0);
    let url = json!({"type":"dispatch_url","launch_id":id,"request_id":"url","generation":"a".repeat(48),"url":"client-test://item/1"});
    let prompt2 = native(&mut mcp, json!({"type":"observe_system_alerts","launch_id":id,"request_id":"prompt-two"})).await;
    native(&mut mcp, url).await;
    assert!(mcp.try_tool("native", json!({"op":tap("pre-url", &prompt2)})).await.0);
    let prompt3 = native(&mut mcp, json!({"type":"observe_system_alerts","launch_id":id,"request_id":"prompt-three"})).await;
    // Reading an old observation result must not replace the current input basis.
    native(&mut mcp, json!({"type":"observe_system_alerts","launch_id":id,"request_id":"prompt"})).await;
    std::fs::write(ws.join("hold-system"), b"").unwrap(); std::fs::remove_file(ws.join("system-dispatched")).unwrap();
    let unread = mcp.send("tools/call", json!({"name":"native","arguments":{"op":tap("confirm", &prompt3)}})).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !ws.join("system-dispatched").exists() { tokio::time::sleep(std::time::Duration::from_millis(10)).await; }
    }).await.unwrap();
    if recorder_dies { frontcap_recorder::runtime::load_runtime(&runtime).unwrap().unwrap().recorder.kill().unwrap(); }
    mcp.kill().await; drop(unread);
    let original_journal = std::fs::read(&journal).unwrap();
    let mut mcp = Mcp::start_in(&ws).await;
    if recorder_dies {
        let results = runtime.join("native-session/session/worker/system-ui/results");
        // Three observations followed by one tap; missing publication stays unresolved.
        std::fs::rename(results.join("4.json"), results.join("held.json")).unwrap();
        let pending = native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"confirm"})).await;
        assert_eq!(pending["result"]["result"], json!({"state":"pending","dispatched":true}));
        std::fs::rename(results.join("held.json"), results.join("4.json")).unwrap();
    } else { native(&mut mcp, json!({"type":"reattach","launch_id":id,"request_id":"reattach"})).await; }
    std::fs::remove_file(ws.join("hold-system")).unwrap();
    let saved = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let r = native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"confirm"})).await;
            if r["result"]["result"]["state"] == "completed" { break r; }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let effects = runtime.join("native-session/session/worker/system-effects.jsonl");
    let rows: Vec<Value> = std::fs::read_to_string(&effects).unwrap().lines().map(|s| serde_json::from_str(s).unwrap()).collect();
    assert_eq!(rows.iter().filter(|r| r["operation"] == "tap_system_button").count(), 1);
    if recorder_dies {
        assert_eq!(std::fs::read(&journal).unwrap(), original_journal);
        let result = worker_result(&runtime);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !result.exists() { tokio::time::sleep(std::time::Duration::from_millis(10)).await; }
        }).await.unwrap();
    } else {
        assert!(mcp.try_tool("native", json!({"op":tap("repeat-input", &prompt3)})).await.0);
        native(&mut mcp, json!({"type":"stop","launch_id":id,"request_id":"stop"})).await;
    }
    assert_eq!(native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"confirm"})).await, saved);
    mcp.kill().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn flutter_external_entry_preserves_owner_and_original_results_without_repeated_effects() {
    for platform in ["android", "ios"] {
        let workspace = support::workspace(); let ws = workspace.root();
        std::fs::create_dir_all(&ws).unwrap();
        let worker = ws.join("worker.py"); let http = ws.join("http.py");
        std::fs::write(&worker, include_str!("../../recorder/tests/fixtures/native/flutter-worker.py")).unwrap();
        std::fs::write(&http, include_str!("../../recorder/tests/fixtures/native/flutter-http.py")).unwrap();
        let python = std::env::var("FRONTCAP_TEST_PYTHON").unwrap_or_else(|_| "/usr/bin/python3".into());
        std::fs::write(ws.join(format!("flutter-{platform}.json")), json!({
            "session_executable":"/unused", "acquisition_budget_ms":5000,
            "target":{"runtime":"flutter", "platform":platform, "config":{
                "worker":{"python":python,"worker_script":worker,"config":{
                    "flutter":"/unused", "project":ws,"platform":platform,"source_sha256":"b".repeat(64),
                    "acquisition_timeout_s":5,"lifetime_s":60}}, "http_script":http}}
        }).to_string()).unwrap();
        let mut mcp = Mcp::start_in(&ws).await;
        let launch = native(&mut mcp, json!({"type":"launch","runtime":"flutter","platform":platform,"request_id":"launch"})).await;
        let id = launch["launch_id"].as_str().unwrap();
        let status = &launch["result"]["body"]["status"];
        assert!(status["capabilities"].as_array().unwrap().contains(&json!("external_url_dispatch")));
        let owner = status["ready"]["owner"].clone();
        let generation = owner["generation"].clone();
        let before = native(&mut mcp, json!({"type":"observe","launch_id":id,"request_id":"before"})).await;
        let observation = &before["result"]["body"]["outcome"]["completion"]["surface"]["observation"];
        let runtime = ws.join("runtimes").join(id); let journal = runtime.join("journal.jsonl");
        let request = json!({"type":"dispatch_url","launch_id":id,"request_id":"entry","generation":generation,
            "url":"client-test://item/0?edit=true#missing"});
        let original = std::fs::read(&journal).unwrap();
        let mut stale = request.clone(); stale["generation"] = json!("c".repeat(48));
        assert!(mcp.try_tool("native", json!({"op":stale})).await.0);
        assert_eq!(std::fs::read(&journal).unwrap(), original);
        std::fs::write(ws.join("hold"), b"").unwrap();
        let unread = mcp.send("tools/call", json!({"name":"native","arguments":{"op":request}})).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !ws.join("dispatched").exists() { tokio::time::sleep(std::time::Duration::from_millis(10)).await; }
        }).await.unwrap();
        mcp.kill().await; drop(unread);
        let mut mcp = Mcp::start_in(&ws).await;
        let pending = native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"entry"})).await;
        assert_eq!(pending["result"]["result"], json!({"state":"pending","dispatched":true}));
        native(&mut mcp, json!({"type":"reattach","launch_id":id,"request_id":"reattach"})).await;
        std::fs::remove_file(ws.join("hold")).unwrap();
        let saved = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let value = native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"entry"})).await;
                if value["result"]["result"]["state"] == "completed" { break value; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.unwrap();
        let acceptance = &saved["result"]["result"]["answer"]["body"]["body"]["outcome"]["acceptance"];
        assert_eq!(acceptance["request"]["generation"], generation);
        assert_eq!(acceptance["confirmation"], "unobserved");
        let original = std::fs::read(&journal).unwrap();
        let tap = json!({"type":"flutter","launch_id":id,"request_id":"stale-tap","generation":generation,
            "operation":{"operation":"tap","observation":observation,"target":"s1"}});
        assert!(mcp.try_tool("native", json!({"op":tap})).await.0);
        assert_eq!(std::fs::read(&journal).unwrap(), original);
        let fresh = native(&mut mcp, json!({"type":"observe","launch_id":id,"request_id":"fresh"})).await;
        native(&mut mcp, request.clone()).await;
        let tapped = native(&mut mcp, json!({"type":"flutter","launch_id":id,"request_id":"tap","generation":generation,
            "operation":{"operation":"tap","observation":fresh["result"]["body"]["outcome"]["completion"]["surface"]["observation"],"target":"s1"}})).await;
        assert_eq!(tapped["result"]["body"]["outcome"]["completion"]["operation"], "tap");
        let worker = runtime.join("native-session/session/worker");
        assert_eq!(std::fs::read_to_string(worker.join("effects.jsonl")).unwrap().lines().count(), 1);
        std::fs::write(ws.join("lose-host-reply"), b"").unwrap();
        let mut lost = request; lost["request_id"] = json!("lost");
        let _ = mcp.try_tool("native", json!({"op":lost})).await;
        let result = native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"lost"})).await;
        assert_eq!(result["result"]["result"], json!({"state":"pending","dispatched":true}));
        assert!(mcp.try_tool("native", json!({"op":{"type":"observe","launch_id":id,"request_id":"after-loss"}})).await.0);
        assert_eq!(std::fs::read_to_string(worker.join("effects.jsonl")).unwrap().lines().count(), 2);
        native(&mut mcp, json!({"type":"stop","launch_id":id,"request_id":"stop"})).await;
        assert_eq!(native(&mut mcp, json!({"type":"result","launch_id":id,"request_id":"entry"})).await, saved);
        mcp.kill().await;
    }
}
