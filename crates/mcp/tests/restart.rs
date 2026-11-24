mod support;

use serde_json::{Value, json};
use std::time::Duration;

/// A replay cassette with two answers to the same request, kept for the run in the workspace.
fn cassette(ws: &support::Workspace) -> String {
    let line = |body: &str| {
        frontcap_core::cassette::Line {
            requires_progress: false,
            key: frontcap_core::cassette::Key::of("GET", "https://api.example.test/cas", None),
            response: frontcap_core::cassette::Response {
                status: 200,
                headers: [("access-control-allow-origin".to_string(), "*".to_string())].into(),
                body_base64: frontcap_core::encode::base64(body.as_bytes()),
            },
            latency: frontcap_core::cassette::Latency { virtual_ms: 10, wall_ms: 1 },
        }
        .to_line()
    };
    let path = ws.tmp.path().join("cas.jsonl");
    std::fs::write(&path, format!("{}\n{}\n", line("first cas"), line("second cas"))).unwrap();
    path.display().to_string()
}

fn restart_rules() -> Value {
    json!([
      {"id": "seq", "match": {"url": "https://api.example.test/seq"},
       "action": {"type": "sequence", "after_last": "fail", "steps": [
         {"status": 200, "headers": {"access-control-allow-origin": "*"}, "body": "first"},
         {"status": 200, "headers": {"access-control-allow-origin": "*"}, "body": "second"}]}},
      {"id": "held", "match": {"url": "https://api.example.test/held"}, "action": {"type": "hold"}},
      {"id": "hold", "match": {"url": "https://api.example.test/hold"}, "action": {"type": "hold"}},
      {"id": "cas", "match": {"url": "https://api.example.test/cas"}, "action": {"type": "mock", "status": 200, "headers": {"access-control-allow-origin": "*"}, "body": "mocked"}}])
}

/// `restart_rules` with the `cas` request answered from `cassette`.
fn replay_rules(cassette: &str) -> Value {
    let mut rules = restart_rules();
    rules[3]["action"] = json!({"type": "replay", "cassette": cassette});
    rules
}

async fn launch(m: &mut support::Mcp, page: &str, addr: std::net::SocketAddr) -> (String, String) {
    launch_with(m, page, addr, restart_rules()).await
}

async fn launch_with(m: &mut support::Mcp, page: &str, addr: std::net::SocketAddr, rules: Value) -> (String, String) {
    let r = m.call_tool("launch", json!({"request_id": "l-1", "url": format!("http://{addr}/{page}"), "rules": rules, "then": {"type": "none"}})).await;
    (r["launch_id"].as_str().unwrap().into(), r["lease"].as_str().unwrap().into())
}

/// Reattaches after the killed controller's lease and owned-input cleanup have ended.
async fn reattach(m: &mut support::Mcp, id: &str) -> Value {
    for _ in 0..250 {
        let (is_error, r) = m.try_tool("session", json!({"operation": "reattach", "launch_id": id, "request_id": "re-1"})).await;
        if !is_error {
            return r;
        }
        match r["code"].as_str() {
            Some("lease_held") => {},
            Some("busy") => assert_eq!(r["message"], "held input cleanup is pending", "{r}"),
            _ => panic!("unexpected reattach failure: {r}"),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the killed controller's lease or input cleanup never ended");
}

#[tokio::test]
async fn mcp_restart_keeps_holds_sequences_watches() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m1 = support::Mcp::start_in(&ws.root()).await;
    // Criterion 6: a replay cassette half consumed, as well as a sequence.
    let (id, lease) = launch_with(&mut m1, "restart.html", addr, replay_rules(&cassette(&ws))).await;
    m1.call_tool("watch", json!({"launch_id": id, "lease": lease, "request_id": "w-1",
        "operation": "add", "name": "seq2", "predicate": {"is": "text_equals", "target": {"by": "css", "selector": "#seq2"}, "text": "second"}})).await;
    m1.call_tool("wait", json!({"launch_id": id, "lease": lease, "request_id": "c-1", "op": {"type": "advance", "by_ms": 500}})).await;
    m1.kill().await;

    let mut m2 = support::Mcp::start_in(&ws.root()).await;
    let r = reattach(&mut m2, &id).await;
    let lease = r["lease"].as_str().unwrap().to_string();
    let held: Vec<Value> = r["status"]["held_requests"].as_array().unwrap().iter().map(|h| h["ordinal"].clone()).collect();
    assert_eq!(held.len(), 1, "{r}");
    assert_eq!(r["status"]["watches"], json!(["seq2"]));
    m2.call_tool("network", json!({"launch_id": id, "lease": lease, "request_id": "n-1",
        "operation": "respond", "ordinal": held[0], "response": {"status": 200, "headers": {"access-control-allow-origin": "*"}, "body": "answered"}})).await;
    let until = m2.call_tool("wait", json!({"launch_id": id, "lease": lease, "request_id": "c-2", "op": {"type": "until", "watch": "seq2", "max_ms": 5000}})).await;
    assert_eq!(until["outcome"]["type"], "condition_met", "{until}");
    let t = until["t_us"].as_u64().unwrap();
    assert!((3_000_000..3_100_000).contains(&t), "the second sequence step after the 3000 ms timer: {t}");
    m2.call_tool("wait", json!({"launch_id": id, "lease": lease, "request_id": "c-3", "op": {"type": "advance", "by_ms": 100}})).await;
    let cas = m2.call_tool("debug", json!({"launch_id": id, "lease": lease, "request_id": "e-1", "operation": "evaluate",
        "expression": "[cas1.textContent, cas2.textContent]"})).await;
    assert_eq!(cas["value"], json!(["first cas", "second cas"]), "the cassette's second answer after the restart");
    let stopped = m2.call_tool("stop", json!({"launch_id": id, "lease": lease, "request_id": "s-1"})).await;
    let trace = std::fs::read_to_string(format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap())).unwrap();
    let events: Vec<Value> = trace.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(events.windows(2).all(|w| w[1]["seq"].as_u64() == w[0]["seq"].as_u64().map(|s| s + 1)));
    let ordinals: Vec<&str> = events.iter().filter(|e| e["kind"] == "request").map(|e| e["payload"]["id"].as_str().unwrap()).collect();
    let unique: std::collections::BTreeSet<_> = ordinals.iter().collect();
    assert_eq!(unique.len(), ordinals.len());
}

