//! `state probe` through the public MCP surface: a child of a saved basis runs bounded steps as one
//! request and answers and behaves as the same steps called separately do, however long the caller
//! waits between them.
mod support;
use serde_json::{Value, json};

fn css(selector: &str) -> Value {
    json!({"by": "css", "selector": selector})
}

fn pay_rule(origin: &str) -> Value {
    json!([{"id": "pay", "match": {"method": "POST", "url": format!("{origin}/api/pay")}, "action": {"type": "sequence",
        "after_last": "repeat_last", "steps": [{"status": 402, "body": "declined", "latency_ms": 1000}, {"status": 200, "body": "ok"}]}}])
}

fn recipe(origin: &str) -> Value {
    json!({
        "url": format!("{origin}/checkout.html"),
        "profile": {
            "initial_storage": {"origin": origin, "local": {"cart": r#"[{"name":"tea","price":2.5},{"name":"cup","price":4}]"#}},
            "initial_cookies": [{"origin": origin, "name": "session", "value": "ada", "path": "/", "same_site": "lax"}],
        },
        "rules": pay_rule(origin),
        "barrier": {"type": "until", "predicate": {"is": "attached", "target": {"by": "test_id", "id": "cart-ready"}}, "max_ms": 2000},
    })
}

fn query(selector: &str, prop: &str) -> Value {
    json!({"type": "query", "target": css(selector), "props": [prop]})
}

fn click(selector: &str, settle: bool) -> (Value, Value) {
    let then = if settle { json!({"type": "settle", "idle_window_ms": 50, "max_ms": 3000}) } else { json!({"type": "none"}) };
    (json!({"type": "click", "target": css(selector)}), then)
}

/// Every kind of step: a read, a state change, user input, time and the reads after each.
fn steps() -> Vec<Value> {
    let (pay, no_settle) = click("#pay", false);
    let (_, settle) = click("#pay", true);
    vec![
        json!({"step": "watch", "op": {"operation": "add", "name": "paid", "predicate": {"is": "text_equals", "target": css("#status"), "text": "Payment accepted"}}}),
        json!({"step": "read", "op": query("#count", "text")}),
        json!({"step": "apply", "changes": [{"target": css("#note"), "field": {"type": "value"}, "value": "ring twice"}]}),
        json!({"step": "read", "op": query("#note", "value")}),
        json!({"step": "act", "action": pay, "then": no_settle}),
        json!({"step": "read", "op": query("#status", "text")}),
        json!({"step": "wait", "op": {"type": "advance", "by_ms": 1000}}),
        json!({"step": "read", "op": query("#status", "text")}),
        json!({"step": "act", "action": pay, "then": settle}),
        json!({"step": "read", "op": query("#status", "text")}),
    ]
}

/// Wall time the caller lets pass before each separate step.
const DELAYS_MS: [u64; 10] = [0, 0, 250, 0, 700, 100, 0, 400, 50, 300];

/// The same steps as separate calls, after the caller's delays; a state apply at the boundary input
/// status reports.
async fn separately(m: &mut support::Mcp, id: &Value) -> Vec<Value> {
    let mut results = Vec::new();
    for (step, delay) in steps().into_iter().zip(DELAYS_MS) {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        let result = match step["step"].as_str().unwrap() {
            "read" => m.call_tool("observe", json!({"launch_id": id, "op": step["op"]})).await,
            "apply" => {
                let boundary = m.call_tool("input", json!({"launch_id": id, "operation": "status"})).await["boundary"].clone();
                m.call_tool("state", json!({"launch_id": id, "operation": "apply", "boundary": boundary, "changes": step["changes"]})).await
            }
            "act" => m.call_tool("act", json!({"launch_id": id, "action": step["action"], "then": step["then"]})).await,
            "wait" => m.call_tool("wait", json!({"launch_id": id, "op": step["op"]})).await,
            "watch" => {
                let mut op = step["op"].clone();
                op["launch_id"] = id.clone();
                m.call_tool("watch", op).await
            }
            other => panic!("{other}"),
        };
        results.push(result);
    }
    results
}

/// What a step answered, without the identities of the child and request that answered it.
fn comparable(v: &Value) -> Value {
    let mut v = v.clone();
    fn strip(v: &mut Value) {
        match v {
            Value::Object(m) => {
                for key in ["request_id", "launch_id", "ref", "context_id", "wall_ms"] {
                    m.remove(key);
                }
                m.values_mut().for_each(strip);
            }
            Value::Array(a) => a.iter_mut().for_each(strip),
            _ => {}
        }
    }
    strip(&mut v);
    v
}

fn trace(stopped: &Value) -> String {
    std::fs::read_to_string(std::path::Path::new(stopped["bundle"].as_str().unwrap()).join("trace.jsonl")).unwrap()
}

fn children(ws: &support::Workspace) -> usize {
    std::fs::read_dir(ws.root().join("runtimes")).map_or(0, |d| d.count())
}

#[tokio::test]
async fn relative_waits_keep_the_action_instant_after_intermediate_work() {
    let ws = support::workspace();
    let origin = format!("http://{}", support::serve());
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let saved = m.call_tool("state", json!({"operation":"save","kind":"recipe","recipe":recipe(&origin)})).await;
    let args = json!({"operation":"probe","saved_id":saved["saved_id"],"kind":"recipe","request_id":"relative",
        "steps":[
            {"step":"act","action":{"type":"click","target":css("#pay")},"then":{"type":"none"}},
            {"step":"wait","op":{"type":"until","predicate":{"is":"text_equals","target":css("#status"),"text":"Payment declined (402)"},"max_ms":2000}},
            {"step":"read","op":query("#status","text")},
            {"step":"wait_from","step_index":0,"after_ms":3000},
            {"step":"wait_from","step_index":0,"after_ms":2000},
            {"step":"wait_from","step_index":3,"after_ms":100}
        ]});
    let result = m.call_tool("state", args.clone()).await;
    assert_eq!(result["completed"], 6, "{result}");
    let anchor = result["steps"][0]["t_after_us"].as_u64().unwrap();
    assert_eq!(result["steps"][2]["values"]["text"], "Payment declined (402)", "{result}");
    for (step, offset) in [(3,3_000_000),(4,3_000_000),(5,3_100_000)] {
        assert_eq!(result["steps"][step]["t_us"], anchor+offset, "{result}");
    }
    assert_eq!(m.call_tool("state", args).await, result, "retry retains its instants and creates no new child");
    let count = children(&ws);
    for steps in [
        json!([{"step":"wait_from","step_index":0,"after_ms":1}]),
        json!([{"step":"read","op":query("#status","text")},{"step":"wait_from","step_index":2,"after_ms":1}]),
    ] {
        let (failed, error) = m.try_tool("state", json!({"operation":"probe","saved_id":saved["saved_id"],"kind":"recipe","steps":steps})).await;
        assert!(failed && error["code"]=="invalid_arguments", "{error}");
    }
    assert_eq!(children(&ws), count, "invalid anchors are refused before branching");
    m.call_tool("stop", json!({"launch_id":result["launch_id"]})).await;
    let replay = m.call_tool("debug", json!({"launch_id":result["launch_id"],"operation":"replay"})).await;
    assert_eq!(replay["equal"], true, "{replay}");
}

#[tokio::test]
async fn a_probe_answers_as_its_steps_called_separately() {
    let ws = support::workspace();
    let origin = format!("http://{}", support::serve());
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "recipe", "recipe": recipe(&origin)})).await;
    let saved_id = saved["saved_id"].clone();
    let probe = |request: &str, steps: Vec<Value>| json!({"operation": "probe", "saved_id": saved_id, "kind": "recipe", "steps": steps, "request_id": request});

    let probed = m.call_tool("state", probe("p", steps())).await;
    assert_eq!((probed["completed"].as_u64(), probed["of"].as_u64()), (Some(10), Some(10)), "{probed}");
    assert_eq!(probed["steps"][0]["value"], "false");
    assert_eq!(probed["steps"][0]["name"], "paid");
    assert_eq!(probed["steps"][5]["values"]["text"], "Paying…");
    assert_eq!(probed["steps"][7]["values"]["text"], "Payment declined (402)");
    assert_eq!(probed["steps"][9]["values"]["text"], "Payment accepted");
    // A watch the probe added saw its changes; its history is read after the probe.
    let paid = m.call_tool("watch", json!({"launch_id": probed["launch_id"], "operation": "history", "name": "paid", "changes_only": true})).await;
    assert_eq!(paid["samples"].as_array().unwrap().last().unwrap()["value"], "true", "{paid}");

    // A sibling of the same basis, the same steps one call at a time with the caller pausing between them.
    let sibling = m.call_tool("state", json!({"operation": "branch", "saved_id": saved_id, "kind": "recipe", "request_id": "p"})).await;
    assert_ne!(sibling["launch_id"], probed["launch_id"], "a branch under the probe's request id is another child");
    assert_eq!(sibling["boundary"]["t_us"], probed["start"]["t_us"]);
    let separate = separately(&mut m, &sibling["launch_id"]).await;
    for (n, (a, b)) in probed["steps"].as_array().unwrap().iter().zip(&separate).enumerate() {
        assert_eq!(comparable(a), comparable(b), "step {n}");
    }
    let status = m.call_tool("status", json!({"launch_id": sibling["launch_id"]})).await;
    assert_eq!(probed["end"]["t_us"], status["t_us"], "the probe ends where the separate steps did");
    // The probe's child stays where it ended, for inspection.
    assert_eq!(m.call_tool("status", json!({"launch_id": probed["launch_id"]})).await["t_us"], probed["end"]["t_us"]);

    // A retry answers the same, in the same child; other steps under its id conflict.
    let count = children(&ws);
    assert_eq!(m.call_tool("state", probe("p", steps())).await, probed);
    assert_eq!(children(&ws), count, "a retried probe creates no child");
    let (refused, error) = m.try_tool("state", probe("p", steps()[..2].to_vec())).await;
    assert!(refused);
    assert_eq!(error["code"], "request_id_conflict", "{error}");

    // A step that does not complete ends the probe; the steps after it do not run.
    let (missing, _) = click("#missing", false);
    let stopped = m.call_tool("state", probe("stopped", vec![steps()[1].clone(), json!({"step": "act", "action": missing}), steps()[5].clone()])).await;
    assert_eq!((stopped["completed"].as_u64(), stopped["of"].as_u64()), (Some(1), Some(3)), "{stopped}");
    assert_eq!(stopped["steps"].as_array().unwrap().len(), 2);
    assert!(stopped["steps"][1]["error"]["code"].is_string(), "{stopped}");

    // Bounds and unsupported steps are refused before any child starts.
    let count = children(&ws);
    let wait = |ms: u64| json!({"step": "wait", "op": {"type": "advance", "by_ms": ms}});
    for (steps, why) in [
        (vec![], "no steps"),
        (vec![wait(1); 33], "33 steps"),
        (vec![json!({"step": "wait", "op": {"type": "now"}})], "a clock read"),
        (vec![json!({"step": "read", "op": {"type": "scene", "image": true}})], "a scene image"),
    ] {
        let (refused, error) = m.try_tool("state", probe(&format!("refused-{}", steps.len()), steps)).await;
        assert!(refused, "{why}");
        assert_eq!(error["code"], "invalid_arguments", "{why}: {error}");
    }
    assert_eq!(children(&ws), count, "refused probes start nothing");
    assert_eq!(m.call_tool("state", json!({"operation": "describe_saved", "saved_id": saved_id})).await["sha256"], saved["sha256"]);

    // The two children behaved alike: the probe's steps and the delayed calls left the same behavior log.
    let probed_stop = m.call_tool("stop", json!({"launch_id": probed["launch_id"]})).await;
    let sibling_stop = m.call_tool("stop", json!({"launch_id": sibling["launch_id"]})).await;
    assert_eq!(frontcap_core::projection::compare_behavior(&trace(&probed_stop), &trace(&sibling_stop)), Ok(()));
    let other = m.call_tool("stop", json!({"launch_id": stopped["launch_id"]})).await;
    assert!(frontcap_core::projection::compare_behavior(&trace(&probed_stop), &trace(&other)).is_err(), "a child that ran other steps differs");
}

