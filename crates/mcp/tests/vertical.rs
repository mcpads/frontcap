mod support;

use serde_json::{Value, json};
use std::time::{Duration, Instant};

struct Launched {
    id: String,
    lease: String,
}

async fn launch(m: &mut support::Mcp, request_id: &str, url: String, rules: Value) -> Launched {
    let r = m.call_tool("launch", json!({"request_id": request_id, "url": url, "rules": rules, "then": {"type": "none"}})).await;
    assert_eq!(r["clock"]["t_us"], 0, "{r}");
    Launched { id: r["launch_id"].as_str().unwrap().into(), lease: r["lease"].as_str().unwrap().into() }
}

/// Criterion 1 at level `time`: returns (trace path, click time, first-true time, wall from act to until).
async fn toast_run(m: &mut support::Mcp, addr: std::net::SocketAddr, n: usize) -> (String, u64, u64, Duration) {
    let g = launch(m, &format!("l-{n}"), format!("http://{addr}/toast.html"), support::toast_rules()).await;
    m.call_tool("watch", json!({"launch_id": g.id, "lease": g.lease, "request_id": "w-1",
        "operation": "add", "name": "toast-shown", "predicate": {"is": "visible", "target": {"by": "css", "selector": ".toast"}}})).await;
    m.call_tool("wait", json!({"launch_id": g.id, "lease": g.lease, "request_id": "c-0", "op": {"type": "advance", "by_ms": 50}})).await;
    let wall = Instant::now();
    let act = m.call_tool("act", json!({"launch_id": g.id, "lease": g.lease, "request_id": "a-1",
        "action": {"type": "click", "target": {"by": "css", "selector": "#go"}},
        "then": {"type": "settle", "idle_window_ms": 250, "max_ms": 2000}})).await;
    assert_eq!(act["settle"]["type"], "quiescent", "{act}");
    let until = m.call_tool("wait", json!({"launch_id": g.id, "lease": g.lease, "request_id": "c-1",
        "op": {"type": "until", "watch": "toast-shown", "max_ms": 10000}})).await;
    let elapsed = wall.elapsed();
    assert_eq!(until["outcome"]["type"], "condition_met", "{until}");
    let click = act["t_before_us"].as_u64().unwrap();
    let shown = until["t_us"].as_u64().unwrap();
    // `settle` returned before the timer fired, with the toast still hidden.
    let history = m.call_tool("watch", json!({"launch_id": g.id, "operation": "history", "name": "toast-shown", "changes_only": false})).await;
    let settled_at = act["t_after_us"].as_u64().unwrap();
    for s in history["samples"].as_array().unwrap() {
        if s["t_us"].as_u64().unwrap() <= settled_at {
            assert_eq!(s["value"], "false", "{s}");
        }
    }
    let changes = m.call_tool("watch", json!({"launch_id": g.id, "operation": "history", "name": "toast-shown"})).await;
    let kept = changes["samples"].as_array().unwrap();
    assert_eq!(kept.iter().map(|s| s["value"].as_str().unwrap()).collect::<Vec<_>>(), ["false", "true"], "{changes}");
    assert_eq!(kept[1]["t_us"].as_u64().unwrap(), shown, "the change is the sample that met the condition");
    assert_eq!(changes["omitted"].as_u64().unwrap() as usize + kept.len(), history["samples"].as_array().unwrap().len());
    let stopped = m.call_tool("stop", json!({"launch_id": g.id, "lease": g.lease, "request_id": "s-1"})).await;
    assert_eq!(stopped["finalized"], true, "{stopped}");
    let trace = format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap());
    (trace, click, shown, elapsed)
}

#[tokio::test]
async fn toast_through_mcp_meets_criterion_1() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (_, click, shown, _) = toast_run(&mut m, addr, 1).await;
    assert_eq!(shown - click, 3_500_000, "first visibility at exactly click + 3500 ms");
}

#[tokio::test]
async fn five_runs_meet_criteria_2_and_3() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let mut runs = Vec::new();
    for n in 0..5 {
        let launch_wall = Instant::now();
        runs.push((toast_run(&mut m, addr, n).await, launch_wall.elapsed()));
    }
    let first = std::fs::read_to_string(&runs[0].0.0).unwrap();
    for ((trace, click, shown, _), _) in &runs {
        let text = std::fs::read_to_string(trace).unwrap();
        assert_eq!(frontcap_core::projection::compare_behavior(&first, &text), Ok(()), "{trace}");
        assert_eq!(shown - click, runs[0].0.2 - runs[0].0.1);
    }
    let mut walls: Vec<Duration> = runs.iter().map(|((_, _, _, w), _)| *w).collect();
    walls.sort();
    eprintln!("act → until wall times: {walls:?}; whole runs: {:?}", runs.iter().map(|(_, w)| *w).collect::<Vec<_>>());
    assert!(walls[2] < Duration::from_secs(1), "median {:?}", walls[2]);
}

#[tokio::test]
async fn unmatched_external_request_appears_in_effects() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, "l-1", format!("http://{addr}/blocked.html"), json!([])).await;
    m.call_tool("wait", json!({"launch_id": g.id, "lease": g.lease, "request_id": "c-0", "op": {"type": "advance", "by_ms": 50}})).await;
    let act = m.call_tool("act", json!({"launch_id": g.id, "lease": g.lease, "request_id": "a-1",
        "action": {"type": "click", "target": {"by": "css", "selector": "#go"}},
        "then": {"type": "settle", "idle_window_ms": 100, "max_ms": 1000}})).await;
    let effects = &act["effects"];
    assert_eq!(effects["unmatched_requests"], json!(["GET https://cdn.example.test/x.js"]), "{act}");
    assert!(effects["requests"].as_array().unwrap().iter().any(|r| r.as_str().unwrap().ends_with("fail(rule=default-block)")), "{act}");
    assert_eq!(effects["console"]["warn"], 1, "{act}");
    m.call_tool("stop", json!({"launch_id": g.id, "lease": g.lease, "request_id": "s-1"})).await;
}

