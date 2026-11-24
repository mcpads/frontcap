mod support;

use serde_json::{Value, json};

async fn launch(m: &mut support::Mcp, page: &str, addr: std::net::SocketAddr) -> (String, String) {
    let r = m.call_tool("launch", json!({"request_id": "l-1", "url": format!("http://{addr}/{page}"), "then": {"type": "none"}})).await;
    let id: String = r["launch_id"].as_str().unwrap().into();
    let lease: String = r["lease"].as_str().unwrap().into();
    m.call_tool("wait", json!({"launch_id": id, "lease": lease, "request_id": "c-0", "op": {"type": "advance", "by_ms": 10}})).await;
    (id, lease)
}

fn find_ref(snapshot: &Value, role: &str, name: &str) -> String {
    snapshot["nodes"].as_array().unwrap().iter().find(|n| n["role"] == role && n["name"] == name).unwrap_or_else(|| panic!("{role} {name} in {snapshot}"))["ref"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn a_scene_binds_structure_and_presentation_without_advancing() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (id, _) = launch(&mut m, "form.html", addr).await;
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 41}})).await;
    let response = m.request("tools/call", json!({"name": "observe", "arguments": {"launch_id": id, "op": {"type": "scene", "image": true}}})).await;
    assert!(response["result"]["content"].as_array().unwrap().iter().any(|part| part["type"] == "image" && part["mimeType"] == "image/png"));
    let before = response["result"]["structuredContent"].clone();
    assert_eq!(before["observation"], "scene");
    let cropped = m.request("tools/call", json!({"name": "observe", "arguments": {"launch_id": id, "op": {"type": "crop", "image_id": before["presentation"]["image"]["id"], "rect": [0, 0, 32, 32]}}})).await;
    assert!(cropped["result"]["content"].as_array().unwrap().iter().any(|part| part["type"] == "image"));
    assert_eq!(cropped["result"]["structuredContent"]["pixel_size"], json!([32, 32]));
    assert_eq!(cropped["result"]["structuredContent"]["boundary"], before["boundary"]);
    let save = find_ref(&before["snapshot"], "button", "Save");
    let inspected = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "inspect", "target": {"by": "ref", "id": save}, "props": ["interaction", "scroll"]}})).await;
    assert_eq!(inspected["boundary"], before["boundary"]);
    assert_eq!(inspected["click"]["ready"], true);
    assert_eq!(inspected["values"]["interaction"]["enabled"], true);
    let element = m.request("tools/call", json!({"name":"capture","arguments":{"launch_id":id,
        "op":{"type":"screenshot","target":save,"inline":true}}})).await;
    assert!(element["result"]["content"].as_array().unwrap().iter().any(|part| part["type"]=="image"), "{element}");
    let element = &element["result"]["structuredContent"];
    assert_eq!(element["boundary"], before["boundary"]);
    let geometry = m.call_tool("observe",json!({"launch_id":id,"op":{"type":"query","target":{"by":"ref","id":save},"props":["bbox"]}})).await;
    let bbox=&geometry["values"]["bbox"];
    let source=&before["presentation"]["image"];
    let sx=source["pixel_size"][0].as_f64().unwrap()/source["viewport_size"][0].as_f64().unwrap();
    let sy=source["pixel_size"][1].as_f64().unwrap()/source["viewport_size"][1].as_f64().unwrap();
    let x=(bbox["x"].as_f64().unwrap()*sx).floor() as u32;
    let y=(bbox["y"].as_f64().unwrap()*sy).floor() as u32;
    let right=((bbox["x"].as_f64().unwrap()+bbox["width"].as_f64().unwrap())*sx).ceil() as u32;
    let bottom=((bbox["y"].as_f64().unwrap()+bbox["height"].as_f64().unwrap())*sy).ceil() as u32;
    assert_eq!(element["clip"],json!([x,y,right-x,bottom-y]));
    let pixels=m.call_tool("observe",json!({"launch_id":id,"op":{"type":"crop","image_id":source["id"],"rect":element["clip"]}})).await;
    assert_eq!(element["sha256"],pixels["sha256"],"element capture uses the already presented pixels");

    let located = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "locate", "boundary": before["boundary"], "point": inspected["click"]["point"]}})).await;
    assert_eq!(located["target"]["ref"], save);
    assert_eq!(located["target"]["boundary"], before["boundary"]);
    let ancestry = located["hit_path"]["nodes"].as_array().unwrap();
    assert_eq!(ancestry.iter().map(|node| node["name"].as_str().unwrap()).collect::<Vec<_>>(), ["button", "body", "html", "#document"]);
    assert_eq!(ancestry[0]["ref"], save);
    assert_eq!(ancestry[0]["via"], "hit");
    assert_eq!(ancestry.last().unwrap()["kind"], "document");
    assert!(located["hit_path"]["omission"].is_null());
    let (wrong_target, error) = m.try_tool("act", json!({"launch_id": id, "action": {"type": "click_at", "target": {"by": "ref", "id": save}, "point": [0, 0], "boundary": before["boundary"]}, "then": {"type": "none"}})).await;
    assert!(wrong_target);
    assert_eq!(error["code"], "not_actionable");
    let repeated = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "scene"}})).await;
    assert_eq!(before["boundary"], repeated["boundary"]);
    assert_eq!(before["boundary"]["launch_id"], id);
    assert!(before["presentation"]["capture"]["path"].is_string(), "{before}");
    // The paint instant beside the instant the image stands for: never later.
    let capture = &before["presentation"]["capture"];
    assert!(capture["painted_at_us"].as_u64().zip(capture["t_us"].as_u64()).is_some_and(|(p, t)| p <= t), "{before}");
    assert!(before["coverage_gaps"].as_array().is_some_and(|g| !g.is_empty()));
    m.call_tool("act", json!({"launch_id": id, "action": {"type": "click_at", "target": {"by": "ref", "id": save}, "point": located["point"], "boundary": located["target"]["boundary"]}, "then": {"type": "none"}})).await;
    let after = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "scene"}})).await;
    assert_eq!(before["boundary"]["t_us"], after["boundary"]["t_us"]);
    assert!(after["boundary"]["state_revision"].as_u64().unwrap() > before["boundary"]["state_revision"].as_u64().unwrap());
    let (pending,error)=m.try_tool("capture",json!({"launch_id":id,"op":{"type":"screenshot","target":save}})).await;
    assert!(pending && error["code"]=="observation_changed","{error}");

    let (stale, error) = m.try_tool("observe", json!({"launch_id": id, "op": {"type": "locate", "boundary": before["boundary"], "point": inspected["click"]["point"]}})).await;
    assert!(stale);
    assert_eq!(error["code"], "observation_changed");
    // It names what changed (scenario 1 consumer run: the error did not say which field differed).
    let revision = format!("state_revision {} → {}", before["boundary"]["state_revision"], after["boundary"]["state_revision"]);
    assert!(error["message"].as_str().is_some_and(|m| m.contains(&revision) && !m.contains("t_us")), "{error}");
    let (stale_action, error) = m.try_tool("act", json!({"launch_id": id, "action": {"type": "click_at", "target": {"by": "ref", "id": save}, "point": located["point"], "boundary": before["boundary"]}, "then": {"type": "none"}})).await;
    assert!(stale_action);
    assert_eq!(error["code"], "observation_changed");

    m.call_tool("state",json!({"launch_id":id,"operation":"apply","boundary":after["boundary"],
        "changes":[{"target":{"by":"ref","id":save},"field":{"type":"style","name":"transform"},"value":"translateX(-5000px)"}]})).await;
    let offscreen=m.call_tool("observe",json!({"launch_id":id,"op":{"type":"scene"}})).await;
    let (failed,error)=m.try_tool("capture",json!({"launch_id":id,"op":{"type":"screenshot","target":save}})).await;
    assert!(failed && error["code"]=="invalid_arguments","{error}");
    let unchanged=m.call_tool("observe",json!({"launch_id":id,"op":{"type":"scene"}})).await;
    assert_eq!(unchanged["boundary"],offscreen["boundary"],"offscreen refusal never scrolls or advances");

    let (failed, error) = m.try_tool("observe", json!({"launch_id": id, "op": {"type": "scene", "max_nodes": 0}})).await;
    assert!(failed);
    assert_eq!(error["code"], "invalid_arguments");
    m.call_tool("stop", json!({"launch_id": id})).await;
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn snapshot_then_click_by_ref_through_mcp() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (id, lease) = launch(&mut m, "form.html", addr).await;
    let s = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "snapshot"}})).await;
    let save = find_ref(&s, "button", "Save");
    m.call_tool("act", json!({"launch_id": id, "lease": lease, "request_id": "a-1", "action": {"type": "click", "target": {"by": "ref", "id": save}}})).await;
    let q = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": {"by": "css", "selector": "#name"}, "props": ["value", "attr:aria-label"]}})).await;
    assert_eq!(q["values"]["value"], "x", "{q}");
    let d = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "diff", "since": s["observation_id"]}})).await;
    assert!(d["added"].is_array(), "{d}");
    m.call_tool("stop", json!({"launch_id": id, "lease": lease, "request_id": "s-1"})).await;
}