async fn holding_page(m: &mut support::Mcp, addr: std::net::SocketAddr) -> (String, String) {
    let (id, lease) = launch(m, "hold.html", addr).await;
    m.call_tool("watch", json!({"launch_id": id, "lease": lease, "request_id": "w-1",
        "operation": "add", "name": "never", "predicate": {"is": "attached", "target": {"by": "css", "selector": ".never"}}})).await;
    m.call_tool("wait", json!({"launch_id": id, "lease": lease, "request_id": "c-0", "op": {"type": "advance", "by_ms": 50}})).await;
    (id, lease)
}

#[tokio::test]
async fn mcp_killed_during_until() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m1 = support::Mcp::start_in(&ws.root()).await;
    let (id, lease) = holding_page(&mut m1, addr).await;
    // A page that works every millisecond: idle, ten seconds would pass before the controller is lost.
    m1.call_tool("debug", json!({"launch_id": id, "operation": "evaluate", "expression": "setInterval(() => {}, 1);true"})).await;
    let start = m1.call_tool("status", json!({"launch_id": id})).await["t_us"].as_u64().unwrap();
    let goal = start + 10_000_000;
    let until = json!({"launch_id": id, "lease": lease, "request_id": "c-1", "op": {"type": "until", "watch": "never", "max_ms": 10000}});
    let _pending = m1.send("tools/call", json!({"name": "wait", "arguments": until})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let active = m1.call_tool("status", json!({"launch_id": id})).await;
    assert_eq!(active["running"], "c-1", "kill must interrupt the wait: {active}");
    assert!(active["running_t_us"].as_u64().unwrap() < goal, "{active}");
    m1.kill().await;

    let mut m2 = support::Mcp::start_in(&ws.root()).await;
    // Completion is a virtual-time contract, not a ratio to host time or a
    // number of status calls. Bound both a lack of progress and total test time.
    let idle = tokio::time::timeout(Duration::from_secs(60), async {
        let mut reached = active["running_t_us"].as_u64().unwrap();
        let mut progressed = tokio::time::Instant::now();
        loop {
            let status = m2.call_tool("status", json!({"launch_id": id})).await;
            assert_eq!(status["faults"], json!([]), "{status}");
            if status["running"].is_null() { break status; }
            assert_eq!(status["running"], "c-1", "{status}");
            let now = status["running_t_us"].as_u64().unwrap();
            assert!((reached..=goal).contains(&now), "detached progress left its budget: {status}");
            if now > reached {
                reached = now;
                progressed = tokio::time::Instant::now();
            }
            assert!(progressed.elapsed() < Duration::from_secs(10), "detached wait stopped progressing: {status}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.expect("detached wait exceeded the 60-second test observation limit");
    assert_eq!(idle["last_completed_request_id"], "c-1", "the request ran to completion without its controller");
    let t = idle["t_us"].as_u64().unwrap();
    assert_eq!(t, goal, "the detached wait consumes exactly its original virtual budget");
    let r = reattach(&mut m2, &id).await;
    assert_eq!(r["completed_while_detached"], json!(["c-1"]), "{r}");
    let lease2 = r["lease"].as_str().unwrap().to_string();
    let mut again = until.clone();
    again["lease"] = json!(lease2);
    let repeated = m2.call_tool("wait", again).await;
    assert_eq!(repeated["outcome"]["type"], "condition_timed_out", "{repeated}");
    assert_eq!(repeated["t_us"].as_u64().unwrap(), t, "answered from the journal, not run again");
    m2.call_tool("stop", json!({"launch_id": id, "lease": lease2, "request_id": "s-1"})).await;
}

/// A controller lost during a long wait leaves it running, and its cleanup waits behind it. The next
/// controller cancels the wait without a lease, which no one holds, then takes the lease and stops.
#[tokio::test]
async fn the_next_controller_cancels_a_lost_controllers_long_wait() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m1 = support::Mcp::start_in(&ws.root()).await;
    let (id, lease) = launch(&mut m1, "restart.html", addr).await;
    // A page that works every millisecond: an idle month would pass before the controller is lost.
    m1.call_tool("debug", json!({"launch_id": id, "operation": "evaluate", "expression": "setInterval(() => {}, 1);true"})).await;
    let month = json!({"launch_id": id, "lease": lease, "request_id": "month", "op": {"type": "advance", "by_ms": 30u64 * 24 * 3600 * 1000}});
    let _pending = m1.send("tools/call", json!({"name": "wait", "arguments": month})).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    m1.kill().await;

    let mut m2 = support::Mcp::start_in(&ws.root()).await;
    let mut status = m2.call_tool("status", json!({"launch_id": id})).await;
    for _ in 0..250 {
        if status["lease_held"] == false { break; }
        tokio::time::sleep(Duration::from_millis(20)).await;
        status = m2.call_tool("status", json!({"launch_id": id})).await;
    }
    assert_eq!((&status["running"], &status["lease_held"]), (&json!("month"), &json!(false)), "{status}");
    // The running wait reports how far it has come; the committed clock is where it started.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let moving = m2.call_tool("status", json!({"launch_id": id})).await;
    assert!(moving["running_t_us"].as_u64().unwrap() > moving["t_us"].as_u64().unwrap(), "{moving}");
    let (refused, busy) = m2.try_tool("stop", json!({"launch_id": id, "request_id": "s-1"})).await;
    assert!(refused && busy["code"] == "busy" && busy["recovery"].as_str().unwrap().contains("session cancel"), "{busy}");
    m2.call_tool("session", json!({"launch_id": id, "operation": "cancel", "operation_id": "month"})).await;
    let lease2 = reattach(&mut m2, &id).await["lease"].as_str().unwrap().to_string();
    let result = m2.call_tool("session", json!({"launch_id": id, "operation": "result", "operation_id": "month"})).await;
    assert_eq!(result["result"]["answer"]["body"]["outcome"]["result"]["type"], "cancelled", "{result}");
    let stopped = m2.call_tool("stop", json!({"launch_id": id, "lease": lease2, "request_id": "s-2"})).await;
    assert_eq!(stopped["finalized"], true, "{stopped}");
}

#[tokio::test]
async fn old_lease_is_refused_after_reattach() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m1 = support::Mcp::start_in(&ws.root()).await;
    let (id, old) = holding_page(&mut m1, addr).await;
    m1.kill().await;
    let mut m2 = support::Mcp::start_in(&ws.root()).await;
    let r = reattach(&mut m2, &id).await;
    let (is_error, e) = m2.try_tool("wait", json!({"launch_id": id, "lease": old, "request_id": "c-9", "op": {"type": "advance", "by_ms": 1}})).await;
    assert!(is_error);
    assert_eq!(e["code"], "lease_required", "{e}");
    m2.call_tool("stop", json!({"launch_id": id, "lease": r["lease"], "request_id": "s-1"})).await;
}

#[tokio::test]
async fn mcp_killed_during_act_clicks_and_settles_once() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m1 = support::Mcp::start_in(&ws.root()).await;
    let (id, lease) = launch(&mut m1, "count.html", addr).await;
    m1.call_tool("wait", json!({"launch_id": id, "lease": lease, "request_id": "c-0", "op": {"type": "advance", "by_ms": 50}})).await;
    let act = json!({"launch_id": id, "lease": lease, "request_id": "a-1",
        "action": {"type": "click", "target": {"by": "css", "selector": "#ok"}},
        "then": {"type": "settle", "idle_window_ms": 250, "max_ms": 2000}});
    let _pending = m1.send("tools/call", json!({"name": "act", "arguments": act})).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    m1.kill().await;

    let mut m2 = support::Mcp::start_in(&ws.root()).await;
    let r = reattach(&mut m2, &id).await;
    let lease2 = r["lease"].as_str().unwrap().to_string();
    let mut again = act.clone();
    again["lease"] = json!(lease2);
    let mut answered = None;
    for _ in 0..250 {
        let (is_error, v) = m2.try_tool("act", again.clone()).await;
        if !is_error {
            answered = Some(v);
            break;
        }
        assert_eq!(v["code"], "busy", "{v}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let answered = answered.expect("the repeated act answered");
    assert_eq!(answered["settle"]["type"], "quiescent", "{answered}");
    let once = m2.call_tool("watch", json!({"launch_id": id, "lease": lease2, "request_id": "w-9",
        "operation": "add", "name": "once", "predicate": {"is": "text_equals", "target": {"by": "css", "selector": "#count"}, "text": "1"}})).await;
    assert_eq!(once["value"], "true", "clicked exactly once");
    let stopped = m2.call_tool("stop", json!({"launch_id": id, "lease": lease2, "request_id": "s-1"})).await;
    let trace = std::fs::read_to_string(format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap())).unwrap();
    let settles = trace.lines().filter(|l| l.contains(r#""op":"settle""#) && l.contains(r#""phase":"start""#)).count();
    assert_eq!(settles, 1, "the settle ran once");
}

#[tokio::test]
async fn a_repeated_act_returns_the_recorded_effects() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m1 = support::Mcp::start_in(&ws.root()).await;
    let (id, lease) = launch(&mut m1, "count.html", addr).await;
    m1.call_tool("wait", json!({"launch_id": id, "lease": lease, "request_id": "c-0", "op": {"type": "advance", "by_ms": 50}})).await;
    let act = json!({"launch_id": id, "lease": lease, "request_id": "a-1",
        "action": {"type": "click", "target": {"by": "css", "selector": "#ok"}},
        "then": {"type": "settle", "idle_window_ms": 250, "max_ms": 2000}});
    let first = m1.call_tool("act", act.clone()).await;
    assert_eq!(first["t_before_us"], 50_000, "{first}");
    assert!(first["effects"]["dom_mutations"].as_u64().unwrap() >= 1, "{first}");
    // The controller dies after the act completed; its answer is lost. A later step moves time on.
    m1.call_tool("wait", json!({"launch_id": id, "lease": lease, "request_id": "c-1", "op": {"type": "advance", "by_ms": 1000}})).await;
    m1.kill().await;
    let mut m2 = support::Mcp::start_in(&ws.root()).await;
    let r = reattach(&mut m2, &id).await;
    let lease2 = r["lease"].as_str().unwrap().to_string();
    let mut again = act.clone();
    again["lease"] = json!(lease2);
    let repeated = m2.call_tool("act", again).await;
    assert_eq!(repeated, first, "the repeated act returns the recorded result");
    m2.call_tool("stop", json!({"launch_id": id, "lease": lease2, "request_id": "s-1"})).await;
}

#[tokio::test]
async fn a_repeated_act_is_not_rewritten_by_later_events_and_changed_arguments_conflict() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let rules = json!([{"id": "held", "match": {"url": "https://cdn.example.test/x.js"}, "action": {"type": "hold"}}]);
    let r = m.call_tool("launch", json!({"request_id": "l-1", "url": format!("http://{addr}/blocked.html"), "rules": rules, "then": {"type": "none"}})).await;
    let (id, lease) = (r["launch_id"].clone(), r["lease"].clone());
    m.call_tool("wait", json!({"launch_id": id, "lease": lease, "request_id": "c-0", "op": {"type": "advance", "by_ms": 50}})).await;
    let act = json!({"launch_id": id, "lease": lease, "request_id": "a-1", "action": {"type": "click", "target": {"by": "css", "selector": "#go"}},
        "then": {"type": "settle", "idle_window_ms": 10, "max_ms": 50}});
    let first = m.call_tool("act", act.clone()).await;
    let held = m.call_tool("network", json!({"launch_id": id, "operation": "list_held"})).await;
    m.call_tool("network", json!({"launch_id": id, "lease": lease, "request_id": "r-1",
        "operation": "respond", "ordinal": held["held"][0]["ordinal"], "response": {"status": 200, "headers": {"access-control-allow-origin": "*"}, "body": "ok"}})).await;
    let again = m.call_tool("act", act.clone()).await;
    assert_eq!(again, first, "a later response does not rewrite the act");
    let mut changed = act;
    changed["then"] = json!({"type": "none"});
    let (is_error, e) = m.try_tool("act", changed).await;
    assert!(is_error);
    assert_eq!(e["code"], "request_id_conflict", "{e}");
    m.call_tool("stop", json!({"launch_id": id, "lease": lease, "request_id": "s-1"})).await;
}

/// A server that does not hold the lease cannot act without one: the call names the way to take it.
#[tokio::test]
async fn a_new_server_without_the_lease_is_told_to_reattach() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m1 = support::Mcp::start_in(&ws.root()).await;
    let (id, _) = launch(&mut m1, "restart.html", addr).await;
    m1.kill().await;
    let mut m2 = support::Mcp::start_in(&ws.root()).await;
    let (is_error, e) = m2.try_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 1}})).await;
    assert!(is_error && e["code"] == "lease_required", "{e}");
    assert!(e["recovery"].as_str().unwrap().contains("reattach"), "{e}");
}

