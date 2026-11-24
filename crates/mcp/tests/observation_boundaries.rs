//! Parser attachment and read-only style preparation at stopped boundaries.
mod support;
use serde_json::{Value, json};
fn css(selector: &str) -> Value { json!({"by":"css","selector":selector}) }

#[tokio::test]
async fn declarative_roots_are_discovered_and_fragment_attachment_stops_before_the_next_timer() {
    let ws = support::workspace();
    let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch",json!({"url":format!("http://{addr}/declarative-shadow.html")})).await;
    let id = &launched["launch_id"];
    for (selector, text) in [("#initial-status","Parsed"),("#nested-status","Nested")] {
        let read = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":css(selector),"props":["text"]}})).await;
        assert_eq!(read["values"]["text"],text);
    }
    m.call_tool("watch",json!({"launch_id":id,"operation":"add","name":"parsed","predicate":{"is":"text_equals","target":css("#parsed-status"),"text":"Ready"}})).await;
    let action = m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":css("#parse")},"then":{"type":"none"}})).await;
    let start = action["t_after_us"].as_u64().unwrap();
    let first = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","watch":"parsed","max_ms":50}})).await;
    assert_eq!(first["t_us"],start+7000,"{first}");
    let ready = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":css("#parsed-status"),"props":["text"]}})).await;
    assert_eq!(ready["values"]["text"],"Ready");
    let later = m.call_tool("wait",json!({"launch_id":id,"op":{"type":"until","predicate":{"is":"text_equals","target":css("#parsed-status"),"text":"Later"},"max_ms":50}})).await;
    assert_eq!(later["t_us"],start+8000,"{later}");
    let callback = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":css("#later-time"),"props":["value"]}})).await;
    assert_eq!(callback["values"]["value"],(start/1000+8).to_string());
    m.call_tool("stop",json!({"launch_id":id})).await;
    let replay = m.call_tool("debug",json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"],true,"{replay}");
}

async fn transition(reads: usize, pause_ms: u64) -> Value {
    let ws = support::workspace();
    let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch",json!({"url":format!("http://{addr}/read-transition.html")})).await;
    let id = &launched["launch_id"];
    m.call_tool("act",json!({"launch_id":id,"action":{"type":"click","target":css("#change")},"then":{"type":"none"}})).await;
    let before = m.call_tool("status",json!({"launch_id":id})).await;
    for _ in 0..reads {
        m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":css("#box"),"props":["style:opacity","bbox"]}})).await;
        m.call_tool("observe",json!({"launch_id":id,"op":{"type":"inspect","target":css("#box"),"props":["style:width"]}})).await;
        let snapshot = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"snapshot","geometry":true}})).await;
        m.call_tool("observe",json!({"launch_id":id,"op":{"type":"diff","since":snapshot["observation_id"]}})).await;
        m.call_tool("observe",json!({"launch_id":id,"op":{"type":"scene","image":true}})).await;
        tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
    }
    let after = m.call_tool("status",json!({"launch_id":id})).await;
    assert_eq!(before["t_us"],after["t_us"]);
    let pending = m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate","expression":"events.slice()"})).await;
    assert_eq!(pending["value"],json!([]),"observation must not dispatch transition callbacks");
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":50}})).await;
    let mid = m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate","expression":"({events:events.slice(),width:box.getBoundingClientRect().width,opacity:getComputedStyle(box).opacity})"})).await;
    let width = mid["value"]["width"].as_f64().unwrap();
    assert!(width > 30.0 && width < 130.0, "the midpoint must observe an active transition: {mid}");
    m.call_tool("wait",json!({"launch_id":id,"op":{"type":"advance","by_ms":150}})).await;
    let end = m.call_tool("debug",json!({"launch_id":id,"operation":"evaluate","expression":"({events:events.slice(),width:box.getBoundingClientRect().width,opacity:getComputedStyle(box).opacity})"})).await;
    assert_eq!(end["value"]["width"],130);
    assert_eq!(end["value"]["opacity"],"1");
    assert_eq!(end["value"]["events"].as_array().unwrap().iter().filter(|e|e[0]=="transitionend").count(),2);
    m.call_tool("stop",json!({"launch_id":id})).await;
    json!({"mid":mid["value"],"end":end["value"]})
}

#[tokio::test]
async fn stopped_observation_does_not_shift_transition_values_or_event_deadlines() {
    let baseline = transition(0,0).await;
    assert_eq!(transition(1,0).await,baseline,"one style/layout read must preserve progression");
    assert_eq!(transition(8,40).await,baseline,"repeated reads and host delay must preserve progression");
}
