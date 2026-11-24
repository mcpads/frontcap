//! Replay bases through the public MCP surface: a generation's recorded commands (live or stopped), repeated in a
//! new child that stays live at the end of the recorded prefix.
mod support;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A page whose version can change between recording and replay; it logs its version and fetches a
/// value on each click.
fn serve_versioned(changed: Arc<AtomicBool>) -> std::net::SocketAddr {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let version = if changed.load(Ordering::SeqCst) { "two" } else { "one" };
            let body = format!("<!doctype html><button id=go>Go</button><output id=out>[]</output><script>\
                console.log('version {version}');let seen=[];\
                go.onclick=()=>fetch('/api/value').then(r=>r.text()).then(t=>{{seen.push(t);out.textContent=JSON.stringify(seen)}});</script>");
            let header = tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap();
            request.respond(tiny_http::Response::from_string(body).with_header(header)).unwrap();
        }
    });
    addr
}

fn click(id: &Value) -> Value {
    json!({"launch_id": id, "action": {"type": "click", "target": {"by": "css", "selector": "#go"}}, "then": {"type": "settle", "idle_window_ms": 50, "max_ms": 1000}})
}

fn out(id: &Value) -> Value {
    json!({"launch_id": id, "op": {"type": "query", "target": {"by": "css", "selector": "#out"}, "props": ["text"]}})
}

/// The button's reference as `id` names it.
async fn go_ref(m: &mut support::Mcp, id: &Value) -> Value {
    m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": {"by": "css", "selector": "#go"}, "props": ["text"]}})).await["ref"].clone()
}

fn click_ref(id: &Value, reference: &Value) -> Value {
    json!({"launch_id": id, "action": {"type": "click", "target": {"by": "ref", "id": reference}}, "then": {"type": "settle", "idle_window_ms": 50, "max_ms": 1000}})
}

async fn record(m: &mut support::Mcp, origin: &str) -> (Value, u64, Value) {
    let steps = json!([{"id": "values", "match": {"url": format!("{origin}/api/value")}, "action": {"type": "sequence", "after_last": "repeat_last",
        "steps": [{"status": 200, "body": "a"}, {"status": 200, "body": "b"}, {"status": 200, "body": "c"}]}}]);
    let launched = m.call_tool("launch", json!({"url": format!("{origin}/"), "rules": steps})).await;
    let id = launched["launch_id"].clone();
    m.call_tool("act", click(&id)).await;
    // The second click names the button by reference, which a replay binds under its recorded name.
    let go = go_ref(m, &id).await;
    m.call_tool("act", click_ref(&id, &go)).await;
    assert_eq!(m.call_tool("observe", out(&id)).await["values"]["text"], "[\"a\",\"b\"]");
    let t = m.call_tool("status", json!({"launch_id": id})).await["t_us"].as_u64().unwrap();
    (id, t, go)
}