/// A generation whose recorder and browser both ended is listed as lost, and every route to it says
/// how to seal its evidence.
#[tokio::test]
async fn a_generation_whose_recorder_and_browser_ended_is_lost() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut a = support::Mcp::start_in(&ws.root()).await;
    let (id, _) = launch(&mut a, "restart.html", addr).await;
    let runtime = frontcap_recorder::runtime::load_runtime(&ws.root().join("runtimes").join(&id)).unwrap().unwrap();
    let browser = runtime.browser.clone().expect("a browser");
    for pid in [runtime.recorder.pid, browser.pid] {
        std::process::Command::new("kill").arg("-9").arg(pid.to_string()).status().unwrap();
    }
    while runtime.recorder.is_alive() || browser.is_alive() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut b = support::Mcp::start_in(&ws.root()).await;
    let listed = b.call_tool("session", json!({"operation":"list"})).await;
    let row = &listed["generations"][0];
    assert_eq!(row["state"], "lost", "{row}");
    assert!(row["next_action"].as_str().unwrap().contains("debug finalize"), "{row}");
    let (failed, gone) = b.try_tool("session", json!({"operation":"reattach","launch_id":id})).await;
    assert!(failed && gone["code"] == "recorder_fault" && gone["recovery"].as_str().unwrap().contains("debug finalize"), "{gone}");
    let finalized = b.call_tool("debug", json!({"launch_id":id,"operation":"finalize"})).await;
    assert_eq!(finalized["finalization"]["state"], "finalized", "{finalized}");
}