#[tokio::test]
async fn stale_ref_through_mcp() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (id, lease) = launch(&mut m, "form.html", addr).await;
    let s = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "snapshot"}})).await;
    let save = find_ref(&s, "button", "Save");
    m.call_tool("debug", json!({"operation": "evaluate", "launch_id": id, "lease": lease, "request_id": "e-1", "expression": "leave()"})).await;
    m.call_tool("wait", json!({"launch_id": id, "lease": lease, "request_id": "c-1", "op": {"type": "advance", "by_ms": 500}})).await;
    let (is_error, e) = m.try_tool("act", json!({"launch_id": id, "lease": lease, "request_id": "a-1", "action": {"type": "click", "target": {"by": "ref", "id": save}}})).await;
    assert!(is_error);
    assert_eq!(e["code"], "stale_ref", "{e}");
    assert!(e["recovery"].as_str().unwrap().contains("snapshot"));
    let (failed,error)=m.try_tool("capture",json!({"launch_id":id,"op":{"type":"screenshot","target":save}})).await;
    assert!(failed && error["code"]=="stale_ref","{error}");

    m.call_tool("stop", json!({"launch_id": id, "lease": lease, "request_id": "s-1"})).await;
}

#[tokio::test]
async fn a_click_that_opens_a_dialog_returns_and_reports_it() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (id, lease) = launch(&mut m, "dialogs.html", addr).await;
    let act = m.call_tool("act", json!({"launch_id": id, "lease": lease, "request_id": "a-1", "action": {"type": "click", "target": {"by": "css", "selector": "#del"}}})).await;
    assert_eq!(act["effects"]["dialogs"], json!(["confirm \"Delete?\" → false"]), "{act}");
    let q = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": {"by": "css", "selector": "#item"}, "props": ["text"]}})).await;
    assert_eq!(q["values"]["text"], "precious", "the guarded deletion did not happen");
    m.call_tool("stop", json!({"launch_id": id, "lease": lease, "request_id": "s-1"})).await;
}