async fn held_run(m: &mut support::Mcp, addr: std::net::SocketAddr, n: usize) -> String {
    let rules = json!([{"id": "hold", "match": {"url": "https://api.example.test/hold"}, "action": {"type": "hold"}}]);
    let g = launch(m, &format!("h-{n}"), format!("http://{addr}/hold.html"), rules).await;
    m.call_tool("wait", json!({"launch_id": g.id, "lease": g.lease, "request_id": "c-0", "op": {"type": "advance", "by_ms": 2000}})).await;
    let held = m.call_tool("network", json!({"launch_id": g.id, "operation": "list_held"})).await;
    let ordinal = held["held"][0]["ordinal"].as_str().expect("one held request").to_string();
    assert_eq!((held["held"][0]["method"].as_str(), held["held"][0]["url"].as_str()), (Some("GET"), Some("https://api.example.test/hold")), "{held}");
    m.call_tool("network", json!({"launch_id": g.id, "lease": g.lease, "request_id": "r-1",
        "operation": "respond", "ordinal": ordinal, "response": {"status": 200, "headers": {"access-control-allow-origin": "*"}, "body": "late"}})).await;
    m.call_tool("wait", json!({"launch_id": g.id, "lease": g.lease, "request_id": "c-1", "op": {"type": "advance", "by_ms": 100}})).await;
    let after = m.call_tool("network", json!({"launch_id": g.id, "operation": "list_held"})).await;
    assert_eq!(after["held"], json!([]));
    let stopped = m.call_tool("stop", json!({"launch_id": g.id, "lease": g.lease, "request_id": "s-1"})).await;
    std::fs::read_to_string(format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap())).unwrap()
}

#[tokio::test]
async fn held_request_is_answered_through_mcp() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let a = held_run(&mut m, addr, 1).await;
    let b = held_run(&mut m, addr, 2).await;
    assert_eq!(frontcap_core::projection::compare_behavior(&a, &b), Ok(()));
}

#[tokio::test]
async fn status_answers_during_a_running_until() {
    // Keep the wait running until the test releases an upstream answer. A held rule does not
    // stop virtual time: an idle page can finish all 30 seconds before a wall-clock sleep ends.
    let backend = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = backend.server_addr().to_ip().unwrap();
    let (arrived, arrival) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let request = backend.recv_timeout(Duration::from_secs(30)).unwrap().expect("upstream request");
        assert_eq!(request.url(), "/slow");
        let _ = arrived.send(());
        let _ = released.recv_timeout(Duration::from_secs(30));
        request.respond(tiny_http::Response::from_string("done")).unwrap();
    });
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rules = json!([
        {"id": "document", "match": {"url": format!("http://{addr}/app")}, "action": {
            "type": "mock", "status": 200, "headers": {"content-type": "text/html"},
            "body": "<!doctype html><script>setTimeout(() => fetch('/slow'), 100)</script>"}},
        {"id": "upstream", "match": {"url": format!("http://{addr}/slow")}, "action": {"type": "passthrough"}}
    ]);
    let g = launch(&mut m, "l-1", format!("http://{addr}/app"), rules).await;
    m.call_tool("watch", json!({"launch_id": g.id, "lease": g.lease, "request_id": "w-1",
        "operation": "add", "name": "never", "predicate": {"is": "attached", "target": {"by": "css", "selector": ".never"}}})).await;
    m.call_tool("wait", json!({"launch_id": g.id, "lease": g.lease, "request_id": "c-0", "op": {"type": "advance", "by_ms": 50}})).await;
    let until = m.send("tools/call", json!({"name": "wait", "arguments": {"launch_id": g.id, "lease": g.lease, "request_id": "c-1",
        "op": {"type": "until", "watch": "never", "max_ms": 30000}}})).await;
    tokio::time::timeout(Duration::from_secs(10), arrival).await.expect("the wait reaches upstream").unwrap();
    let started = Instant::now();
    let status = m.call_tool("status", json!({"launch_id": g.id})).await;
    assert!(started.elapsed() < Duration::from_millis(500), "status waited for the running until");
    assert_eq!(status["clock"]["state"], "advancing", "{status}");
    assert_eq!(status["running"], "c-1", "{status}");
    let (is_error, busy) = m.try_tool("wait", json!({"launch_id": g.id, "lease": g.lease, "request_id": "c-2", "op": {"type": "advance", "by_ms": 1}})).await;
    assert!(is_error);
    assert_eq!(busy["code"], "busy", "{busy}");
    release.send(()).unwrap();
    let done = until.await.unwrap();
    assert_eq!(done["result"]["isError"], false, "{done}");
    server.join().unwrap();
    m.call_tool("stop", json!({"launch_id": g.id, "lease": g.lease, "request_id": "s-1"})).await;
}

#[tokio::test]
async fn frame_level_is_unsupported() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (is_error, e) = m.try_tool("launch", json!({"request_id": "l-1", "url": "http://127.0.0.1:1/", "determinism": "frame"})).await;
    assert!(is_error);
    assert_eq!(e["code"], "unsupported_capability", "{e}");
    assert!(!ws.root().join("runtimes").exists(), "nothing was started");
}

#[tokio::test]
async fn a_retried_launch_returns_the_same_generation_and_lease() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let a = launch(&mut m, "l-1", format!("http://{addr}/appear.html"), json!([])).await;
    let b = launch(&mut m, "l-1", format!("http://{addr}/appear.html"), json!([])).await;
    assert_eq!((a.id.as_str(), a.lease.as_str()), (b.id.as_str(), b.lease.as_str()));
    m.call_tool("stop", json!({"launch_id": a.id, "lease": a.lease, "request_id": "s-1"})).await;
}

#[tokio::test]
async fn bootstrap_reports_capabilities_and_live_runtimes() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let before = m.call_tool("bootstrap", json!({})).await;
    assert_eq!(before["levels"], json!({"time": true, "frame": false}));
    assert_eq!(before["live_runtimes"], 0);
    let checkpoint = std::env::var_os("FRONTCAP_CELLS").is_some();
    let kinds = if checkpoint { json!(["recipe", "replay", "checkpoint"]) } else { json!(["recipe", "replay"]) };
    assert_eq!(before["basis_kinds"], kinds, "a checkpoint only where browsers run in cells");
    assert!(before["next_action"].as_str().unwrap().contains("session list"));
    assert_eq!(m.call_tool("session", json!({"operation":"list"})).await["generations"], json!([]));
    let g = launch(&mut m, "l-1", format!("http://{addr}/appear.html"), json!([])).await;
    let after = m.call_tool("bootstrap", json!({})).await;
    assert_eq!(after["live_runtimes"], 1, "{after}");
    assert_eq!(m.call_tool("session", json!({"operation":"list"})).await["generations"][0]["launch_id"], g.id);
    m.call_tool("stop", json!({"launch_id": g.id, "lease": g.lease, "request_id": "s-1"})).await;
}

#[tokio::test]
async fn invalid_passthrough_origins_do_not_start_or_reserve_a_launch() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let mut args = json!({"request_id":"origin-validation","url":format!("http://{addr}/appear.html"),"then":{"type":"none"}});
    for origin in ["localhost:8080", "https://example.test/path", "https://*.example.test"] {
        args["passthrough_origins"] = json!([origin]);
        let (failed, error) = m.try_tool("launch", args.clone()).await;
        assert!(failed && error["code"] == "invalid_arguments", "{error}");
        for name in ["launches", "runtimes", "runs"] {
            let path = ws.root().join(name);
            assert!(!path.exists() || std::fs::read_dir(path).unwrap().next().is_none(), "invalid input created {name}");
        }
    }
    args["passthrough_origins"] = json!([format!("http://{addr}")]);
    let launched = m.call_tool("launch", args).await;
    m.call_tool("stop", json!({"launch_id":launched["launch_id"]})).await;
}