/// A lease this server holds can go stale: the recorder died and another server reattached. The next
/// call is refused once, the stale lease is dropped, and `session reattach` then reports who holds the
/// lease instead of handing the stale one back.
#[tokio::test]
async fn a_stale_lease_is_dropped_after_its_refusal() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut a = support::Mcp::start_in(&ws.root()).await;
    let (id, _) = launch(&mut a, "restart.html", addr).await;
    let runtime = frontcap_recorder::runtime::load_runtime(&ws.root().join("runtimes").join(&id)).unwrap().unwrap();
    std::process::Command::new("kill").arg("-9").arg(runtime.recorder.pid.to_string()).status().unwrap();
    // A crashed recorder is gone before anyone reattaches.
    while runtime.recorder.is_alive() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut b = support::Mcp::start_in(&ws.root()).await;
    let listed = b.call_tool("session", json!({"operation":"list"})).await;
    assert_eq!(listed["generations"][0]["state"], "recorder_unavailable");
    assert_eq!(listed["generations"][0]["browser_alive"], true);
    assert!(!runtime.recorder.is_alive(), "listing does not recover the recorder");
    let r = reattach(&mut b, &id).await;
    assert!(r["lease"].is_string(), "{r}");
    let stale = a.call_tool("status", json!({"launch_id":id})).await;
    assert_eq!(stale["ownership"], "other", "a cached old token is not authority");
    let (is_error, e) = a.try_tool("session", json!({"operation":"reattach", "launch_id":id})).await;
    assert!(is_error && e["code"] == "lease_held", "reattach must not return a stale token: {e}");
    let (is_error, e) = a.try_tool("wait", json!({"launch_id": id, "op": {"type": "advance", "by_ms": 1}})).await;
    assert!(is_error && e["code"] == "lease_required" && e["recovery"].as_str().unwrap().contains("session reattach"), "{e}");
    let (is_error, e) = a.try_tool("session", json!({"operation": "reattach", "launch_id": id})).await;
    assert!(is_error && e["code"] == "lease_held", "the stale lease is not handed back: {e}");
}