#[tokio::test]
async fn evaluate_through_mcp_is_tainted_and_console_is_filtered() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (id, lease) = launch(&mut m, "form.html", addr).await;
    let r = m.call_tool("debug", json!({"operation": "evaluate", "launch_id": id, "lease": lease, "request_id": "e-1", "expression": "console.warn('careful'); 6 * 7"})).await;
    assert_eq!(r["value"], 42, "{r}");
    let c = m.call_tool("debug", json!({"launch_id": id, "operation": "console", "filter": {"level": "warning"}})).await;
    assert_eq!(c["entries"][0]["text"], "careful", "{c}");
    let s = m.call_tool("status", json!({"launch_id": id})).await;
    assert!(s["faults"].as_array().unwrap().is_empty());
    let stopped = m.call_tool("stop", json!({"launch_id": id, "lease": lease, "request_id": "s-1"})).await;
    let trace = std::fs::read_to_string(format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap())).unwrap();
    assert!(trace.contains("arbitrary_evaluate"));
}

#[tokio::test]
async fn document_state_changes_are_verified_deduplicated_and_replayed() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (id, _) = launch(&mut m, "form.html", addr).await;
    let read = m.call_tool("state", json!({"launch_id": id, "operation": "read", "target": {"by": "css", "selector": "#name"}, "field": {"type": "value"}})).await;
    let storage = m.call_tool("state", json!({"launch_id": id, "operation": "storage_scope", "target": {"by": "ref", "id": read["ref"]}})).await;
    let mut args = json!({"launch_id": id, "operation": "apply", "request_id": "set-name", "boundary": read["boundary"], "changes": [{"target": {"by": "ref", "id": read["ref"]}, "field": {"type": "value"}, "value": "Prepared", "expected": {"value": read["value"]}}]});
    let storage_field = json!({"type": "storage", "area": "local", "scope": storage["scope"], "key": "cart"});
    args["changes"].as_array_mut().unwrap().push(json!({"target": {"by": "ref", "id": read["ref"]}, "field": storage_field, "value": "large-cart", "expected": {"value": null}}));
    let applied = m.call_tool("state", args.clone()).await;
    assert_eq!(applied["completed"], true);
    assert_eq!(applied["receipts"][0]["after"], "Prepared");
    assert_eq!(m.call_tool("state", args).await, applied, "retry must return the original receipt despite the now-stale boundary");
    let now = m.call_tool("state", json!({"launch_id": id, "operation": "read", "target": {"by": "css", "selector": "#name"}, "field": {"type": "value"}})).await;
    assert_eq!(now["value"], "Prepared");
    let stored = m.call_tool("state", json!({"launch_id": id, "operation": "read", "target": {"by": "ref", "id": read["ref"]}, "field": storage_field})).await;
    assert_eq!(stored["value"], "large-cart");
    assert_eq!(now["boundary"]["t_us"], read["boundary"]["t_us"]);
    m.call_tool("stop", json!({"launch_id": id})).await;
    let relocated = support::serve();
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay", "url": format!("http://{relocated}/form.html")})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn launch_storage_initialization_precedes_app_startup_and_replays_at_another_origin() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (invalid, error) = m.try_tool("launch", json!({"url": format!("http://{addr}/initial-state.html"), "profile": {"initial_storage": {"origin": "not-an-origin", "local": {"cart": "invalid"}}}})).await;
    assert!(invalid);
    assert_eq!(error["code"], "invalid_arguments");
    let launched = m.call_tool("launch", json!({"url": format!("http://{addr}/initial-state.html"), "profile": {"initial_storage": {"origin": format!("http://{addr}"), "local": {"cart": "large-cart"}, "session": {"login": "signed-in"}}}})).await;
    let id = &launched["launch_id"];
    let boot = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": {"by": "css", "selector": "#boot"}, "props": ["text"]}})).await;
    assert_eq!(serde_json::from_str::<Value>(boot["values"]["text"].as_str().unwrap()).unwrap(), json!({"local":"large-cart", "session":"signed-in"}));
    m.call_tool("stop", json!({"launch_id": id})).await;
    let other = support::serve();
    let replay = m.call_tool("debug", json!({"launch_id": id, "operation": "replay", "url": format!("http://{other}/initial-state.html")})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

/// Scenario 1 consumer run: a snapshot's `text` reference (an error message under a field) is queried
/// like an element; reading its attributes failed as the page's script before.
#[tokio::test]
async fn a_text_reference_is_queried_for_its_own_box_and_its_elements_style() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let page = r#"<!doctype html><label for=email>Email</label> <input id=email><span id=error style="color: rgb(176, 0, 32)">Email is already in use</span><span id=gone hidden>Hidden note</span>
<p style="margin-top: 40px">Thanks for writing</p><div style="position: fixed; left: 0; top: 30px; width: 400px; height: 60px; background: white">Help panel</div>"#;
    let rules = json!([{"id":"app","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}}]);
    let launch = m.call_tool("launch", json!({"url":"https://app.example.test/","rules":rules})).await;
    let id = launch["launch_id"].as_str().unwrap();
    let snapshot = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"snapshot"}})).await;
    let error = find_ref(&snapshot, "text", "Email is already in use");
    let props = ["text", "visible", "bbox", "attr:id", "style:color", "interaction"];
    let read = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"ref","id":error},"props":props}})).await;
    let v = &read["values"];
    assert_eq!((&v["text"], &v["visible"], &v["attr:id"], &v["style:color"]), (&json!("Email is already in use"), &json!(true), &Value::Null, &json!("rgb(176, 0, 32)")), "{read}");
    assert!(v["bbox"]["width"].as_f64().unwrap() > 0.0, "{read}");
    assert_eq!(v["interaction"]["focused"], false, "{read}");
    // Inspect assesses a click at the text, which its element receives; a panel over it covers it.
    let inspected = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"inspect","target":{"by":"ref","id":error}}})).await;
    assert_eq!(inspected["click"]["ready"], true, "{inspected}");
    let thanks = find_ref(&snapshot, "text", "Thanks for writing");
    let covered = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"inspect","target":{"by":"ref","id":thanks}}})).await;
    assert_eq!(covered["click"]["ready"], false, "{covered}");
    assert!(covered["click"]["reason"].as_str().unwrap().starts_with("covered_by <div"), "{covered}");
    m.call_tool("stop", json!({"launch_id":id})).await;
}