/// A return hours after paying runs in the probe that paid: each step is bounded by its own bound,
/// not by a total.
#[tokio::test]
async fn a_probe_returns_hours_later() {
    let ws = support::workspace();
    let origin = format!("http://{}", support::serve());
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "recipe", "recipe": recipe(&origin)})).await;
    let (pay, settle) = click("#pay", true);
    let later_ms: u64 = 2 * 3600 * 1000;
    let probed = m.call_tool("state", json!({"operation": "probe", "saved_id": saved["saved_id"], "kind": "recipe", "steps": [
        {"step": "act", "action": pay, "then": settle},
        {"step": "act", "action": pay, "then": settle},
        {"step": "wait", "op": {"type": "advance", "by_ms": later_ms}},
        {"step": "act", "action": {"type": "reload"}},
        {"step": "read", "op": query("#returning", "visible")},
    ]})).await;
    assert_eq!(probed["completed"], 5, "{probed}");
    assert_eq!(probed["steps"][4]["values"]["visible"], true, "{probed}");
    assert!(probed["end"]["t_us"].as_u64().unwrap() > later_ms * 1000, "{probed}");
    m.call_tool("stop", json!({"launch_id": probed["launch_id"]})).await;
}

/// What the payment status shows, and when.
async fn shown(m: &mut support::Mcp, id: &Value) -> (Value, Value) {
    let text = m.call_tool("observe", json!({"launch_id": id, "op": query("#status", "text")})).await["values"]["text"].clone();
    (text, m.call_tool("status", json!({"launch_id": id})).await["t_us"].clone())
}