#[tokio::test]
async fn a_duration_too_large_for_microseconds_is_invalid() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let (is_error, e) = m
        .try_tool("wait", json!({"launch_id": "g-1", "lease": "0123456789abcdef0123456789abcdef", "request_id": "c-1",
            "op": {"type": "advance", "by_ms": 18446744073709552u64}}))
        .await;
    assert!(is_error);
    assert_eq!(e["code"], "invalid_arguments", "{e}");
    assert_eq!(m.request("ping", json!({})).await["result"], json!({}), "the server is still up");
}

#[tokio::test]
async fn reattach_in_the_same_server_returns_the_lease_it_holds() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, "l-1", format!("http://{addr}/appear.html"), json!([])).await;
    let r = m.call_tool("session", json!({"operation": "reattach", "launch_id": g.id, "request_id": "re-1"})).await;
    assert_eq!(r["lease"], g.lease.as_str(), "{r}");
    m.call_tool("stop", json!({"launch_id": g.id, "lease": g.lease, "request_id": "s-1"})).await;
}

#[tokio::test]
async fn a_stopped_generation_answers_stop_again_and_points_to_its_bundle() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, "l-1", format!("http://{addr}/appear.html"), json!([])).await;
    let stop = json!({"launch_id": g.id, "lease": g.lease, "request_id": "s-1"});
    let first = m.call_tool("stop", stop.clone()).await;
    let again = m.call_tool("stop", stop).await;
    assert_eq!(first, again);
    let status = m.call_tool("status", json!({"launch_id": g.id})).await;
    assert_eq!(status["stopped"], true, "{status}");
    assert_eq!(status["bundle"], first["bundle"], "status of a stopped generation points to its bundle");
    let (is_error, e) = m.try_tool("act", json!({"launch_id": g.id, "lease": g.lease, "action": {"type": "click", "target": {"by": "css", "selector": "p"}}})).await;
    assert!(is_error);
    assert_eq!(e["code"], "stopped", "mutations still refuse: {e}");
    let inspected = m.call_tool("debug", json!({"launch_id": g.id, "operation": "bundle"})).await;
    assert_eq!(inspected["finalized"], true);
}

#[tokio::test]
async fn malformed_lines_never_stop_the_server() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    m.write_raw(b"\xff\xfe not utf-8\n").await;
    m.write_raw(b"[1,2]\n").await;
    // Their answers carry a null id; let them pass before waiting for the next one.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let null_id = m.request_raw(r#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#).await;
    assert_eq!(null_id["error"]["code"], -32600, "{null_id}");
    assert_eq!(m.request("ping", json!({})).await["result"], json!({}), "the server is still up");
}

#[tokio::test]
async fn a_zero_idle_settle_is_refused_before_anything_runs() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, "l-1", format!("http://{addr}/appear.html"), json!([])).await;
    let (is_error, e) = m.try_tool("wait", json!({"launch_id": g.id, "lease": g.lease, "request_id": "c-1", "op": {"type": "settle", "idle_window_ms": 0, "max_ms": 100}})).await;
    assert!(is_error);
    assert_eq!(e["code"], "invalid_arguments", "{e}");
    let now = m.call_tool("wait", json!({"launch_id": g.id, "op": {"type": "now"}})).await;
    assert_eq!(now["state"], "frozen");
    m.call_tool("stop", json!({"launch_id": g.id, "lease": g.lease, "request_id": "s-1"})).await;
}

fn act(g: &Launched, id: &str, action: Value) -> Value {
    json!({"launch_id": g.id, "lease": g.lease, "request_id": id, "action": action})
}

#[tokio::test]
async fn type_then_until_through_the_server() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, "l-1", format!("http://{addr}/type.html"), json!([])).await;
    m.call_tool("watch", json!({"launch_id": g.id, "lease": g.lease, "request_id": "w-1",
        "operation": "add", "name": "sent", "predicate": {"is": "text_equals", "target": {"by": "css", "selector": "#out"}, "text": "sent ok"}})).await;
    m.call_tool("wait", json!({"launch_id": g.id, "lease": g.lease, "request_id": "c-0", "op": {"type": "advance", "by_ms": 50}})).await;
    let q = json!({"by": "css", "selector": "#q"});
    m.call_tool("act", act(&g, "a-1", json!({"type": "fill", "target": q, "value": ""}))).await;
    m.call_tool("act", act(&g, "a-2", json!({"type": "type", "target": q, "text": "ok"}))).await;
    let mut submit = act(&g, "a-3", json!({"type": "press", "target": q, "key": "Enter"}));
    submit["then"] = json!({"type": "none"});
    let pressed = m.call_tool("act", submit).await;
    assert_eq!(pressed["effects"]["unmatched_requests"], json!([]), "{pressed}");
    let t = pressed["t_after_us"].as_u64().unwrap();
    let until = m.call_tool("wait", json!({"launch_id": g.id, "lease": g.lease, "request_id": "c-1", "op": {"type": "until", "watch": "sent", "max_ms": 1000}})).await;
    assert_eq!(until["outcome"]["type"], "condition_met", "{until}");
    assert_eq!(until["t_us"].as_u64().unwrap(), t + 200_000, "the reply timer after the submit");
    let waited = m.call_tool("act", act(&g, "a-4", json!({"type": "wait_actionable", "target": q, "max_ms": 100}))).await;
    assert_eq!(waited["waited"]["type"], "actionable", "{waited}");
}

#[tokio::test]
async fn navigate_back_and_reload_through_the_server() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, "l-1", format!("http://{addr}/type.html"), json!([])).await;
    m.call_tool("wait", json!({"launch_id": g.id, "lease": g.lease, "request_id": "c-0", "op": {"type": "advance", "by_ms": 50}})).await;
    let nav = m.call_tool("act", act(&g, "n-1", json!({"type": "navigate", "url": format!("http://{addr}/nav2.html")}))).await;
    assert_eq!(nav["effects"]["navigations"].as_array().unwrap().len(), 1, "{nav}");
    let back = m.call_tool("act", act(&g, "n-2", json!({"type": "back"}))).await;
    assert!(back["effects"]["navigations"][0].as_str().unwrap().contains("type.html"), "{back}");
    let reload = m.call_tool("act", json!({"launch_id": g.id, "lease": g.lease, "request_id": "n-3", "action": {"type": "reload"},
        "then": {"type": "settle", "idle_window_ms": 100, "max_ms": 1000}})).await;
    assert_eq!(reload["settle"]["type"], "quiescent", "{reload}");
    let (is_error, r) = m.try_tool("act", act(&g, "n-4", json!({"type": "wait_actionable", "target": {"by": "css", "selector": "#q"}, "max_ms": 10}))
        .as_object().map(|o| { let mut o = o.clone(); o.insert("then".into(), json!({"type": "settle", "idle_window_ms": 10, "max_ms": 10})); Value::Object(o) }).unwrap()).await;
    assert!(is_error && r["code"] == "invalid_arguments", "{r}");
}

