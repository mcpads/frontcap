//! Selector discovery and watches across open roots and shadow-hosted cross-origin frames.
mod support;
use serde_json::{Value, json};

async fn launch(m: &mut support::Mcp) -> Value {
    let addr = support::serve();
    m.call_tool("launch", json!({"url":format!("http://{addr}/shadow-discovery.html"),
        "passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await
}

fn css(selector: &str) -> Value { json!({"by":"css","selector":selector}) }
fn test_id(id: &str) -> Value { json!({"by":"test_id","id":id}) }
fn child(target: Value) -> Value { json!({"by":"frame","frame":css("#editor"),"target":target}) }

async fn query(m: &mut support::Mcp, id: &Value, target: Value) -> Value {
    m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":target,"props":["text"]}})).await
}

async fn replay(m: &mut support::Mcp, id: &Value) {
    m.call_tool("stop",json!({"launch_id":id})).await;
    let result = m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(result["equal"],true,"{result}");
}

#[tokio::test]
async fn shadow_selectors_share_scope_counts_input_and_exact_predicate_stops() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = launch(&mut m).await;
    let id = &launched["launch_id"];
    assert_eq!(query(&mut m,id,css("#status-child")).await["values"]["text"],"Idle child");
    assert_eq!(query(&mut m,id,json!({"by":"text","text":"Deep child"})).await["values"]["text"],"Deep child");
    assert_eq!(query(&mut m,id,child(test_id("status"))).await["values"]["text"],"Idle child");
    let (failed,error) = m.try_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":test_id("status"),"props":["text"]}})).await;
    assert!(failed && error["code"]=="ambiguous_locator","{error}");
    let (failed,error) = m.try_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":css("#host #status-main"),"props":["text"]}})).await;
    assert!(failed && error["code"]=="not_found","selectors retain their root scope: {error}");
    // Closed roots are searched too: the page and its child frame each hold one.
    let (failed,error) = m.try_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":css("#closed-secret"),"props":["text"]}})).await;
    assert!(failed && error["message"].as_str().unwrap().starts_with("2 nodes match"),"{error}");
    assert_eq!(query(&mut m,id,child(css("#closed-secret"))).await["values"]["text"],"Closed content");
    for (n, expected) in [(2,"true"),(3,"false")] {
        let count = m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":format!("count-{n}"),
            "predicate":{"is":"count_at_least","target":test_id("status"),"n":n}})).await;
        assert_eq!(count["value"],expected);
    }
    let added = m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"saved-child",
        "predicate":{"is":"text_equals","target":child(test_id("status")),"text":"Saved child"}})).await;
    assert_eq!(added["value"],"false");
    let action = m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child(test_id("save"))},"then":{"type":"none"}})).await;
    let waited = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","watch":"saved-child","max_ms":1000}})).await;
    assert_eq!(waited["t_us"].as_u64().unwrap(),action["t_after_us"].as_u64().unwrap()+123_000);
    assert_eq!(query(&mut m,id,child(test_id("status"))).await["values"]["text"],"Saved child");
    replay(&mut m,id).await;
}

#[tokio::test]
async fn inserted_open_roots_join_existing_watches_and_replay() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = launch(&mut m).await;
    let id = &launched["launch_id"];
    let added = m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"late-ready",
        "predicate":{"is":"text_equals","target":css("#late-status"),"text":"Ready"}})).await;
    assert_eq!(added["value"],"false");
    let action = m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"text","text":"Add component"}},"then":{"type":"none"}})).await;
    let waited = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","watch":"late-ready","max_ms":1000}})).await;
    assert_eq!(waited["t_us"].as_u64().unwrap(),action["t_after_us"].as_u64().unwrap()+123_000);
    assert_eq!(query(&mut m,id,css("#late-status")).await["values"]["text"],"Ready");
    replay(&mut m,id).await;
}