#[tokio::test]
async fn a_replay_child_continues_live_from_the_end_of_the_recording() {
    let ws = support::workspace();
    let changed = Arc::new(AtomicBool::new(false));
    let origin = format!("http://{}", serve_versioned(changed.clone()));
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (source, end, go) = record(&mut m, &origin).await;

    // Saved while live: the prefix so far is copied into the basis, and the generation goes on unchanged.
    // Named predicates are read in the source now and checked in each ready child.
    let verify = json!([{"name": "two-values", "predicate": {"is": "text_equals", "target": {"by": "css", "selector": "#out"}, "text": "[\"a\",\"b\"]"}}]);
    let live = m.call_tool("state", json!({"operation": "save", "kind": "replay", "from": source, "verify": verify})).await;
    assert_eq!(live["verify_at_source"], json!([{"name": "two-values", "value": "true"}]), "{live}");
    let described = m.call_tool("state", json!({"operation": "describe_saved", "saved_id": live["saved_id"]})).await;
    assert_eq!(described["verify"], verify, "{described}");
    assert_eq!(described["origin"]["kept"], true, "{described}");
    assert_eq!(described["commands"], 3);
    assert_eq!(described["ends_at_us"], end, "a branch stops where the prefix ends: {described}");
    assert_eq!(m.call_tool("status", json!({"launch_id": source})).await["t_us"], end, "saving moved no time");
    let from_live = m.call_tool("state", json!({"operation": "branch", "saved_id": live["saved_id"], "kind": "replay"})).await;
    assert_eq!(from_live["verification"], json!([{"name": "two-values", "value": "true"}]), "{from_live}");
    assert_eq!(from_live["clock"]["t_us"], end, "{from_live}");
    let (refused, error) = m.try_tool("state", json!({"operation": "save", "kind": "recipe", "verify": verify,
        "recipe": {"url": format!("{origin}/")}})).await;
    assert!(refused && error["recovery"].as_str().unwrap().contains("recipe's verify"), "{error}");
    assert_eq!(m.call_tool("observe", out(&from_live["launch_id"])).await["values"]["text"], "[\"a\",\"b\"]");
    m.call_tool("act", click(&from_live["launch_id"])).await;
    assert_eq!(m.call_tool("observe", out(&from_live["launch_id"])).await["values"]["text"], "[\"a\",\"b\",\"c\"]");
    assert_eq!(m.call_tool("observe", out(&source)).await["values"]["text"], "[\"a\",\"b\"]", "the source is not the child");
    m.call_tool("stop", json!({"launch_id": from_live["launch_id"]})).await;
    m.call_tool("stop", json!({"launch_id": source})).await;
    // A stopped generation still answers evidence queries, from its finalized bundle.
    let stopped = m.call_tool("status", json!({"launch_id": source})).await;
    assert_eq!(stopped["stopped"], true, "{stopped}");
    assert_eq!(stopped["clock"]["state"], "stopped");
    assert!(stopped["t_us"].as_u64().unwrap() >= end);
    assert_eq!(stopped["commands"], 3);
    let console = m.call_tool("debug", json!({"launch_id": source, "operation": "console"})).await;
    assert_eq!(console["entries"][0]["text"], "version one", "{console}");
    let requests = m.call_tool("debug", json!({"launch_id": source, "operation": "requests", "filter": {"url_contains": "/api/value"}})).await;
    assert_eq!(requests["entries"].as_array().unwrap().len(), 2, "{requests}");
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "replay", "from": source})).await;
    assert_eq!(saved["kind"], "replay");
    let described = m.call_tool("state", json!({"operation": "describe_saved", "saved_id": saved["saved_id"]})).await;
    assert_eq!(described["commands"], 3, "the launch settle and two clicks: {described}");

    let branch = |request: &str| json!({"operation": "branch", "saved_id": saved["saved_id"], "kind": "replay", "request_id": request});
    let first = m.call_tool("state", branch("first")).await;
    assert_eq!(first["phase"], "ready", "{first}");
    assert_eq!(first["replayed"], json!({"commands": 3, "of": 3}));
    assert_eq!(first["origin"]["kind"], "replay");
    assert_eq!(first["verified"].as_array().unwrap().len(), 2, "the result says what was checked: {first}");
    assert_eq!(first["clock"]["t_us"], end, "the child stops where the recording's commands ended");
    assert_eq!(described["ends_at_us"], end, "{described}");
    let child = first["launch_id"].clone();
    assert_eq!(m.call_tool("observe", out(&child)).await["values"]["text"], "[\"a\",\"b\"]");
    // The reference the recording used was the source's: the ready child refuses it and names the
    // button itself.
    let (refused, error) = m.try_tool("act", click_ref(&child, &go)).await;
    assert!(refused);
    assert_eq!(error["code"], "invalid_arguments", "{error}");
    let own = go_ref(&mut m, &child).await;
    let tag = |r: &Value| r.as_str().unwrap().split_once('-').unwrap().1.to_string();
    assert_ne!(tag(&own), tag(&go), "{own} {go}");
    // Live from there: the next click takes the sequence's next step.
    m.call_tool("act", click(&child)).await;
    assert_eq!(m.call_tool("observe", out(&child)).await["values"]["text"], "[\"a\",\"b\",\"c\"]");

    // A retry finds the same child; a second branch is independent of the first child's later click.
    let count = std::fs::read_dir(ws.root().join("runtimes")).unwrap().count();
    assert_eq!(m.call_tool("state", branch("first")).await["launch_id"], child);
    assert_eq!(std::fs::read_dir(ws.root().join("runtimes")).unwrap().count(), count);
    let second = m.call_tool("state", branch("second")).await;
    assert_eq!(m.call_tool("observe", out(&second["launch_id"])).await["values"]["text"], "[\"a\",\"b\"]");

    // Methods are never substituted, and a replay branch takes no changes.
    for kind in ["recipe", "checkpoint"] {
        let (refused, error) = m.try_tool("state", json!({"operation": "branch", "saved_id": saved["saved_id"], "kind": kind})).await;
        assert!(refused);
        assert_eq!(error["code"], "method_mismatch", "{error}");
    }
    let mut with_changes = branch("changes");
    with_changes["changes"] = json!([{"target": {"by": "css", "selector": "#out"}, "field": {"type": "text"}, "value": "x"}]);
    let (refused, error) = m.try_tool("state", with_changes).await;
    assert!(refused);
    assert_eq!(error["code"], "invalid_arguments", "{error}");

    // The app changed since the recording: the replay diverges, and no ready child is published.
    changed.store(true, Ordering::SeqCst);
    let (refused, error) = m.try_tool("state", branch("changed-app")).await;
    assert!(refused);
    assert_eq!(error["code"], "construction_failed", "{error}");
    assert!(error["message"].as_str().unwrap().contains("failed at replay"), "{error}");

    // A changed source run is never used.
    let manifest = ws.root().join("runs").join(source.as_str().unwrap().replacen("g-", "r-", 1)).join("manifest.json");
    let text = std::fs::read_to_string(&manifest).unwrap();
    std::fs::write(&manifest, text.replacen('{', "{ ", 1)).unwrap();
    let (refused, error) = m.try_tool("state", branch("tampered")).await;
    assert!(refused);
    assert_eq!(error["code"], "basis_corrupt", "{error}");

    for id in [&child, &second["launch_id"]] {
        m.call_tool("stop", json!({"launch_id": id})).await;
    }
}