/// The common loop needs neither a lease (this server holds it) nor request ids (the server makes them).
#[tokio::test]
async fn the_common_loop_needs_no_lease_or_request_id() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/toast.html"), "rules": support::toast_rules()})).await;
    let id = r["launch_id"].as_str().unwrap().to_string();
    m.call_tool("watch", json!({"launch_id": id, "operation": "add", "name": "shown", "predicate": {"is": "visible", "target": {"by": "css", "selector": ".toast"}}})).await;
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 50}})).await;
    m.call_tool("act", json!({"launch_id": id, "action": {"type": "click", "target": {"by": "css", "selector": "#go"}}})).await;
    let until = m.call_tool("wait", json!({"launch_id": id, "op": {"type": "until", "watch": "shown", "max_ms": 10000}})).await;
    assert_eq!(until["outcome"]["type"], "condition_met", "{until}");
    m.call_tool("stop", json!({"launch_id": id})).await;
}

#[tokio::test]
async fn a_reserved_request_id_is_refused() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/toast.html")})).await;
    let id = r["launch_id"].as_str().unwrap().to_string();
    let (is_error, e) = m.try_tool("wait", json!({"launch_id": id, "request_id": "mcp-1", "op": {"type": "advance", "by_ms": 1}})).await;
    assert!(is_error && e["code"] == "invalid_arguments" && e["message"].as_str().unwrap().contains("mcp-"), "{e}");
    // A branch takes the same ids as any state-changing call.
    let saved = m.call_tool("state", json!({"operation": "save", "kind": "replay", "from": id})).await;
    for request_id in ["mcp-1".to_string(), "x".repeat(101)] {
        let (is_error, e) = m.try_tool("state", json!({"operation": "branch", "saved_id": saved["saved_id"], "kind": "replay", "request_id": request_id})).await;
        assert!(is_error && e["code"] == "invalid_arguments", "{e}");
    }
}

/// `launch` returns a loaded page: it settles first unless told not to.
#[tokio::test]
async fn launch_returns_a_loaded_page() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/toast.html"), "rules": support::toast_rules()})).await;
    assert_eq!(r["settle"]["type"], "quiescent", "{r}");
    assert!(r["clock"]["t_us"].as_u64().unwrap() > 0, "{r}");
    let snap = m.call_tool("observe", json!({"launch_id": r["launch_id"], "op": {"type": "snapshot"}})).await;
    assert!(snap["nodes"].as_array().unwrap().iter().any(|n| n["role"] == "button"), "{snap}");
    let raw = m.call_tool("launch", json!({"url": format!("http://{addr}/toast.html"), "then": {"type": "none"}})).await;
    assert_eq!((raw["clock"]["t_us"].as_u64(), &raw["settle"]["type"]), (Some(0), &json!("not_requested")), "{raw}");
}

/// A page that never quiets down still launches; the settle reports what kept it busy.
#[tokio::test]
async fn launch_of_a_busy_page_reports_the_exhausted_settle() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rules = json!([{"id": "hold", "match": {"url": "https://api.example.test/hold"}, "action": {"type": "hold"}}]);
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/hold.html"), "rules": rules,
        "then": {"type": "settle", "idle_window_ms": 100, "max_ms": 500}})).await;
    assert_eq!(r["settle"]["type"], "budget_exhausted", "{r}");
    m.call_tool("stop", json!({"launch_id": r["launch_id"]})).await;
}

/// `wait` takes the predicate itself: one call, no watch left behind, and a retry is answered alike.
#[tokio::test]
async fn wait_until_a_predicate_in_one_call() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/toast.html"), "rules": support::toast_rules()})).await;
    let id = r["launch_id"].as_str().unwrap().to_string();
    let act = m.call_tool("act", json!({"launch_id": id, "action": {"type": "click", "target": {"by": "css", "selector": "#go"}}})).await;
    // The act settled by default; the toast is timed from the click itself.
    assert_eq!(act["settle"]["type"], "quiescent", "{act}");
    let clicked = act["t_before_us"].as_u64().unwrap();
    m.call_tool("watch", json!({"launch_id": id, "operation": "add", "name": "kept",
        "predicate": {"is": "attached", "target": {"by": "css", "selector": "body"}}})).await;
    let until = json!({"launch_id": id, "request_id": "w-1",
        "op": {"type": "until", "predicate": {"is": "visible", "target": {"by": "css", "selector": ".toast"}}, "max_ms": 10000}});
    let first = m.call_tool("wait", until.clone()).await;
    assert_eq!(first["outcome"]["type"], "condition_met", "{first}");
    assert_eq!(first["t_us"].as_u64().unwrap(), clicked + 3_500_000, "{first}");
    let settled = act["t_after_us"].as_u64().unwrap();
    assert_eq!((first["started_us"].as_u64(), first["elapsed_us"].as_u64()), (Some(settled), Some(clicked + 3_500_000 - settled)), "{first}");
    assert_eq!(m.call_tool("wait", until.clone()).await, first, "a retry is answered from the journal");
    let saved = m.call_tool("session", json!({"launch_id": id, "operation": "result", "operation_id": "w-1"})).await;
    assert_eq!(saved["result"]["state"], "completed", "the public request id must identify the whole wait: {saved}");
    let mut changed = until;
    changed["op"]["max_ms"] = json!(1);
    let (is_error, conflict) = m.try_tool("wait", changed).await;
    assert!(is_error && conflict["code"] == "request_id_conflict", "{conflict}");
    let watches = m.call_tool("watch", json!({"launch_id": id, "operation": "list"})).await;
    assert_eq!(watches["watches"].as_array().map(|w| w.len()), Some(1), "temporary cleanup changed a user watch: {watches}");
    assert_eq!(watches["watches"][0]["name"], "kept", "{watches}");
    assert!(watches["watches"][0]["predicate"].is_object(), "a watch is listed with its predicate: {watches}");
    m.call_tool("watch", json!({"launch_id": id, "operation": "remove", "name": "kept"})).await;
    for op in [json!({"type": "until", "max_ms": 1}), json!({"type": "until", "watch": "w", "predicate": {"is": "attached", "target": {"by": "css", "selector": "b"}}, "max_ms": 1})] {
        let (is_error, e) = m.try_tool("wait", json!({"launch_id": id, "op": op})).await;
        assert!(is_error && e["code"] == "invalid_arguments", "{e}");
    }
}