#[tokio::test]
async fn shadow_hosted_frame_watches_keep_identity_through_recovery_and_reload() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = launch(&mut m).await;
    let id = &launched["launch_id"];
    let node = query(&mut m,id,json!({"by":"role","role":"status","name":"Status child"})).await;
    let target = json!({"by":"ref","id":node["ref"]});
    let bound = m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"same-node",
        "predicate":{"is":"attached","target":target}})).await;
    assert_eq!(bound["value"],"true");
    m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"saved-child",
        "predicate":{"is":"text_equals","target":child(test_id("status")),"text":"Saved child"}})).await;
    let action = m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child(test_id("save"))},"then":{"type":"none"}})).await;
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    let before = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    before.recorder.kill().unwrap();
    m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
    assert_eq!(frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().browser,before.browser);
    let waited = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","watch":"saved-child","max_ms":1000}})).await;
    assert_eq!(waited["t_us"].as_u64().unwrap(),action["t_after_us"].as_u64().unwrap()+123_000);
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":child(test_id("replace"))},"then":{"type":"none"}})).await;
    let removed = m.call_tool("watch",json!({"launch_id":id,"operation":"remove","name":"same-node"})).await;
    assert_eq!(removed["value"],"false");
    let replacement = query(&mut m,id,child(test_id("status"))).await;
    assert_ne!(replacement["ref"],node["ref"]);
    m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"before-reload",
        "predicate":{"is":"attached","target":{"by":"ref","id":replacement["ref"]}}})).await;
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"reload"}})).await;
    let removed = m.call_tool("watch",json!({"launch_id":id,"operation":"remove","name":"before-reload"})).await;
    assert_eq!(removed["value"],"unknown");
    let removed = m.call_tool("watch",json!({"launch_id":id,"operation":"remove","name":"saved-child"})).await;
    assert_eq!(removed["value"],"false");
    assert_eq!(query(&mut m,id,child(test_id("status"))).await["values"]["text"],"Idle child");
    m.call_tool("stop",json!({"launch_id":id})).await;
}

async fn launch_attachment(m: &mut support::Mcp, suffix: &str) -> Value {
    let addr = support::serve();
    m.call_tool("launch", json!({"url":format!("http://{addr}/shadow-attachment.html{}",suffix),
        "passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await
}

// A root created at +7 ms must be sampled before the separate +8 ms task changes its content.
// This also protects page-world MutationObserver semantics from the privileged engine notification.
#[tokio::test]
async fn first_shadow_attachment_stops_at_creation_and_replays() {
    for nested in [false,true] {
        let ws = support::workspace();
        let mut m = support::Mcp::start_in(&ws.root()).await;
        let launched = launch_attachment(&mut m,if nested {"?nested"} else {"?main"}).await;
        let id = &launched["launch_id"];
        m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"attached-ready",
            "predicate":{"is":"text_equals","target":css("#attached-status"),"text":"Ready"}})).await;
        let action = m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":css("#attach")},"then":{"type":"none"}})).await;
        let waited = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","watch":"attached-ready","max_ms":50}})).await;
        assert_eq!(waited["t_us"].as_u64().unwrap(),action["t_after_us"].as_u64().unwrap()+7_000,"nested={nested}: {waited}");
        assert_eq!(query(&mut m,id,css("#attached-status")).await["values"]["text"],"Ready");
        assert_eq!(query(&mut m,id,css("#page-observations")).await["values"]["text"],"0");
        let later = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{
            "is":"text_equals","target":css("#attached-status"),"text":"Later"},"max_ms":50}})).await;
        assert_eq!(later["t_us"].as_u64().unwrap(),action["t_after_us"].as_u64().unwrap()+8_000);
        assert_eq!(query(&mut m,id,css("#attached-status")).await["values"]["text"],"Later");
        let callback = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":css("#later-time"),"props":["value"]}})).await;
        assert_eq!(callback["values"]["value"],(action["t_after_us"].as_u64().unwrap()/1000+8).to_string());
        replay(&mut m,id).await;
    }
}

#[tokio::test]
async fn first_shadow_attachment_in_a_child_survives_recorder_replacement() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = launch_attachment(&mut m,"").await;
    let id = &launched["launch_id"];
    let scoped = |target| json!({"by":"frame","frame":css("#child"),"target":target});
    m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"child-ready",
        "predicate":{"is":"text_equals","target":scoped(css("#attached-status")),"text":"Ready"}})).await;
    let action = m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":scoped(css("#attach"))},"then":{"type":"none"}})).await;
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    let before = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    before.recorder.kill().unwrap();
    m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
    assert_eq!(frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().browser,before.browser);
    let waited = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","watch":"child-ready","max_ms":50}})).await;
    assert_eq!(waited["t_us"].as_u64().unwrap(),action["t_after_us"].as_u64().unwrap()+7_000,"{waited}");
    assert_eq!(query(&mut m,id,scoped(css("#attached-status"))).await["values"]["text"],"Ready");
    assert_eq!(query(&mut m,id,scoped(css("#page-observations"))).await["values"]["text"],"0");
    let recovered = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    recovered.recorder.kill().unwrap();
    m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
    let later = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{
        "is":"text_equals","target":scoped(css("#attached-status")),"text":"Later"},"max_ms":50}})).await;
    assert_eq!(later["t_us"].as_u64().unwrap(),action["t_after_us"].as_u64().unwrap()+8_000);
    m.call_tool("stop",json!({"launch_id":id})).await;
}
