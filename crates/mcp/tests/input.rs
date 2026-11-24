mod support;
use serde_json::json;

#[tokio::test]
async fn explicit_cancel_is_deduplicated_never_clicks_and_replays() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/cancel.html"),"then":{"type":"none"}})).await;
    let id = launched["launch_id"].as_str().unwrap();
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
    let state = m.call_tool("input", json!({"launch_id":id,"operation":"status"})).await;
    let begun = m.call_tool("input", json!({"launch_id":id,"operation":"begin","target":{"by":"css","selector":"#commit"},"point":[40,40],"boundary":state["boundary"]})).await;
    let args = json!({"launch_id":id,"request_id":"cancel","operation":"cancel","handle":begun["held"]["handle"]});
    let cancelled = m.call_tool("input", args.clone()).await;
    assert!(cancelled["held"].is_null());
    assert_eq!(cancelled["boundary"]["t_us"], 51_000);
    let input = &cancelled["effects"]["input"][0];
    assert_eq!(input["action"], "pointer_cancel", "{cancelled}");
    assert_eq!(input["request_id"], "cancel", "{cancelled}");
    let events = input["events"].as_array().unwrap();
    assert_eq!(events.len(), 1, "{cancelled}");
    assert_eq!((&events[0]["event"], &events[0]["t_us"], &events[0]["trusted"]),
        (&json!("pointercancel"), &json!(51_000), &json!(true)));
    assert_eq!(m.call_tool("input", args).await, cancelled);
    let observed = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({commits,timer,frame,captured:document.querySelector('#commit').hasPointerCapture(pointer),events:events.filter(e => ['pointercancel','pointerup','click'].includes(e[0])).map(e => [e[0],e[4],e[5]])})"})).await;
    assert_eq!(observed["value"], json!({"commits":0,"timer":false,"frame":false,"captured":false,"events":[["pointercancel",true,51]]}));
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// Held input reports what the page did, as an act does (scenario 3 consumer run: it reported nothing).
#[tokio::test]
async fn held_input_reports_its_effects() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/cancel.html#warn"),"then":{"type":"none"}})).await;
    let id = launched["launch_id"].as_str().unwrap();
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
    let state = m.call_tool("input", json!({"launch_id":id,"operation":"status"})).await;
    let begun = m.call_tool("input", json!({"launch_id":id,"operation":"begin","target":{"by":"css","selector":"#commit"},"point":[40,40],"boundary":state["boundary"]})).await;
    assert!(begun["effects"].is_object(), "{begun}");
    assert_eq!(begun["effects"]["console"]["warn"], 0, "{begun}");
    let args = json!({"launch_id":id,"request_id":"warn","operation":"cancel","handle":begun["held"]["handle"]});
    let cancelled = m.call_tool("input", args.clone()).await;
    assert_eq!(cancelled["effects"]["console"]["warn"], 1, "the page's warning on cancel is the cancel's effect: {cancelled}");
    assert_eq!(cancelled["effects"]["console_messages"], json!([{
        "t_us": 51_000, "level": "warning", "text": "pointer cancelled"
    }]), "{cancelled}");
    assert_eq!(m.call_tool("input", args).await, cancelled, "a repeat reports the same effects");
    m.call_tool("stop", json!({"launch_id":id})).await;
}