/// Drawers describe their operations on request and refuse an operation they do not have.
#[tokio::test]
async fn drawers_describe_their_operations() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    for (drawer, ops) in [
        ("network", vec!["list_held", "respond", "fail"]),
        ("watch", vec!["add", "remove", "list", "history"]),
        ("session", vec!["list", "reattach", "release", "result", "dialog_rules", "answer_dialog"]),
        ("state", vec!["storage_scope", "read", "apply", "save", "describe_saved", "branch"]),
        ("input", vec!["status", "begin", "move", "press", "release", "touch_add", "touch_move", "touch_end", "end", "cancel", "wheel", "key_down", "key_up", "key_repeat", "compose", "commit"]),
        ("debug", vec!["evaluate", "console", "requests", "result", "bundle", "finalize", "replay"]),
    ] {
        let d = m.call_tool(drawer, json!({"operation": "describe"})).await;
        for op in &ops {
            assert!(d["operations"][op]["fields"].is_array(), "{drawer} indexes {op}: {d}");
        }
        if drawer == "watch" {
            let d = m.call_tool(drawer, json!({"operation": "describe", "of": "add"})).await;
            let text = d["schema"].to_string();
            for condition in ["attached", "visible", "opacity_at_least", "text_equals", "value_equals", "count_at_least"] {
                assert!(text.contains(&format!("\"{condition}\"")), "watch describes {condition}: {d}");
            }
            assert!(text.contains("\"selector\""), "predicate locators remain discoverable: {d}");
            let locators = d["schema"]["$defs"]["Locator"]["oneOf"].as_array().unwrap();
            let kinds: Vec<_> = locators.iter().map(|v| v["properties"]["by"]["const"].as_str().unwrap()).collect();
            assert!(kinds.contains(&"role"), "watch discovery includes engine-backed role locators: {d}");
            for kind in ["ref", "css", "text", "test_id", "frame"] {
                assert!(kinds.contains(&kind), "watch locator {kind} remains discoverable: {d}");
            }
            let frame = locators.iter().find(|v| v["properties"]["by"]["const"] == "frame").unwrap();
            for field in ["frame", "target"] {
                assert_eq!(frame["properties"][field]["$ref"], "#/$defs/Locator", "nested scopes use the supported watch locators");
            }
        }
        if drawer == "state" {
            assert!(d["signed_in"].as_str().is_some_and(|s| s.contains("cookie")), "state describe says how to stand in for signing in: {d}");
            let d = m.definition(drawer, "apply", "ObservationBoundary").await;
            for field in ["launch_id", "state_revision", "t_us", "context_id", "document_id"] {
                assert!(d["properties"].get(field).is_some(), "boundary field {field}: {d}");
            }
        }
        let (is_error, e) = m.try_tool(drawer, json!({"operation": "warp", "launch_id": "g-1"})).await;
        assert!(is_error && e["code"] == "invalid_arguments" && e["recovery"].as_str().unwrap().contains("describe"), "{e}");
    }
    let act = m.call_tool("act", json!({"describe": true})).await;
    assert!(act["input_schema"]["$defs"]["Locator"]["oneOf"].as_array().unwrap().iter()
        .any(|v| v["properties"]["by"]["const"] == "role"), "actions still support role locators");
    // The rules `launch` lists as plain objects are described in full by the network drawer.
    let rules = m.call_tool("network", json!({"operation": "describe", "of": "set_rules"})).await["schema"].to_string();
    for action in ["mock", "sequence", "hold", "passthrough", "fail", "record", "replay", "stream_mock"] {
        assert!(rules.contains(&format!("\"{action}\"")), "{action}: {rules}");
    }
}

/// A request the action itself sends is in its effects even without a settle: it is issued at the
/// action's instant, and the act handles it before returning.
#[tokio::test]
async fn an_act_reports_the_requests_it_sent() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rules = json!([{"id": "save", "match": {"url": "https://api.example.test/profile"}, "action": {"type": "mock", "status": 200, "body": "{}"}}]);
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/profile.html"), "rules": rules})).await;
    let act = m.call_tool("act", json!({"launch_id": r["launch_id"], "action": {"type": "click", "target": {"by": "css", "selector": "#save"}}})).await;
    let requests = act["effects"]["requests"].to_string();
    assert!(requests.contains("POST https://api.example.test/profile"), "{act}");
    let stopped = m.call_tool("stop", json!({"launch_id": r["launch_id"]})).await;
    let trace = std::fs::read_to_string(format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap())).unwrap();
    assert!(!trace.contains("unintercepted_request"), "every request was intercepted: {trace}");
}

#[tokio::test]
async fn long_request_ids_and_unknown_fields_are_refused() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/toast.html"), "then": {"type": "none"}})).await;
    let id = r["launch_id"].as_str().unwrap().to_string();
    let long = "a".repeat(101);
    let (is_error, e) = m.try_tool("wait", json!({"launch_id": id, "request_id": long, "op": {"type": "advance", "by_ms": 1}})).await;
    assert!(is_error && e["code"] == "invalid_arguments", "{e}");
    let (is_error, e) = m.try_tool("network", json!({"launch_id": id, "operation": "list_held", "bogus": 1})).await;
    assert!(is_error && e["code"] == "invalid_arguments", "{e}");
    let (is_error, e) = m.try_tool("wait", json!({"launch_id": id, "op": {"type": "now", "bogus": 1}})).await;
    assert!(is_error && e["code"] == "invalid_arguments", "{e}");
}

/// A retried launch with other `then` is another request; a settle that fails still names the generation.
#[tokio::test]
async fn a_launch_retry_with_another_then_conflicts() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    m.call_tool("launch", json!({"request_id": "l-1", "url": format!("http://{addr}/toast.html")})).await;
    let (is_error, e) = m.try_tool("launch", json!({"request_id": "l-1", "url": format!("http://{addr}/toast.html"), "then": {"type": "none"}})).await;
    assert!(is_error && e["code"] == "request_id_conflict", "{e}");
}

/// `wait_actionable` with `then` is refused before anything is journaled: the same id can then be used
/// without `then`.
#[tokio::test]
async fn a_refused_then_on_wait_actionable_leaves_the_request_id_free() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/type.html")})).await;
    let wait = json!({"type": "wait_actionable", "target": {"by": "css", "selector": "#q"}, "max_ms": 10});
    let (is_error, e) = m.try_tool("act", json!({"launch_id": r["launch_id"], "request_id": "w-1", "action": wait,
        "then": {"type": "settle", "idle_window_ms": 10, "max_ms": 10}})).await;
    assert!(is_error && e["code"] == "invalid_arguments", "{e}");
    let ok = m.call_tool("act", json!({"launch_id": r["launch_id"], "request_id": "w-1", "action": wait})).await;
    assert_eq!(ok["waited"]["type"], "actionable", "{ok}");
}