#[tokio::test]
async fn a_probe_of_a_live_replay_basis_leaves_its_source_unchanged() {
    let ws = support::workspace();
    let origin = format!("http://{}", support::serve());
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let source = m.call_tool("launch", json!({"url": format!("{origin}/checkout.html"), "rules": pay_rule(&origin)})).await["launch_id"].clone();
    let (pay, settle) = click("#pay", true);
    m.call_tool("act", json!({"launch_id": source, "action": pay, "then": settle})).await;
    let before = shown(&mut m, &source).await;
    assert_eq!(before.0, "Payment declined (402)");

    let saved = m.call_tool("state", json!({"operation": "save", "kind": "replay", "from": source})).await;
    let probed = m.call_tool("state", json!({"operation": "probe", "saved_id": saved["saved_id"], "kind": "replay", "steps": [
        {"step": "read", "op": query("#status", "text")},
        {"step": "act", "action": pay, "then": settle},
        {"step": "read", "op": query("#status", "text")},
    ]})).await;
    assert_eq!(probed["completed"], 3, "{probed}");
    assert_eq!(probed["replayed"]["commands"], probed["replayed"]["of"], "{probed}");
    assert_eq!(probed["start"]["t_us"], before.1, "the child starts where the recording ends");
    assert_eq!(probed["steps"][0]["values"]["text"], "Payment declined (402)");
    assert_eq!(probed["steps"][2]["values"]["text"], "Payment accepted", "the child takes the route's next step");
    assert_eq!(shown(&mut m, &source).await, before, "the source is unchanged");

    for id in [&probed["launch_id"], &source] {
        m.call_tool("stop", json!({"launch_id": id})).await;
    }
}

