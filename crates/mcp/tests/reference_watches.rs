mod support;
use serde_json::{Value, json};

async fn launch(m: &mut support::Mcp) -> Value {
    let addr = support::serve();
    m.call_tool("launch", json!({"url":format!("http://{addr}/shadow-watch.html"),"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await
}

async fn query(m: &mut support::Mcp, id: &Value, target: Value) -> Value {
    m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":target,"props":["text"]}})).await
}

#[tokio::test]
async fn role_references_inside_a_shadow_hosted_frame_replay_without_repeating_observation() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let addr = support::serve();
    let launched = m.call_tool("launch",json!({"url":format!("http://{addr}/shadow-watch.html?nested-frame"),"passthrough_origins":[format!("http://{addr}"),format!("http://localhost:{}",addr.port())]})).await;
    let id = &launched["launch_id"];
    let node = query(&mut m,id,json!({"by":"role","role":"button","name":"Save child"})).await;
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"ref","id":node["ref"]}}})).await;
    let status = query(&mut m,id,json!({"by":"role","role":"status","name":"Save status child"})).await;
    assert_eq!(status["values"]["text"],"Saved child");
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay = m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn shadow_frame_references_work_in_input_watches_predicate_waits_and_replay() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = launch(&mut m).await;
    let id = &launched["launch_id"];
    let state = query(&mut m,id,json!({"by":"role","role":"status","name":"Save status child"})).await;
    let target = json!({"by":"ref","id":state["ref"]});
    let button = query(&mut m,id,json!({"by":"role","role":"button","name":"Save child"})).await;
    let button = json!({"by":"ref","id":button["ref"]});
    let predicate = json!({"is":"text_equals","target":target,"text":"Saved child"});
    let added = m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"saved-child","predicate":predicate})).await;
    assert_eq!(added["value"],"false");
    let frame = query(&mut m,id,json!({"by":"css","selector":"#editor"})).await;
    let scoped = json!({"by":"frame","frame":{"by":"ref","id":frame["ref"]},"target":target});
    m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"scoped-child","predicate":{"is":"text_equals","target":scoped,"text":"Saved child"}})).await;
    let action = m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":button},"then":{"type":"none"}})).await;
    let waited = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","watch":"saved-child","max_ms":1000}})).await;
    assert_eq!(waited["t_us"].as_u64().unwrap(),action["t_after_us"].as_u64().unwrap()+123_000);
    assert_eq!(query(&mut m,id,target.clone()).await["values"]["text"],"Saved child");
    let removed = m.call_tool("watch",json!({"launch_id":id,"operation":"remove","name":"scoped-child"})).await;
    assert_eq!(removed["value"],"true");
    m.call_tool("watch",json!({"launch_id":id,"operation":"remove","name":"saved-child"})).await;
    let action = m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":button},"then":{"type":"none"}})).await;
    let waited = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":predicate,"max_ms":1000}})).await;
    assert_eq!(waited["t_us"].as_u64().unwrap(),action["t_after_us"].as_u64().unwrap()+123_000);
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay = m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn reference_watches_become_unknown_after_reload_while_selectors_follow_the_new_document() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = launch(&mut m).await;
    let id = &launched["launch_id"];
    let state = query(&mut m,id,json!({"by":"role","role":"status","name":"Save status child"})).await;
    let target = json!({"by":"ref","id":state["ref"]});
    m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"old-node","predicate":{"is":"attached","target":target}})).await;
    m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"new-node","predicate":{"is":"attached","target":{"by":"css","selector":"#editor"}}})).await;
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"reload"}})).await;
    let removed = m.call_tool("watch",json!({"launch_id":id,"operation":"remove","name":"old-node"})).await;
    assert_eq!(removed["value"],"unknown");
    let removed = m.call_tool("watch",json!({"launch_id":id,"operation":"remove","name":"new-node"})).await;
    assert_eq!(removed["value"],"true");
    let (failed,error) = m.try_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"attached","target":target},"max_ms":1000}})).await;
    assert!(failed && error["code"]=="stale_ref","{error}");
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay = m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

#[tokio::test]
async fn reference_watch_survives_recorder_loss_but_does_not_retarget_a_replacement_node() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = launch(&mut m).await;
    let id = &launched["launch_id"];
    let state = query(&mut m,id,json!({"by":"role","role":"status","name":"Save status child"})).await;
    let target = json!({"by":"ref","id":state["ref"]});
    m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"same-node","predicate":{"is":"attached","target":target}})).await;
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    let before = frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap();
    before.recorder.kill().unwrap();
    m.call_tool("session",json!({"launch_id":id,"operation":"reattach"})).await;
    assert_eq!(frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().browser,before.browser);
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":{"by":"role","role":"button","name":"Replace child"}},"then":{"type":"none"}})).await;
    let removed = m.call_tool("watch",json!({"launch_id":id,"operation":"remove","name":"same-node"})).await;
    assert_eq!(removed["value"],"false");
    let (failed,error) = m.try_tool("watch",json!({"launch_id":id,"operation":"add","name":"stale-node","predicate":{"is":"attached","target":target}})).await;
    assert!(failed && error["code"]=="stale_ref","{error}");
    assert_ne!(query(&mut m,id,json!({"by":"role","role":"status","name":"Save status child"})).await["ref"],state["ref"]);
    m.call_tool("stop",json!({"launch_id":id})).await;
    let (failed,error) = m.try_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert!(failed && error["code"]=="not_replayable","{error}");
}