#[tokio::test]
async fn watch_names_of_the_server_and_bad_launch_ids_are_refused_clearly() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/toast.html"), "then": {"type": "none"}})).await;
    let (is_error, e) = m.try_tool("watch", json!({"launch_id": r["launch_id"], "operation": "add", "name": "mcp-wait-x",
        "predicate": {"is": "attached", "target": {"by": "css", "selector": "#go"}}})).await;
    assert!(is_error && e["code"] == "invalid_arguments" && e["message"].as_str().unwrap().contains("mcp-"), "{e}");
    let (is_error, e) = m.try_tool("network", json!({"launch_id": 7, "operation": "list_held"})).await;
    assert!(is_error && e["message"].as_str().unwrap().contains("launch_id"), "{e}");
}

/// `capture`: the last rendered frame with its instant, inline as an MCP image when asked, and a
/// recording of the frames of a wait.
#[tokio::test]
async fn capture_screenshots_and_recordings_through_the_server() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/color.html"), "then": {"type": "none"}})).await;
    let id = r["launch_id"].clone();
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 10}})).await;
    m.call_tool("capture", json!({"launch_id": id, "op": {"type": "record_start"}})).await;
    m.call_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 50}})).await;
    let shot = m.call_tool("capture", json!({"launch_id": id, "op": {"type": "screenshot"}})).await;
    assert_eq!((shot["t_us"].as_u64(), &shot["current"]), (Some(60_000), &json!(true)), "{shot}");
    assert!(std::path::Path::new(shot["path"].as_str().unwrap()).exists());
    let raw = m.request("tools/call", json!({"name": "capture", "arguments": {"launch_id": id, "op": {"type": "screenshot", "inline": true}}})).await;
    let image = raw["result"]["content"].as_array().unwrap().iter().find(|c| c["type"] == "image").unwrap_or_else(|| panic!("{raw}"));
    assert_eq!(image["mimeType"], "image/png");
    assert!(image["data"].as_str().unwrap().starts_with("iVBOR"), "a PNG");
    let rec = m.call_tool("capture", json!({"launch_id": id, "op": {"type": "record_stop"}})).await;
    // A recorded frame is at the instant its state was drawn: the green of 50 ms, drawn at the tick at 51 ms
    // on the patched engine (engine patch 6).
    let patched = browser_pin::engine::patched_binary(&support::repo_root(), browser_pin::current_platform().unwrap()).unwrap().is_some();
    let drawn: u64 = if patched { 51_000 } else { 60_000 };
    assert!(rec["frames"].as_array().unwrap().iter().any(|f| f["t_us"] == drawn), "{rec}");
    let recorded = m.call_tool("capture", json!({"launch_id":id,"op":{"type":"recorded_frames"}})).await;
    assert_eq!(recorded["frames"], rec["frames"]);
    assert_eq!(recorded["incomplete_tail"], false);
    let frame = rec["frames"].as_array().unwrap().iter().find(|f| f["t_us"]==drawn).unwrap().clone();
    let selected = m.call_tool("capture", json!({"launch_id":id,"op":{"type":"recorded_frames","since_us":drawn,"until_us":drawn}})).await;
    assert!(selected["frames"].as_array().unwrap().iter().all(|f| f["t_us"]==drawn));
    let spec = frontcap_recorder::runtime::load_spec(&ws.root().join("runtimes").join(id.as_str().unwrap())).unwrap().unwrap();
    let bundle = spec.runs_root.join(spec.run_id.as_str());
    let expected = std::fs::read(bundle.join(frame["file"].as_str().unwrap())).unwrap();
    let read_args = json!({"name":"capture","arguments":{"launch_id":id,"op":{"type":"recorded_frame","file":frame["file"]}}});
    let read = m.request("tools/call",read_args.clone()).await;
    assert_eq!(read["result"]["structuredContent"], frame, "{read}");
    let image = read["result"]["content"].as_array().unwrap().iter().find(|c|c["type"]=="image").unwrap();
    assert_eq!(frontcap_core::encode::unbase64(image["data"].as_str().unwrap()).unwrap(), expected);
    let sheet_args=json!({"name":"capture","arguments":{"launch_id":id,"op":{"type":"keyframes","max_frames":3}}});
    let sheet=m.request("tools/call",sheet_args.clone()).await;
    let metadata=&sheet["result"]["structuredContent"];
    assert!(metadata["tiles"].as_array().is_some_and(|t|!t.is_empty()),"{sheet}");
    assert_eq!(metadata["incomplete_tail"],false);
    let image=sheet["result"]["content"].as_array().unwrap().iter().find(|c|c["type"]=="image").unwrap();
    let png=frontcap_core::encode::unbase64(image["data"].as_str().unwrap()).unwrap();
    assert_eq!(std::fs::read(bundle.join(metadata["file"].as_str().unwrap())).unwrap(),png);
    for tile in metadata["tiles"].as_array().unwrap() {
        assert!(rec["frames"].as_array().unwrap().contains(&tile["source"]));
    }
    let history=m.call_tool("debug",json!({"launch_id":id,"operation":"events","limit":2})).await;
    assert_eq!(history["events"].as_array().unwrap().len(),2);
    let next=m.call_tool("debug",json!({"launch_id":id,"operation":"events","after_seq":history["next_after"],"limit":2})).await;
    assert!(next["events"][0]["seq"].as_u64().unwrap()>history["events"][1]["seq"].as_u64().unwrap());
    assert_eq!(m.call_tool("status",json!({"launch_id":id})).await["t_us"],60_000,"evidence reads do not move time");
    let stopped = m.call_tool("stop",json!({"launch_id":id})).await;
    assert!(stopped["finalized"].as_bool().unwrap(),"{stopped}");
    let mut reader = support::Mcp::start_in(&ws.root()).await;
    assert_eq!(reader.request("tools/call",read_args).await["result"], read["result"],"stopped evidence needs no lease or browser");
    assert_eq!(reader.request("tools/call",sheet_args).await["result"],sheet["result"],"contact sheets derive identical evidence after stop");
    let artifacts=reader.call_tool("debug",json!({"launch_id":id,"operation":"artifacts"})).await;
    assert_eq!(artifacts["artifacts"].as_array().unwrap().len(),1,"{artifacts}");
    assert_eq!(artifacts["artifacts"][0]["file"],metadata["file"]);
    assert_eq!(artifacts["artifacts"][0]["sha256"],metadata["sha256"]);
    if std::process::Command::new("ffmpeg").arg("-version").output().is_ok_and(|o|o.status.success()) {
        let movie=reader.call_tool("debug",json!({"launch_id":id,"operation":"video","tail_ms":100})).await;
        let bytes=std::fs::read(bundle.join(movie["file"].as_str().unwrap())).unwrap();
        assert_eq!(movie["sha256"],frontcap_core::canonical::sha256_bytes_hex(&bytes));
        assert_eq!(movie["sources"].as_array().unwrap().len(),rec["frames"].as_array().unwrap().len());
        let artifacts=reader.call_tool("debug",json!({"launch_id":id,"operation":"artifacts"})).await;
        assert_eq!(artifacts["artifacts"].as_array().unwrap().len(),2);
    } else {
        eprintln!("SKIP: ffmpeg unavailable; offline video MCP path remains unqualified on this host");
    }



    let history=reader.call_tool("debug",json!({"launch_id":id,"operation":"events","limit":100})).await;
    let event=history["events"].as_array().unwrap().iter().rev().find(|e|e["kind"]=="clock").unwrap();
    let raw=reader.request("tools/call",json!({"name":"capture","arguments":{"launch_id":id,"op":{"type":"keyframes","event_seqs":[event["seq"]]}}})).await;
    let selected=&raw["result"]["structuredContent"];
    assert_eq!(selected["event_selection"]["trace_prefix_sha256"],history["complete_prefix_sha256"],"{raw}");
    assert_eq!(selected["event_selection"]["matches"][0]["event"],*event);
    assert!(raw["result"]["content"].as_array().unwrap().iter().any(|c|c["type"]=="image"));
    for tile in selected["tiles"].as_array().unwrap() { assert!(rec["frames"].as_array().unwrap().contains(&tile["source"])); }
    let (failed, error) = reader.try_tool("capture",json!({"launch_id":id,"op":{"type":"recorded_frame","file":"../spec.json"}})).await;
    assert!(failed && error["code"]=="not_found","{error}");
}