/// A page that changes every 10 ms, so a long wait renders every frame and takes wall time.
const BUSY: &str = "<!doctype html><output id=count>0</output><script>let n = 0; setInterval(() => { count.textContent = ++n; }, 10);</script>";

fn busy_rules() -> Value {
    json!([{"id": "document", "match": {"url": "*busy.html"}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": BUSY}}])
}

/// The structured answer of a call sent with `send`.
async fn answer_of(rx: tokio::sync::oneshot::Receiver<Value>) -> Value {
    let response = tokio::time::timeout(std::time::Duration::from_secs(60), rx).await.expect("an answer within 60 s").unwrap();
    assert_eq!(response["result"]["isError"], false, "{response}");
    response["result"]["structuredContent"].clone()
}

/// Waits until `id` runs a request, then lets it run 200 ms more. A child being constructed has no
/// generation, or no recorder listening, yet.
async fn running(m: &mut support::Mcp, id: &Value) -> Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let (missing, status) = m.try_tool("status", json!({"launch_id": id})).await;
        assert!(std::time::Instant::now() < deadline, "{id} ran no request within 30 s: {status}");
        assert!(!missing || status["code"] == "not_found" || status["code"] == "transport_lost", "{status}");
        if !missing && !status["running"].is_null() {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            return status["running"].clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_running_wait_or_probe_is_cancelled_where_it_stands() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;

    // A wait: status names it, and session cancel stops it at a frame tick before its bound.
    let launched = m.call_tool("launch", json!({"url": "https://app.example.test/busy.html", "rules": busy_rules(), "passthrough_origins": [], "then": {"type": "none"}})).await;
    let id = launched["launch_id"].clone();
    let wait = m.send("tools/call", json!({"name": "wait", "arguments": {"launch_id": id, "request_id": "long", "op": {"type": "advance", "by_ms": 600_000}}})).await;
    assert_eq!(running(&mut m, &id).await, "long");
    let cancel = m.call_tool("session", json!({"operation": "cancel", "launch_id": id, "operation_id": "long"})).await;
    assert_eq!(cancel["cancelling"], true, "{cancel}");
    let waited = answer_of(wait).await;
    assert_eq!(waited["outcome"]["type"], "cancelled", "{waited}");
    assert!(waited["elapsed_us"].as_u64().unwrap() < 600_000_000, "{waited}");
    let (refused, error) = m.try_tool("session", json!({"operation": "cancel", "launch_id": id, "operation_id": "long"})).await;
    assert!(refused);
    assert_eq!(error["code"], "not_found", "a request that ended is not cancelled: {error}");

    // A probe: named by its request id alone; the wait it was running stops and its later step does not
    // run. Its child stays where the probe ended.
    let recipe = json!({"url": "https://app.example.test/busy.html", "rules": busy_rules(), "passthrough_origins": [], "barrier": {"type": "none"}});
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "recipe", "recipe": recipe})).await;
    let probe = m.send("tools/call", json!({"name": "state", "arguments": {"operation": "probe", "saved_id": saved["saved_id"], "kind": "recipe", "request_id": "slow", "steps": [
        {"step": "wait", "op": {"type": "advance", "by_ms": 600_000}},
        {"step": "read", "op": query("#count", "text")},
    ]}})).await;
    let child = loop {
        let registered = std::fs::read_to_string(ws.root().join("launches/mcp-probe-slow.json")).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok());
        if let Some(entry) = registered {
            break entry["launch_id"].clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    running(&mut m, &child).await;
    let cancel = m.call_tool("session", json!({"operation": "cancel", "operation_id": "slow"})).await;
    assert_eq!(cancel["launch_id"], child, "{cancel}");
    let probed = answer_of(probe).await;
    assert_eq!(probed["launch_id"], child);
    assert_eq!((probed["cancelled"].as_bool(), probed["completed"].as_u64()), (Some(true), Some(1)), "{probed}");
    assert_eq!(probed["steps"].as_array().map(Vec::len), Some(1), "{probed}");
    assert_eq!(probed["steps"][0]["outcome"]["type"], "cancelled", "{probed}");
    assert_eq!(m.call_tool("status", json!({"launch_id": child})).await["t_us"], probed["end"]["t_us"]);

    for id in [&id, &child] {
        m.call_tool("stop", json!({"launch_id": id})).await;
    }
}

#[tokio::test]
async fn a_probe_whose_child_recorder_dies_completes_on_retry_in_the_same_child() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let loaded = json!({"type": "until", "predicate": {"is": "attached", "target": css("#count")}, "max_ms": 1000});
    let recipe = json!({"url": "https://app.example.test/busy.html", "rules": busy_rules(), "passthrough_origins": [], "barrier": loaded});
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "recipe", "recipe": recipe})).await;
    let probe = json!({"operation": "probe", "saved_id": saved["saved_id"], "kind": "recipe", "request_id": "crash", "steps": [
        {"step": "read", "op": query("#count", "text")},
        {"step": "wait", "op": {"type": "advance", "by_ms": 20_000}},
        {"step": "read", "op": query("#count", "text")},
    ]});
    let sent = m.send("tools/call", json!({"name": "state", "arguments": probe})).await;
    let child = loop {
        let registered = std::fs::read_to_string(ws.root().join("launches/mcp-probe-crash.json")).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok());
        if let Some(entry) = registered {
            break entry["launch_id"].clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    };
    // The child's recorder dies while the probe's wait (its step 1) runs: once its journal records the
    // wait dispatched.
    let journal = ws.root().join(format!("runtimes/{}/journal.jsonl", child.as_str().unwrap()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !std::fs::read_to_string(&journal).unwrap_or_default().lines()
        .any(|l| l.contains(r#""step":"dispatched""#) && l.contains(r#""mcp-probe-1-crash""#))
    {
        assert!(std::time::Instant::now() < deadline, "the wait was never dispatched");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let runtime: Value = serde_json::from_str(&std::fs::read_to_string(ws.root().join(format!("runtimes/{}/runtime.json", child.as_str().unwrap()))).unwrap()).unwrap();
    let killed = std::process::Command::new("kill").arg("-9").arg(runtime["recorder"]["pid"].to_string()).status().unwrap();
    assert!(killed.success());
    let lost = tokio::time::timeout(std::time::Duration::from_secs(60), sent).await.unwrap().unwrap();
    assert_eq!(lost["result"]["isError"], true, "{lost}");
    assert!(lost["result"]["structuredContent"]["message"].as_str().unwrap().contains(child.as_str().unwrap()), "{lost}");

    // The same probe again takes the child back, its recorder relaunched: the wait finished as a single
    // wait recovers, and the read after it did not run.
    let count = std::fs::read_dir(ws.root().join("runtimes")).unwrap().count();
    let again = m.call_tool("state", probe.clone()).await;
    assert_eq!(again["launch_id"], child, "{again}");
    assert_eq!(std::fs::read_dir(ws.root().join("runtimes")).unwrap().count(), count, "no new child");
    assert_eq!((again["interrupted"].as_bool(), again["completed"].as_u64(), again["of"].as_u64()), (Some(true), Some(2), Some(3)), "{again}");
    assert_eq!(again["steps"][1]["outcome"]["type"], "reached", "{again}");
    assert_eq!(again["steps"][1]["elapsed_us"], 20_000_000, "{again}");
    assert_eq!(m.call_tool("state", probe).await, again, "and the same answer once more");
    m.call_tool("stop", json!({"launch_id": child})).await;
}

/// A pad that shows the last pointer event it received and the buttons then held.
const PAD: &str = "<!doctype html><style>body { margin: 0; } #pad { position: absolute; left: 20px; top: 20px; width: 240px; height: 140px; touch-action: none; }</style>\
<div id=pad>none</div><script>for (const t of ['pointerdown', 'pointermove', 'pointerup', 'pointercancel']) pad.addEventListener(t, e => {\
 if (t === 'pointerdown') pad.setPointerCapture(e.pointerId); pad.textContent = t + ' ' + e.buttons; });</script>";

/// Input steps continue the basis's held pointer: the recorder names the child's handle and boundary
/// when each step runs, which the caller cannot before the child exists.
#[tokio::test]
async fn input_steps_continue_the_held_pointer_of_the_basis() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rules = json!([{"id": "pad", "match": {"url": "*pad.html"}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": PAD}}]);
    let source = m.call_tool("launch", json!({"url": "https://app.example.test/pad.html", "rules": rules, "passthrough_origins": [], "then": {"type": "none"}})).await["launch_id"].clone();
    m.call_tool("wait", json!({"launch_id": source, "op": {"type": "advance", "by_ms": 51}})).await;
    let state = m.call_tool("input", json!({"launch_id": source, "operation": "status"})).await;
    m.call_tool("input", json!({"launch_id": source, "operation": "begin", "target": css("#pad"), "point": [40, 40], "boundary": state["boundary"]})).await;
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "replay", "from": source})).await;
    let probe = |steps: Value| json!({"operation": "probe", "saved_id": saved["saved_id"], "kind": "replay", "steps": steps});
    let input = |op: Value| json!({"step": "input", "op": op});
    let read = json!({"step": "read", "op": query("#pad", "text")});

    let cancelled = m.call_tool("state", probe(json!([
        input(json!({"operation": "move", "point": [100, 40]})), read,
        input(json!({"operation": "cancel"})), read,
        input(json!({"operation": "move", "point": [120, 40]})),
    ]))).await;
    let child = &cancelled["launch_id"];
    assert_eq!(cancelled["completed"], 4, "{cancelled}");
    assert_eq!(cancelled["steps"][0]["held"]["handle"]["launch_id"], *child, "the child's own handle: {cancelled}");
    assert_eq!(cancelled["steps"][0]["held"]["point"], json!([100, 40]));
    assert_eq!(cancelled["steps"][1]["values"]["text"], "pointermove 1");
    assert!(cancelled["steps"][2]["held"].is_null(), "{cancelled}");
    assert_eq!(cancelled["steps"][3]["values"]["text"], "pointercancel 0");
    assert_eq!(cancelled["steps"][4]["error"]["code"], "invalid_arguments", "no pointer is held after the cancel: {cancelled}");

    let released = m.call_tool("state", probe(json!([input(json!({"operation": "end"})), read, input(json!({"operation": "key_down", "key": "Shift"}))]))).await;
    assert_eq!(released["completed"], 3, "{released}");
    assert_eq!(released["steps"][1]["values"]["text"], "pointerup 0", "another child released the same press");
    assert_eq!(released["steps"][2]["keys"], json!(["Shift"]));
    let held = m.call_tool("input", json!({"launch_id": source, "operation": "status"})).await;
    assert_eq!(held["held"]["point"], json!([40, 40]), "the source still holds its press: {held}");

    for id in [&cancelled["launch_id"], &released["launch_id"], &source] {
        m.call_tool("stop", json!({"launch_id": id})).await;
    }
}