#[tokio::test]
async fn right_button_lease_cleanup_cancels_the_owned_button_and_replays() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url":format!("http://{addr}/pointer.html"),"then":{"type":"none"}})).await;
    let id = launched["launch_id"].as_str().unwrap();
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
    let state = m.call_tool("input", json!({"launch_id":id,"operation":"status"})).await;
    let begun = m.call_tool("input", json!({"launch_id":id,"operation":"begin","target":{"by":"css","selector":"#pad"},"point":[40,40],"button":"right","boundary":state["boundary"]})).await;
    assert_eq!(begun["held"]["button"], "right");
    m.call_tool("session", json!({"launch_id":id,"operation":"release"})).await;
    m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    let status = m.call_tool("input", json!({"launch_id":id,"operation":"status"})).await;
    assert!(status["held"].is_null());
    assert_eq!(status["boundary"]["t_us"], 51_000);
    let events = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"buttons.filter(e => ['pointerdown', 'pointerup', 'pointercancel', 'click', 'auxclick'].includes(e[0])).map(e => [e[0],e[2],e[3],e[4]])"})).await;
    assert_eq!(events["value"], json!([["pointerdown",2,true,51],["pointercancel",0,true,51]]));
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn native_wheel_is_boundary_checked_deduplicated_and_replayable() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": format!("http://{addr}/wheel.html"), "then":{"type":"none"}})).await;
    let id = launched["launch_id"].as_str().unwrap();
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":51}})).await;
    for (request, selector, point) in [("blocked-wheel", "#blocked-content", [340, 40]), ("nested-wheel", "#content", [40, 40])] {
        let state = m.call_tool("input", json!({"launch_id":id,"operation":"status"})).await;
        let args = json!({"launch_id":id,"request_id":request,"operation":"wheel","target":{"by":"css","selector":selector},"point":point,"delta":[0,80],"boundary":state["boundary"]});
        let answer = m.call_tool("input", args.clone()).await;
        assert_eq!(answer["boundary"]["t_us"], state["boundary"]["t_us"]);
        assert!(answer["held"].is_null());
        assert_eq!(m.call_tool("input", args).await, answer);
    }
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"advance","by_ms":34}})).await;
    let result = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"({events: wheels, scroll: [document.querySelector('#inner').scrollTop, document.querySelector('#outer').scrollTop, document.querySelector('#blocked').scrollTop]})"})).await;
    assert_eq!(result["value"]["scroll"], json!([80, 0, 0]));
    assert_eq!(result["value"]["events"], json!([["blocked-content",0,80,true,true,51],["content",0,80,true,false,51]]));
    m.call_tool("stop", json!({"launch_id":id})).await;
    let replay = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn pointer_calls_are_frozen_and_lease_release_cleans_up_before_reattach() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": format!("http://{addr}/drag.html")})).await;
    let id = launched["launch_id"].as_str().unwrap();
    let target = json!({"by":"css","selector":"#handle"});
    let initial = m.call_tool("input", json!({"launch_id":id,"operation":"status"})).await;
    let at = initial["boundary"]["t_us"].clone();
    let args = json!({"launch_id":id,"request_id":"pointer-begin","operation":"begin","target":target,"point":[40,40],"boundary":initial["boundary"]});
    let begun = m.call_tool("input", args.clone()).await;
    assert_eq!(m.call_tool("input", args).await, begun, "retry does not press twice");
    let handle = begun["held"]["handle"].clone();
    assert!(!handle.is_null(), "{begun}");
    let moved = m.call_tool("input", json!({"launch_id":id,"operation":"move","handle":handle,"point":[160,40],"boundary":begun["boundary"]})).await;
    assert_eq!(moved["boundary"]["t_us"], at);
    let inspected = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":target,"props":["style:left"]}})).await;
    assert_eq!(inspected["values"]["style:left"], "140px");
    let ended = m.call_tool("input", json!({"launch_id":id,"operation":"end","handle":handle})).await;
    assert!(ended["held"].is_null());
    assert_eq!(ended["boundary"]["t_us"], at);
    let again = m.call_tool("input", json!({"launch_id":id,"operation":"begin","target":target,"point":[160,40],"boundary":ended["boundary"]})).await;
    assert!(!again["held"].is_null());
    m.call_tool("session", json!({"launch_id":id,"operation":"release"})).await;
    let cleaned = m.call_tool("input", json!({"launch_id":id,"operation":"status"})).await;
    assert!(cleaned["held"].is_null(), "release must finish cleanup before answering: {cleaned}");
    assert_eq!(cleaned["boundary"]["t_us"], at);
    m.call_tool("session", json!({"launch_id":id,"operation":"reattach"})).await;
    let (error, rejected) = m.try_tool("input", json!({"launch_id":id,"operation":"end","handle":again["held"]["handle"]})).await;
    assert!(error && rejected["code"] == "invalid_arguments", "{rejected}");
    let log = m.call_tool("debug", json!({"launch_id":id,"operation":"evaluate","expression":"JSON.stringify(log.filter(e => e[0] === 'pointerdown' || e[0] === 'pointerup' || e[0] === 'pointercancel').map(e => e[0]))"})).await;
    assert_eq!(log["value"], "[\"pointerdown\",\"pointerup\",\"pointerdown\",\"pointercancel\"]");
    m.call_tool("stop", json!({"launch_id":id})).await;
    let report = m.call_tool("debug", json!({"launch_id":id,"operation":"replay"})).await;
    assert_eq!(report["equal"], true, "{report}");
    assert!(report["diverged"].is_null(), "{report}");
}

/// `hover_at` moves the unpressed mouse to a checked point; a point outside its target is refused.
#[tokio::test]
async fn hover_at_moves_the_unpressed_mouse_to_a_checked_point() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let id = m.call_tool("launch", json!({"url": format!("http://{addr}/pointer.html"), "then": {"type": "none"}})).await["launch_id"].clone();
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 51}})).await;
    let state = m.call_tool("input", json!({"launch_id": id, "operation": "status"})).await;
    let pad = json!({"by": "css", "selector": "#pad"});
    let (refused, error) = m.try_tool("act", json!({"launch_id": id, "action": {"type": "hover_at", "target": pad, "point": [5, 5], "boundary": state["boundary"]}, "then": {"type": "none"}})).await;
    assert!(refused);
    assert_eq!(error["code"], "not_actionable", "{error}");
    for x in [100, 180] {
        let state = m.call_tool("input", json!({"launch_id": id, "operation": "status"})).await;
        m.call_tool("act", json!({"launch_id": id, "action": {"type": "hover_at", "target": pad, "point": [x, 60], "boundary": state["boundary"]}, "then": {"type": "none"}})).await;
    }
    let events = m.call_tool("debug", json!({"launch_id": id, "operation": "evaluate", "expression": "buttons.map(e => [e[0], e[2]])"})).await;
    assert_eq!(events["value"], json!([["pointermove", 0], ["pointermove", 0]]), "{events}");
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}
