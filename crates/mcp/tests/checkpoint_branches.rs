//! Checkpoint bases and branches through the public MCP surface: a live generation's process image saved
//! as a basis and branched into children that continue its execution (page closure, pending timer, worker
//! heap and held request) independently, while the source goes on.
mod support;
use serde_json::{Value, json};

fn rules() -> Value {
    json!([
        {"id": "document", "match": {"url": "*checkpoint.html"}, "action": {"type": "mock", "status": 200,
            "headers": {"content-type": "text/html"}, "body": include_str!("../../adapter-chromium/tests/fixtures/checkpoint.html")}},
        {"id": "worker", "match": {"url": "*checkpoint-worker.js"}, "action": {"type": "mock", "status": 200,
            "headers": {"content-type": "text/javascript"}, "body": include_str!("../../adapter-chromium/tests/fixtures/checkpoint-worker.js")}},
        {"id": "held", "match": {"url": "*checkpoint-held"}, "action": {"type": "hold"}}
    ])
}

fn css(selector: &str) -> Value {
    json!({"by": "css", "selector": selector})
}

async fn text(m: &mut support::Mcp, id: &Value, selector: &str) -> Value {
    m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": css(selector), "props": ["text"]}})).await["values"]["text"].clone()
}

async fn click(m: &mut support::Mcp, id: &Value, selector: &str) {
    m.call_tool("act", json!({"launch_id": id, "action": {"type": "click", "target": css(selector)}, "then": {"type": "none"}})).await;
}

async fn advance(m: &mut support::Mcp, id: &Value, by_ms: u64) {
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": by_ms}})).await;
}

/// `extra` more hidden steps, the held request answered, 400 ms: what the timer, the worker and the
/// response then show.
async fn continuation(m: &mut support::Mcp, id: &Value, extra: usize) -> Vec<Value> {
    for _ in 0..extra {
        click(m, id, "#step").await;
    }
    let held = m.call_tool("network", json!({"launch_id": id, "operation": "list_held"})).await;
    assert_eq!(held["held"].as_array().map(Vec::len), Some(1), "the held request crossed the image: {held}");
    m.call_tool("network", json!({"launch_id": id, "operation": "respond", "ordinal": held["held"][0]["ordinal"], "response": {"status": 200, "body": "answer"}})).await;
    advance(m, id, 400).await;
    let mut shown = Vec::new();
    for selector in ["#timer", "#worker", "#response"] {
        shown.push(text(m, id, selector).await);
    }
    shown
}

