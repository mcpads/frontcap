//! Closed shadow roots: the patched engine hands each to the platform's observer world as it is
//! attached, so locators, input checks, watches and replay reach inside while page script cannot.
mod support;
use serde_json::{Value, json};

fn css(selector: &str) -> Value { json!({"by":"css","selector":selector}) }

async fn text(m: &mut support::Mcp, id: &Value, target: Value) -> Value {
    m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":target,"props":["text"]}})).await["values"]["text"].clone()
}

#[tokio::test]
async fn closed_roots_are_reachable_for_the_platform_only() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/closed-roots.html"),"then":{"type":"settle","idle_window_ms":50,"max_ms":300}})).await;
    let id = &launched["launch_id"];
    assert_eq!(text(&mut m, id, css("#buy")).await, "Buy now");
    assert_eq!(text(&mut m, id, json!({"by":"text","text":"Buy now"})).await, "Buy now");
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"fill","target":{"by":"test_id","id":"qty"},"value":"3"},"then":{"type":"none"}})).await;
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":{"by":"role","role":"button","name":"Buy now"}}})).await;
    assert_eq!(text(&mut m, id, css("#out")).await, "bought 3", "input inside the closed root is delivered and checked");
    let evaluated = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"pageSees() === null"})).await;
    assert_eq!(evaluated["value"], true, "page script still cannot reach the closed root");

    // A closed root attached later joins a predicate wait at the task that attached it.
    let added = m.call_tool("watch", json!({"launch_id":id,"operation":"add","name":"toast","predicate":{"is":"attached","target":css("#toast")}})).await;
    assert_eq!(added["value"], "false");
    let waited = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"until","watch":"toast","max_ms":2000}})).await;
    assert_eq!(waited["outcome"]["type"], "condition_met", "{waited}");
    assert_eq!(waited["t_us"], 700_000, "the attachment's own instant: {waited}");
    assert_eq!(text(&mut m, id, css("#toast")).await, "Added to cart");
    assert_eq!(text(&mut m, id, css("#promo")).await, "Free shipping", "a declarative closed root is found too");

    // The observer world outlives a recorder: a replacement still reaches the closed roots.
    let dir = ws.root().join("runtimes").join(id.as_str().unwrap());
    frontcap_recorder::runtime::load_runtime(&dir).unwrap().unwrap().recorder.kill().unwrap();
    m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    assert_eq!(text(&mut m, id, css("#buy")).await, "Buy now", "after recorder replacement");

    m.call_tool("stop", json!({"launch_id":id})).await;
}

#[tokio::test]
async fn input_and_watches_inside_closed_roots_replay() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/closed-roots.html"),"then":{"type":"settle","idle_window_ms":50,"max_ms":300}})).await;
    let id = &launched["launch_id"];
    let reference = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":css("#qty"),"props":["text"]}})).await["ref"].clone();
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"fill","target":{"by":"ref","id":reference},"value":"5"},"then":{"type":"none"}})).await;
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":css("#buy")}})).await;
    m.call_tool("watch", json!({"launch_id":id,"operation":"add","name":"toast","predicate":{"is":"attached","target":css("#toast")}})).await;
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"until","watch":"toast","max_ms":2000}})).await;
    assert_eq!(text(&mut m, id, css("#out")).await, "bought 5");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "a reference inside a closed root binds again in replay: {replay}");
}

#[tokio::test]
async fn an_acts_effects_name_each_field_focus_moves_through_in_a_closed_component() {
    let ws = support::workspace(); let addr = support::serve();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/code-input.html")})).await;
    let id = &launched["launch_id"];
    let digit = |n: u8| format!(r#"<input aria-label="Digit {n}" maxlength="1">"#);
    let clicked = m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":{"by":"role","role":"textbox","name":"Digit 1"}}})).await;
    assert_eq!(clicked["effects"]["focus"], json!([format!("focus {}", digit(1))]), "{clicked}");
    let pressed = m.call_tool("act", json!({"launch_id":id,"action":{"type":"press","key":"4"}})).await;
    assert_eq!(pressed["effects"]["focus"], json!([format!("blur {}", digit(1)), format!("focus {}", digit(2))]), "{pressed}");
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"press","key":"2"}})).await;
    let last = m.call_tool("act", json!({"launch_id":id,"action":{"type":"press","key":"7"}})).await;
    assert_eq!(last["effects"]["focus"], json!([format!("blur {}", digit(3)), r#"focus <button id="go">Go</button>"#]), "{last}");
    // An act that moves no focus lists none.
    let hovered = m.call_tool("act", json!({"launch_id":id,"action":{"type":"hover","target":css("#go")}})).await;
    assert!(hovered["effects"].get("focus").is_none(), "{hovered}");
    m.call_tool("stop", json!({"launch_id":id})).await;
}