/// Scenario 3 consumer run: a scene without an image reported its presentation `unavailable` whenever it
/// was asked without one. It is the presented surface's relation, read without copying it.
#[tokio::test]
async fn a_scene_without_an_image_reports_the_presented_relation() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let page = r#"<!doctype html><button id=b onclick="this.textContent = 'Pressed'">Press</button>"#;
    let rules = json!([{"id":"app","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}}]);
    let launch = m.call_tool("launch", json!({"url":"https://app.example.test/","rules":rules})).await;
    let id = launch["launch_id"].as_str().unwrap();
    let scene = json!({"launch_id":id,"op":{"type":"scene"}});
    assert_eq!(m.call_tool("observe", scene.clone()).await["presentation"]["relation"], "current");
    m.call_tool("act", json!({"launch_id":id,"action":{"type":"click","target":{"by":"css","selector":"#b"}},"then":{"type":"none"}})).await;
    let changed = m.call_tool("observe", scene.clone()).await;
    assert_eq!(changed["presentation"]["relation"], "pending", "{changed}");
    assert!(changed["presentation"]["capture"].is_null(), "no image was asked for: {changed}");
    m.call_tool("wait", json!({"launch_id":id,"op":{"type":"next","event":"presentation","max_ms":100}})).await;
    assert_eq!(m.call_tool("observe", scene).await["presentation"]["relation"], "current");
    m.call_tool("stop", json!({"launch_id":id})).await;
}

#[tokio::test]
async fn a_frame_the_browser_refuses_is_an_empty_frame_node_and_the_launch_reports_why() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": format!("http://{addr}/refused-frame.html"), "rules": [
        {"id": "partner", "match": {"url": "https://deny.example.test/*"}, "action": {"type": "mock", "status": 200,
         "headers": {"content-type": "text/html", "x-frame-options": "DENY"}, "body": "<button>Inside</button>"}}]})).await;
    let id = &launched["launch_id"];
    // The load's effects, as an act's: the frame's document was answered, and the browser said why it shows nothing.
    let effects = &launched["effects"];
    assert_eq!(effects["console"]["error"], 1, "{launched}");
    assert!(effects["requests"].as_array().unwrap().iter().any(|r| r.as_str().unwrap().starts_with("GET https://deny.example.test/ → mock(rule=partner) 200")), "{launched}");
    assert!(effects["navigations"].as_array().unwrap().iter().any(|n| n.as_str().unwrap().ends_with("/refused-frame.html")), "{launched}");
    let snapshot = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "snapshot"}})).await;
    let nodes: Vec<(String, String, u64)> = snapshot["nodes"].as_array().unwrap().iter()
        .map(|n| (n["role"].as_str().unwrap().to_string(), n["name"].as_str().unwrap().to_string(), n["depth"].as_u64().unwrap())).collect();
    let at = nodes.iter().position(|(role, name, _)| role == "iframe" && name == "Partner").unwrap_or_else(|| panic!("{snapshot}"));
    assert_eq!(nodes[at + 1], ("button".to_string(), "After".to_string(), 0), "the refused frame has nothing under it: {snapshot}");
    assert_eq!(snapshot["nodes"][at]["value"], "chrome-error://chromewebdata/", "{snapshot}");
    let console = m.call_tool("debug", json!({"launch_id": id, "operation": "console"})).await;
    let entry = &console["entries"][0];
    assert_eq!((entry["level"].as_str(), entry["source"].as_str()), (Some("error"), Some("other")), "{console}");
    assert!(entry["text"].as_str().unwrap().contains("X-Frame-Options"), "{console}");
    assert_eq!(effects["console_messages"], console["entries"], "launch explains the refusal without a separate debug read");
    // Without a settle the launch returns before scripts run, and reports no effects.
    let bare = m.call_tool("launch", json!({"url": format!("http://{addr}/refused-frame.html"), "then": {"type": "none"}})).await;
    assert!(bare.get("effects").is_none(), "{bare}");
    for launched in [&launched, &bare] { m.call_tool("stop", json!({"launch_id": launched["launch_id"]})).await; }
}