#[tokio::test]
#[ignore = "creates cells and runs CRIU with the patched engine: run as root on Linux with FRONTCAP_CELLS and --ignored"]
async fn a_checkpoint_basis_branches_into_children_that_continue_its_execution() {
    assert!(std::env::var_os("FRONTCAP_CELLS").is_some(), "the MCP server's recorders need FRONTCAP_CELLS");
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/checkpoint.html", "rules": rules(), "passthrough_origins": [], "then": {"type": "none"}})).await;
    let source = launched["launch_id"].clone();
    advance(&mut m, &source, 51).await;
    assert_eq!(text(&mut m, &source, "#ready").await, "ready");
    click(&mut m, &source, "#step").await;
    click(&mut m, &source, "#step").await;
    click(&mut m, &source, "#arm").await;
    advance(&mut m, &source, 50).await;
    // Held input crosses into each child.
    m.call_tool("input", json!({"launch_id": source, "operation": "key_down", "key": "Shift"})).await;

    // Named start conditions are read in the source and checked in every ready child.
    let verify = json!([{"name": "ready-shown", "predicate": {"is": "text_equals", "target": css("#ready"), "text": "ready"}}]);
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "checkpoint", "from": source, "verify": verify})).await;
    assert_eq!(saved["kind"], "checkpoint", "{saved}");
    assert_eq!(saved["verify_at_source"], json!([{"name": "ready-shown", "value": "true"}]), "{saved}");
    assert_eq!(saved["origin"]["launch_id"], source);
    let saved_id = saved["saved_id"].clone();
    let described = m.call_tool("state", json!({"operation": "describe_saved", "saved_id": saved_id})).await;
    assert!(described["bytes"].as_u64().unwrap() > 0, "{described}");
    let boundary = saved["origin"]["boundary"]["t_us"].clone();

    let branch = |request: &str| json!({"operation": "branch", "saved_id": saved_id, "kind": "checkpoint", "request_id": request});
    let first = m.call_tool("state", branch("first")).await;
    let second = m.call_tool("state", branch("second")).await;
    for child in [&first, &second] {
        assert_eq!(child["phase"], "ready", "{child}");
        assert_eq!(child["boundary"]["t_us"], boundary, "a child starts at the image's boundary");
        assert_eq!(child["held"]["keys"], json!(["Shift"]), "and holds what the source held: {child}");
        assert_eq!(child["verification"], json!([{"name": "ready-shown", "value": "true"}]), "{child}");
        assert_ne!(child["launch_id"], source);
    }
    // A retry finds the same child; methods are never substituted.
    assert_eq!(m.call_tool("state", branch("first")).await["launch_id"], first["launch_id"]);
    let (refused, error) = m.try_tool("state", json!({"operation": "branch", "saved_id": saved_id, "kind": "recipe"})).await;
    assert!(refused);
    assert_eq!(error["code"], "method_mismatch", "{error}");

    // A probe of the basis takes a hidden step in a child of its own and reads what follows; the source
    // stays where it was.
    let before = m.call_tool("status", json!({"launch_id": source})).await["t_us"].clone();
    let read = |selector: &str| json!({"step": "read", "op": {"type": "query", "target": css(selector), "props": ["text"]}});
    let probed = m.call_tool("state", json!({"operation": "probe", "saved_id": saved_id, "kind": "checkpoint", "steps": [
        {"step": "input", "op": {"operation": "key_up", "key": "Shift"}},
        {"step": "act", "action": {"type": "click", "target": css("#step")}, "then": {"type": "none"}},
        {"step": "wait", "op": {"type": "advance", "by_ms": 400}},
        read("#timer"), read("#worker"),
    ]})).await;
    assert_eq!(probed["completed"], 5, "{probed}");
    assert_eq!(probed["start"]["t_us"], boundary, "{probed}");
    assert_eq!(probed["steps"][0]["keys"], json!([]), "the child lets go of the Shift it took over: {probed}");
    assert_eq!([&probed["steps"][3]["values"]["text"], &probed["steps"][4]["values"]["text"]], [&json!("3"), &json!("3")], "{probed}");
    assert_eq!(m.call_tool("status", json!({"launch_id": source})).await["t_us"], before, "the source is unchanged");
    m.call_tool("stop", json!({"launch_id": probed["launch_id"]})).await;

    // The first child takes one more hidden step; the second and the source take none.
    let one = continuation(&mut m, &first["launch_id"], 1).await;
    let two = continuation(&mut m, &second["launch_id"], 0).await;
    let original = continuation(&mut m, &source, 0).await;
    assert_eq!(original, [json!("2"), json!("2"), json!("answer:2")]);
    assert_eq!(two, original, "a child continues the saved closure, timer, worker heap and held request");
    assert_eq!(one, [json!("3"), json!("3"), json!("answer:3")], "and on its own");

    // Each child's evidence names the basis; the source's run still replays: the image took no input.
    let stopped = m.call_tool("stop", json!({"launch_id": first["launch_id"]})).await;
    let origin: Value = serde_json::from_str(&std::fs::read_to_string(format!("{}/origin.json", stopped["bundle"].as_str().unwrap())).unwrap()).unwrap();
    assert_eq!(origin["saved_id"], saved_id);
    m.call_tool("stop", json!({"launch_id": second["launch_id"]})).await;
    m.call_tool("stop", json!({"launch_id": source})).await;
    let replay = m.call_tool("debug", json!({"launch_id": source, "operation": "replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
    // Deleting the basis removes its image.
    let image = ws.root().join("images").join(saved_id.as_str().unwrap());
    assert!(image.exists());
    assert_eq!(m.call_tool("state", json!({"operation": "delete_saved", "saved_id": saved_id})).await["deleted"], saved_id);
    assert!(!image.exists());
}