/// A run an agent drove with the defaults (generated request ids, the launch settle, a predicate wait)
/// replays equal through `debug replay` and through `frontcap-mcp replay`.
#[tokio::test]
async fn a_stopped_run_replays_equal() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/toast.html"), "rules": support::toast_rules()})).await;
    let id = r["launch_id"].as_str().unwrap().to_string();
    let snapshot = m.call_tool("observe", json!({"launch_id": id, "op": {"type": "snapshot"}})).await;
    let go = snapshot["nodes"].as_array().unwrap().iter().find(|n| n["role"] == "button").unwrap_or_else(|| panic!("{snapshot}"))["ref"].clone();
    m.call_tool("act", json!({"launch_id": id, "action": {"type": "click", "target": {"by": "ref", "id": go}}})).await;
    let until = m.call_tool("wait", json!({"launch_id": id, "op": {"type": "until", "predicate": {"is": "visible", "target": {"by": "css", "selector": ".toast"}}, "max_ms": 10000}})).await;
    assert_eq!(until["outcome"]["type"], "condition_met", "{until}");
    let stopped = m.call_tool("stop", json!({"launch_id": id})).await;
    let bundle = stopped["bundle"].as_str().unwrap().to_string();
    // The launch settle, the act (with its settle), and one recorder-owned predicate wait.
    let commands = frontcap_core::inputs::read_commands(std::path::Path::new(&bundle)).unwrap();
    assert_eq!(commands.len(), 3, "{commands:#?}");
    assert_eq!(commands[2].op["op"], "until_predicate");
    assert_eq!(commands[1].refs.keys().collect::<Vec<_>>(), vec![go.as_str().unwrap()], "the act's reference has its resolution record");

    let report = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    assert_eq!(report["equal"], true, "{report}");
    assert_eq!(report["replayed"], report["commands"], "{report}");

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_frontcap-mcp"))
        .args(["replay", &bundle])
        .env("FRONTCAP_WORKSPACE", ws.root())
        .env("FRONTCAP_RECORDER", support::recorder_bin())
        .env("FRONTCAP_HOME", support::repo_root())
        .output()
        .unwrap();
    let printed: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
    assert_eq!((out.status.code(), printed["equal"].clone()), (Some(0), json!(true)), "{printed}");
}

#[tokio::test]
async fn replay_of_a_live_generation_is_refused() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/toast.html"), "rules": support::toast_rules()})).await;
    let (is_error, e) = m.try_tool("debug", json!({"launch_id": r["launch_id"], "operation": "replay"})).await;
    assert!(is_error, "{e}");
    assert_eq!(e["code"], "not_finalized", "{e}");
}

/// Drag and upload through the server, and the run replays equal from the bundle's copy of the file.
#[tokio::test]
async fn drag_and_upload_through_mcp_and_replay() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/drag.html")})).await;
    let id = r["launch_id"].as_str().unwrap().to_string();
    let dragged = m.call_tool("act", json!({"launch_id": id, "then": {"type": "none"}, "action": {"type": "drag", "target": {"by": "css", "selector": "#src"}, "to": {"by": "css", "selector": "#dst"}}})).await;
    assert_eq!(dragged["t_after_us"], dragged["t_before_us"], "drag without a progress grant: {dragged}");
    let paced = m.call_tool("act", json!({"launch_id": id, "then": {"type": "none"}, "action": {
        "type": "drag", "target": {"by": "css", "selector": "#handle"}, "offset": {"dx": 90, "dy": 0}, "steps": 3, "interval_ms": 10
    }})).await;
    assert_eq!(paced["t_after_us"].as_u64().unwrap() - paced["t_before_us"].as_u64().unwrap(), 30_000, "only the requested progress: {paced}");
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("notes.txt");
    std::fs::write(&file, "hello").unwrap();
    m.call_tool("act", json!({"launch_id": id, "action": {"type": "upload", "target": {"by": "css", "selector": "#f"}, "files": [file.display().to_string()]}})).await;
    let log = m.call_tool("debug", json!({"launch_id": id, "operation": "evaluate", "expression": "JSON.stringify(log.map(e => e[0]))"})).await;
    let names: Vec<String> = serde_json::from_str(log["value"].as_str().unwrap()).unwrap();
    assert!(names.contains(&"drop".to_string()) && names.contains(&"change".to_string()), "{names:?}");
    m.call_tool("stop", json!({"launch_id": id})).await;
    drop(dir); // the replay reads the bundle's copy, not the original file
    let report = m.call_tool("debug", json!({"launch_id": id, "operation": "replay"})).await;
    // Every command answered as recorded — the upload from the bundle's copy, and the page's event log
    // (by its hash) — including frozen and explicitly paced drag instants.
    assert!(report["diverged"].is_null(), "{report}");
    assert_eq!(report["equal"], true, "{report}");
}

