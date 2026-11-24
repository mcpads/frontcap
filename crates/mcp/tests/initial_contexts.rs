//! Explicit construction topology and native session seeds through public tools.
mod support;
use serde_json::json;

#[tokio::test]
async fn launch_restores_named_noopener_storage_before_the_first_script() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let origin = "https://initial-context.test";
    let area = |value: &str| json!({
        "origin":origin,
        "entries":[{"key":b"owner".to_vec(),
            "value":value.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>()}]
    });
    let inputs = json!({
        "url":format!("{origin}/root"),
        "profile":{
            "initial_window_name":"launch-root",
            "initial_contexts":[{"opener":"page","url":format!("{origin}/child"),"name":"restored","no_opener":true}],
            "initial_dom_storage":{"encoding":"chromium_little_endian","local":[],
                "sessions":[{"context":"popup-1","areas":[area("child")]},{"context":"page","areas":[area("parent")]}]}
        },
        "rules":[
            {"id":"root","match":{"url":format!("{origin}/root")},"action":{"type":"mock","status":200,
                "headers":{"content-type":"text/html"},"body":"<!doctype html><body><script>window.first=[name,sessionStorage.getItem('owner')];document.body.textContent=first.join(':')</script>"}},
            {"id":"child","match":{"url":format!("{origin}/child")},"action":{"type":"mock","status":200,
                "headers":{"content-type":"text/html"},"body":"<!doctype html><body><script>document.body.textContent=[name,!!opener,sessionStorage.getItem('owner')].join(':')</script>"}}
        ]
    });
    let launched = m.call_tool("launch", inputs.clone()).await;
    let id = &launched["launch_id"];
    let contexts = m.call_tool("session", json!({"launch_id":id,"operation":"contexts"})).await;
    assert_eq!(contexts["contexts"].as_array().unwrap().len(), 2, "{contexts}");
    assert_eq!(contexts["contexts"][1]["no_opener"], true, "{contexts}");
    let root = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"first"})).await;
    assert_eq!(root["value"], json!(["launch-root","parent"]), "{root}");
    let popup = m.call_tool("observe", json!({"launch_id":id,"context":"popup-1","op":{"type":"snapshot"}})).await;
    assert!(popup["nodes"].to_string().contains("restored:false:child"), "{popup}");
    let saved = m.call_tool("state", json!({"operation":"save","kind":"recipe","recipe":inputs})).await;
    let described = m.call_tool("state", json!({"operation":"describe_saved","saved_id":saved["saved_id"]})).await;
    assert_eq!(described["initial_state"]["window_name"], "launch-root", "{described}");
    assert_eq!(described["initial_state"]["contexts"][0], json!({
        "context":"popup-1","opener":"page","url":format!("{origin}/child"),"name":"restored","no_opener":true,
    }), "{described}");
    let storage = &described["initial_state"]["dom_storage"];
    assert_eq!(storage["encoding"], "chromium_little_endian", "{described}");
    assert_eq!(storage["sessions"][0]["context"], "popup-1", "{described}");
    assert_eq!(storage["sessions"][0]["areas"][0]["entries"], 1, "{described}");
    // A declaration remains an independent starting point after source changes.
    m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate",
        "expression":"window.name='mutated-root';sessionStorage.setItem('owner','changed');true"})).await;
    let branch = m.call_tool("state", json!({"operation":"branch","kind":"recipe",
        "saved_id":saved["saved_id"],"request_id":"context-recipe-child"})).await;
    assert_eq!(branch["phase"], "ready", "{branch}");
    let child = &branch["launch_id"];
    let restored = m.call_tool("debug", json!({"launch_id":child,"operation":"evaluate",
        "expression":"[first,sessionStorage.getItem('owner')]"})).await;
    assert_eq!(restored["value"], json!([["launch-root","parent"],"parent"]), "{restored}");
    let restored_popup = m.call_tool("observe", json!({"launch_id":child,"context":"popup-1",
        "op":{"type":"snapshot"}})).await;
    assert!(restored_popup["nodes"].to_string().contains("restored:false:child"), "{restored_popup}");
    m.call_tool("stop", json!({"launch_id":child})).await;
    // Saved from the live generation, the windows and their storage are the current ones.
    let current = m.call_tool("state", json!({"operation":"save","kind":"recipe","from":id})).await;
    let described = m.call_tool("state", json!({"operation":"describe_saved","saved_id":current["saved_id"]})).await;
    assert_eq!(described["initial_state"]["window_name"], "mutated-root", "{described}");
    assert_eq!(described["initial_state"]["contexts"][0]["name"], "restored", "{described}");
    let branch = m.call_tool("state", json!({"operation":"branch","kind":"recipe",
        "saved_id":current["saved_id"],"request_id":"live-context-recipe-child"})).await;
    assert_eq!(branch["phase"], "ready", "{branch}");
    let child = &branch["launch_id"];
    let restored = m.call_tool("debug", json!({"launch_id":child,"operation":"evaluate","expression":"first"})).await;
    assert_eq!(restored["value"], json!(["mutated-root","changed"]), "{restored}");
    let restored_popup = m.call_tool("observe", json!({"launch_id":child,"context":"popup-1",
        "op":{"type":"snapshot"}})).await;
    assert!(restored_popup["nodes"].to_string().contains("restored:false:child"), "{restored_popup}");
    m.call_tool("stop", json!({"launch_id":child})).await;
    m.call_tool("stop", json!({"launch_id":id})).await;
}
