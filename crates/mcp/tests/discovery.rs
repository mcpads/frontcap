mod support;
use serde_json::{Value, json};

#[tokio::test]
async fn discovery_does_not_take_control_and_survives_controller_replacement() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut owner = support::Mcp::start_in(&ws.root()).await;
    let launched = owner.call_tool("launch", json!({"url": format!("http://{addr}/count.html"), "request_id": "launch"})).await;
    let id = launched["launch_id"].as_str().unwrap();
    let before = owner.call_tool("status", json!({"launch_id": id})).await;
    assert_eq!(before["ownership"], "self");
    let mut reader = support::Mcp::start_in(&ws.root()).await;
    let bootstrap = reader.call_tool("bootstrap", json!({})).await;
    assert!(bootstrap["next_action"].as_str().unwrap().contains("session list"));
    let inventory = reader.call_tool("session", json!({"operation": "list"})).await;
    let found = inventory["generations"][0]["launch_id"].as_str().unwrap();
    assert_eq!(found, id);
    assert_eq!(inventory["generations"][0]["state"], "ready");
    let status = reader.call_tool("status", json!({"launch_id": found})).await;
    assert_eq!(status["ownership"], "other");
    assert_eq!(status["t_us"], before["t_us"]);
    assert_eq!(status["observation_seq"], before["observation_seq"]);
    let (failed, rejected) = reader.try_tool("session", json!({"operation": "reattach", "launch_id": found})).await;
    assert!(failed);
    assert_eq!(rejected["code"], "lease_held");
    owner.kill().await;
    let mut available = false;
    for _ in 0..100 {
        let s = reader.call_tool("status", json!({"launch_id": id})).await;
        if s["ownership"] == "none" { available = true; break; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(available, "controller disconnect must release its lease");
    let attached = reader.call_tool("session", json!({"operation": "reattach", "launch_id": id})).await;
    assert_eq!(attached["status"]["ownership"], "self");
    assert_eq!(attached["status"]["t_us"], before["t_us"]);
    let action = json!({"launch_id": id,"request_id":"one-click","action":{"type":"click","target":{"by":"css","selector":"#ok"}},"then":{"type":"none"}});
    reader.call_tool("act", action.clone()).await;
    reader.call_tool("act", action).await;
    let value = reader.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#count"},"props":["text"]}})).await;
    assert_eq!(value["values"]["text"], "1", "retry must not click twice: {value}");
    reader.call_tool("session", json!({"operation":"release","launch_id":id})).await;
    assert_eq!(reader.call_tool("status",json!({"launch_id":id})).await["ownership"], "none");
    reader.call_tool("session", json!({"operation":"reattach","launch_id":id})).await;
    assert_eq!(reader.call_tool("stop",json!({"launch_id":id})).await["finalized"], true);
    assert_eq!(reader.call_tool("session",json!({"operation":"list"})).await["generations"][0]["state"], "stopped");
}

#[tokio::test]
async fn inventory_pages_preserve_unavailable_metadata_without_recovery() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    assert_eq!(m.call_tool("session",json!({"operation":"list"})).await["generations"], json!([]));
    for n in 0..23 {
        let dir=ws.root().join("runtimes").join(format!("generation-{n:02}"));
        std::fs::create_dir_all(&dir).unwrap();
        if n % 2 == 0 { std::fs::write(dir.join("runtime.json"), b"{broken").unwrap(); }
    }
    let first=m.call_tool("session",json!({"operation":"list"})).await;
    assert!(first["next_after"].is_string());
    let second=m.call_tool("session",json!({"operation":"list","after":first["next_after"]})).await;
    assert!(second["next_after"].is_null());
    let rows:Vec<&Value>=first["generations"].as_array().unwrap().iter().chain(second["generations"].as_array().unwrap()).collect();
    assert_eq!(rows.len(),23);
    let ids:std::collections::BTreeSet<_>=rows.iter().map(|row|row["launch_id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(),23);
    assert!(rows.iter().all(|row|row["state"]=="unavailable" && row["recorder_alive"].is_null()));
    assert!(!ws.root().join("launches").exists(),"listing must not launch or recover");
    let (failed, invalid)=m.try_tool("session",json!({"operation":"list","launch_id":"generation-00"})).await;
    assert!(failed);
    assert_eq!(invalid["code"],"invalid_arguments");
    // Lifecycle metadata stays visible even without a usable recorder. A mismatched identity is unknown.
    for (index, state) in [json!({"state":"starting"}), json!({"state":"stopped"}), json!({"state":"failed","reason":"retained failure"}), json!({"state":"ready"})].into_iter().enumerate() {
        let id = format!("generation-{index:02}");
        let path = ws.root().join("runtimes").join(&id).join("runtime.json");
        let record = json!({"launch_id":id,"state":state,
            "recorder":{"pid":std::process::id(),"started":"not this process start"},
            "browser":null,"endpoint":null,"socket":"/nonexistent/frontcap-test.sock"});
        std::fs::write(path, record.to_string()).unwrap();
    }
    let mismatch = ws.root().join("runtimes/generation-04/runtime.json");
    std::fs::copy(ws.root().join("runtimes/generation-03/runtime.json"), &mismatch).unwrap();
    let before = std::fs::read(&mismatch).unwrap();
    let listed = m.call_tool("session",json!({"operation":"list"})).await;
    let states:Vec<_>=listed["generations"].as_array().unwrap().iter().take(5).map(|r|r["state"].as_str().unwrap()).collect();
    assert_eq!(states, ["starting","stopped","failed","recorder_unavailable","unavailable"]);
    assert_eq!(std::fs::read(mismatch).unwrap(), before, "discovery preserves invalid evidence");
}