/// A link click navigates through the server, and the settle after it ends in the new document.
#[tokio::test]
async fn a_link_click_navigates_through_mcp() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/link.html")})).await;
    let id = r["launch_id"].as_str().unwrap().to_string();
    let act = m.call_tool("act", json!({"launch_id": id, "action": {"type": "click", "target": {"by": "css", "selector": "#l"}}})).await;
    assert_eq!(act["effects"]["navigations"].as_array().map(|n| n.len()), Some(1), "{act}");
    // Without `then`, the act lets the page react until it is quiet (fresh-consumer run 5).
    assert_eq!(act["settle"]["type"], "quiescent", "{act}");
    let path = m.call_tool("debug", json!({"launch_id": id, "operation": "evaluate", "expression": "location.pathname"})).await;
    assert_eq!(path["value"], "/nav2.html");
}

#[tokio::test]
async fn saved_operation_results_are_read_without_a_lease_or_reexecution() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, "result-launch", format!("http://{addr}/toast.html"), support::toast_rules()).await;
    let unknown = m.call_tool("session", json!({"launch_id":g.id,"operation":"result","operation_id":"result-advance"})).await;
    assert_eq!(unknown["result"]["state"], "unknown", "{unknown}");
    let advanced = m.call_tool("wait", json!({"launch_id":g.id,"request_id":"result-advance","op":{"type":"advance","by_ms":50}})).await;
    assert_eq!(advanced["t_us"], 50_000, "{advanced}");
    m.call_tool("session", json!({"launch_id":g.id,"operation":"release"})).await;
    for _ in 0..2 {
        let result = m.call_tool("session", json!({"launch_id":g.id,"operation":"result","operation_id":"result-advance"})).await;
        assert_eq!(result["result"]["state"], "completed", "{result}");
        assert_eq!(result["result"]["answer"]["body"]["outcome"]["t_us"], 50_000, "{result}");
        let status = m.call_tool("status", json!({"launch_id":g.id})).await;
        assert_eq!(status["clock"]["t_us"], 50_000, "{status}");
        assert_eq!(status["lease_held"], false, "{status}");
    }
    let runtime_dir = ws.root().join("runtimes").join(&g.id);
    let record = frontcap_recorder::runtime::load_runtime(&runtime_dir).unwrap().unwrap();
    assert!(record.recorder.kill().unwrap());
    let saved_runtime = std::fs::read(runtime_dir.join("runtime.json")).unwrap();
    let saved_journal = std::fs::read(runtime_dir.join("journal.jsonl")).unwrap();
    let dead = m.call_tool("session", json!({"launch_id":g.id,"operation":"result","operation_id":"result-advance"})).await;
    assert_eq!(dead["result"]["answer"]["body"]["outcome"]["t_us"], 50_000, "{dead}");
    assert!(!record.recorder.is_alive());
    assert_eq!(std::fs::read(runtime_dir.join("runtime.json")).unwrap(), saved_runtime);
    assert_eq!(std::fs::read(runtime_dir.join("journal.jsonl")).unwrap(), saved_journal);
    m.call_tool("stop", json!({"launch_id":g.id})).await;
    let stopped_runtime = std::fs::read(runtime_dir.join("runtime.json")).unwrap();
    let stopped = m.call_tool("session", json!({"launch_id":g.id,"operation":"result","operation_id":"result-advance"})).await;
    assert_eq!(stopped, dead);
    assert_eq!(std::fs::read(runtime_dir.join("runtime.json")).unwrap(), stopped_runtime);

}

#[tokio::test]
async fn dialog_rules_accept_hold_and_replace_it_with_automatic_answers() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let g = launch(&mut m, "launch-dialog-policy", format!("http://{addr}/toast.html"), json!([])).await;
    for answer in ["hold", "dismiss"] {
        let accepted = m.call_tool("session", json!({"launch_id":g.id,"operation":"dialog_rules","rules":[{"answer":answer}]})).await;
        assert_eq!(accepted["ok"], true);
    }
    m.call_tool("stop", json!({"launch_id":g.id})).await;
}

#[tokio::test]
async fn a_quiescent_settle_names_the_pages_next_timer() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let origin = "https://debounce.example.test";
    let body = r#"<!doctype html><input id="q"><output id="s"></output><script>
let t;document.querySelector('#q').oninput=()=>{clearTimeout(t);t=setTimeout(()=>document.querySelector('#s').textContent='saved',500)};
</script>"#;
    let rules = json!([{"id":"app","match":{"url":format!("{origin}/")},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":body}}]);
    let launched = m.call_tool("launch", json!({"url":format!("{origin}/"),"rules":rules})).await;
    assert!(launched["settle"].get("next_timer_t").is_none(), "no timer yet: {launched}");
    let id = &launched["launch_id"];
    let typed = m.call_tool("act", json!({"launch_id":id,"action":{"type":"type","target":{"by":"css","selector":"#q"},"text":"a"}})).await;
    assert_eq!(typed["settle"]["type"], "quiescent", "{typed}");
    // The debounce was set while typing and is still pending after the 250 ms quiet window.
    let due = typed["settle"]["next_timer_t"].as_u64().expect("a pending timer");
    let (before, after) = (typed["t_before_us"].as_u64().unwrap(), typed["t_after_us"].as_u64().unwrap());
    assert!(due >= before + 500_000 && due > after, "{typed}");
    let waited = m.call_tool("wait", json!({"launch_id":id,"op":{"type":"settle","idle_window_ms":250,"max_ms":2000}})).await;
    assert!(waited["outcome"].get("next_timer_t").is_none(), "{waited}");
    let saved = m.call_tool("observe", json!({"launch_id":id,"op":{"type":"query","target":{"by":"css","selector":"#s"},"props":["text"]}})).await;
    assert_eq!(saved["values"]["text"], "saved");
    m.call_tool("stop", json!({"launch_id":id})).await;
}

/// A predicate reads a form control's value as the user left it.
#[tokio::test]
async fn a_value_predicate_reads_a_form_controls_value() {
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let url = "https://value.example.test/";
    let launched = m.call_tool("launch", json!({"url": url, "rules": [{"id": "page", "match": {"url": url},
        "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": "<!doctype html><input id=title value=Draft>"}}]})).await;
    let id = &launched["launch_id"];
    let value = |text: &str| json!({"is": "value_equals", "target": {"by": "css", "selector": "#title"}, "value": text});
    let first = m.call_tool("watch", json!({"launch_id": id, "operation": "add", "name": "draft", "predicate": value("Draft")})).await;
    assert_eq!(first["value"], "true", "{first}");
    m.call_tool("act", json!({"launch_id": id, "action": {"type": "fill", "target": {"by": "css", "selector": "#title"}, "value": "Q3 plan"}})).await;
    let typed = m.call_tool("watch", json!({"launch_id": id, "operation": "add", "name": "typed", "predicate": value("Q3 plan")})).await;
    assert_eq!(typed["value"], "true", "{typed}");
    let last = m.call_tool("watch", json!({"launch_id": id, "operation": "remove", "name": "draft"})).await;
    assert_eq!(last["value"], "false", "{last}");
    m.call_tool("stop", json!({"launch_id": id})).await;
}