/// A snapshot or query the screen does not show yet says so (`presentation: pending`); once a frame
/// shows it, the mark is gone.
#[tokio::test]
async fn reads_ahead_of_the_screen_say_so() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (id, _) = launch(&mut m, "count.html", addr).await;
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 50}})).await;
    let query = json!({"launch_id": id, "op": {"type": "query", "target": {"by": "css", "selector": "#count"}, "props": ["text"]}});
    let painted = m.call_tool("observe", query.clone()).await;
    assert!(painted.get("presentation").is_none(), "{painted}");
    m.call_tool("act", json!({"launch_id": id, "action": {"type": "click", "target": {"by": "css", "selector": "#ok"}}, "then": {"type": "none"}})).await;
    let ahead = m.call_tool("observe", query.clone()).await;
    assert_eq!((ahead["values"]["text"].as_str(), ahead["presentation"].as_str()), (Some("1"), Some("pending")), "{ahead}");
    let snapshot = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "snapshot"}})).await;
    assert_eq!(snapshot["presentation"], "pending", "{snapshot}");
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "next", "event": "presentation", "max_ms": 100}})).await;
    let shown = m.call_tool("observe", query).await;
    assert!(shown.get("presentation").is_none(), "{shown}");
    m.call_tool("stop", json!({"launch_id": id})).await;
}
