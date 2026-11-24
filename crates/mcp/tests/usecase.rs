//! An agent's test of a small shop app through the server: sign-in with validation and a spinner, a routed
//! product list, a debounced search, add to cart with a toast, a confirm dialog, and an order that fails
//! once and is retried. Every run must behave, time, and look the same.
mod support;

use serde_json::{Value, json};
use std::time::Instant;

fn shop_rules() -> Value {
    let api = |path: &str| format!("https://api.shop.test{path}");
    let json_mock = |body: &str, latency: u64| json!({"type": "mock", "status": 200, "headers": {"content-type": "application/json"}, "body": body, "latency_ms": latency});
    json!([
        {"id": "login", "match": {"method": "POST", "url": api("/login")}, "action": json_mock(r#"{"token":"t1"}"#, 300)},
        {"id": "search", "match": {"url": api("/products?q=ch")}, "action": json_mock(r#"[{"name":"Chair","price":"$50"}]"#, 100)},
        {"id": "products", "match": {"url": api("/products")}, "action": json_mock(r#"[{"name":"Chair","price":"$50"},{"name":"Lamp","price":"$20"},{"name":"Desk","price":"$120"}]"#, 200)},
        {"id": "order", "match": {"method": "POST", "url": api("/order")}, "action": {"type": "sequence", "after_last": "repeat_last", "steps": [
            {"status": 500, "body": "", "latency_ms": 100},
            {"status": 200, "headers": {"content-type": "application/json"}, "body": "{}", "latency_ms": 100}]}}
    ])
}

/// The first step at which two runs' logs differ.
fn first_difference(a: &[String], b: &[String]) -> Option<(usize, Option<String>, Option<String>)> {
    (0..a.len().max(b.len())).find(|&i| a.get(i) != b.get(i)).map(|i| (i, a.get(i).cloned(), b.get(i).cloned()))
}

/// Require the current controlled engine or verify explicit baseline evidence.
async fn controlled_engine_or_check_baseline(m: &mut support::Mcp, addr: std::net::SocketAddr) -> bool {
    let root = support::repo_root();
    if browser_pin::engine::patched_binary(&root, browser_pin::current_platform().unwrap()).unwrap().is_some() {
        return true;
    }
    assert!(std::env::var("FRONTCAP_ALLOW_BASELINE").is_ok_and(|v| v == "1"), "no patched engine: `browser-pin engine build`, or FRONTCAP_ALLOW_BASELINE=1");
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/toast.html")})).await;
    let stopped = m.call_tool("stop", json!({"launch_id": r["launch_id"]})).await;
    let trace = std::fs::read_to_string(format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap())).unwrap();
    let missing = browser_pin::engine::patches(&root).unwrap().into_iter()
        .map(|p| p.file_stem().unwrap().to_string_lossy().into_owned())
        .collect::<Vec<_>>().join(", ");
    assert!(!missing.is_empty(), "the test requires controlled engine changes");
    assert!(trace.contains("\"engine_baseline\"") &&
        trace.contains(&format!("the baseline lacks {missing};")), "{trace}");
    false
}

fn css(selector: &str) -> Value {
    json!({"by": "css", "selector": selector})
}

fn role(role: &str, name: &str) -> Value {
    json!({"by": "role", "role": role, "name": name})
}

/// One run: every step's outcome as a line, the screenshots' bytes, and the trace path. `pause_ms` of wall
/// time passes before each step.
async fn shop_run(m: &mut support::Mcp, addr: std::net::SocketAddr, n: usize, pause_ms: u64) -> (Vec<String>, Vec<Vec<u8>>, String) {
    let mut log = Vec::new();
    let mut shots = Vec::new();
    let wall = Instant::now();
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/shop/index.html"), "rules": shop_rules()})).await;
    let id = r["launch_id"].as_str().unwrap().to_string();
    log.push(format!("launch t={} settle={}", r["clock"]["t_us"], r["settle"]["type"]));
    macro_rules! step {
        ($name:expr, $tool:expr, $args:expr) => {{
            tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
            let started = Instant::now();
            let mut args: Value = $args;
            args["launch_id"] = json!(id);
            let (err, v) = m.try_tool($tool, args).await;
            let line = format!("{} err={} t_before={} t_after={} t={} outcome={} settle={} effects={} wall_ms={}", $name, err,
                v["t_before_us"], v["t_after_us"], v["t_us"], v["outcome"], v["settle"]["type"],
                v.get("effects").map(|e| json!({"requests": e["requests"], "unmatched": e["unmatched_requests"], "exceptions": e["exceptions"]})).unwrap_or(Value::Null),
                started.elapsed().as_millis());
            eprintln!("run {n}: {line}");
            if err { eprintln!("  -> {v}"); }
            log.push(format!("{} err={} {} {} {} {} {}", $name, err, v["t_before_us"], v["t_after_us"], v["t_us"], v["outcome"]["type"], v["effects"]["requests"]));
            v
        }};
    }
    macro_rules! shot {
        ($name:expr) => {{
            let v = step!($name, "capture", json!({"op": {"type": "screenshot"}}));
            if let Some(p) = v["path"].as_str() {
                shots.push(std::fs::read(p).unwrap());
            }
            log.push(format!("{} t={} exact={} current={}", $name, v["t_us"], v["exact"], v["current"]));
        }};
    }
    // Validation: signing in with no email.
    step!("click sign-in empty", "act", json!({"action": {"type": "click", "target": role("button", "Sign in")}}));
    let q = step!("query error", "observe", json!({"op": {"type": "query", "target": css("#error"), "props": ["text", "visible"]}}));
    // What the page shows; the reference names the generation that issued it, so runs differ there.
    log.push(format!("error shown: {}", q["values"]));
    step!("fill email", "act", json!({"action": {"type": "fill", "target": role("textbox", "Email"), "value": "a@b.c"}}));
    step!("fill password", "act", json!({"action": {"type": "fill", "target": css("#password"), "value": "pw"}}));
    step!("click sign-in", "act", json!({"action": {"type": "click", "target": role("button", "Sign in")}, "then": {"type": "none"}}));
    step!("advance 120", "wait", json!({"op": {"type": "advance", "by_ms": 120}}));
    shot!("shot spinner");
    step!("until products", "wait", json!({"op": {"type": "until", "predicate": {"is": "count_at_least", "target": css(".product"), "n": 3}, "max_ms": 3000}}));
    shot!("shot products");
    step!("type search", "act", json!({"action": {"type": "type", "target": role("textbox", "Search"), "text": "ch"}}));
    step!("until filtered", "wait", json!({"op": {"type": "until", "predicate": {"is": "text_equals", "target": css("#list .product:only-child .name"), "text": "Chair"}, "max_ms": 2000}}));
    step!("add chair", "act", json!({"action": {"type": "click", "target": role("button", "Add Chair")}}));
    step!("until toast", "wait", json!({"op": {"type": "until", "predicate": {"is": "opacity_at_least", "target": css(".toast"), "milli": 1000}, "max_ms": 1000}}));
    shot!("shot toast");
    step!("checkout", "act", json!({"action": {"type": "click", "target": role("button", "Checkout")}}));
    step!("place order", "act", json!({"action": {"type": "click", "target": role("button", "Place order")}}));
    step!("until error", "wait", json!({"op": {"type": "until", "predicate": {"is": "visible", "target": css("#order-error")}, "max_ms": 1000}}));
    step!("retry", "act", json!({"action": {"type": "click", "target": role("button", "Retry")}}));
    step!("until ok", "wait", json!({"op": {"type": "until", "predicate": {"is": "visible", "target": css("#order-ok")}, "max_ms": 1000}}));
    shot!("shot done");
    let snap = step!("snapshot", "observe", json!({"op": {"type": "snapshot"}}));
    log.push(format!("snapshot nodes={}", snap["nodes"].as_array().map(|a| a.len()).unwrap_or(0)));
    let stopped = m.call_tool("stop", json!({"launch_id": id})).await;
    eprintln!("run {n}: whole run {} ms", wall.elapsed().as_millis());
    (log, shots, format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap()))
}

#[tokio::test]
async fn a_shop_test_runs_the_same_every_time() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    if !controlled_engine_or_check_baseline(&mut m, addr).await {
        return;
    }
    let mut runs = Vec::new();
    // Four runs back to back, and one with wall time between every step.
    for n in 0..5 {
        runs.push(shop_run(&mut m, addr, n, if n == 4 { 300 } else { 0 }).await);
    }
    let (log, shots, trace) = &runs[0];
    assert!(log.iter().all(|l| !l.contains("err=true")), "{log:#?}");
    // Login starts at 1250ms and answers after 300ms. Its hashchange handler requests the
    // products at that same instant; the 200ms response needs no extra frame-tick delay.
    assert!(log.iter().any(|l| l.starts_with("until products err=false null null 1750000 \"condition_met\"")), "{log:#?}");
    let events = frontcap_core::log::BehaviorLog::read(std::path::Path::new(trace)).unwrap();
    let delivery: Vec<_> = events.iter().filter_map(|e| {
        use frontcap_core::event::EventKind;
        let (kind, rule) = match &e.kind {
            EventKind::Request(r) => ("request", r.rule.as_ref()?),
            EventKind::Response(r) => ("response", r.rule.as_ref()?),
            _ => return None,
        };
        matches!(rule.as_str(), "login" | "products").then_some((kind, rule.as_str(), e.t_us.0))
    }).collect();
    assert_eq!(delivery, [
        ("request", "login", 1_250_000), ("response", "login", 1_550_000),
        ("request", "products", 1_550_000), ("response", "products", 1_750_000),
    ], "the chained request retains its own issue and response deadline");
    // The spinner (an animated GIF and a CSS animation) as presented at the frame tick before 1370 ms; not
    // current, since both still want frames.
    assert!(log.iter().any(|l| l == "shot spinner t=1367000 exact=true current=false"), "{log:#?}");
    assert_eq!(shots.len(), 4);
    let first = std::fs::read_to_string(trace).unwrap();
    for (n, (other_log, other_shots, other_trace)) in runs.iter().enumerate().skip(1) {
        assert_eq!(first_difference(log, other_log), None, "run {n}: every step ends at the same instant with the same outcome");
        assert!(other_shots == shots, "run {n}: the same screenshots");
        let text = std::fs::read_to_string(other_trace).unwrap();
        assert_eq!(frontcap_core::projection::compare_behavior(&first, &text), Ok(()), "run {n}: the same behavior");
    }
}

fn dash_rules() -> Value {
    let api = |path: &str| format!("https://api.dash.test{path}");
    let json_mock = |body: String, latency: u64| json!({"type": "mock", "status": 200, "headers": {"content-type": "application/json"}, "body": body, "latency_ms": latency});
    let days = |n: usize, scale: u64| {
        let names = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        let v: Vec<Value> = (0..n).map(|i| json!({"day": names[i], "n": (i as u64 * 7 + 3) * scale})).collect();
        Value::Array(v).to_string()
    };
    let items = |page: usize| Value::Array((0..10).map(|i| json!(format!("event {}", page * 10 + i))).collect()).to_string();
    json!([
        {"id": "stats-7", "match": {"url": api("/stats?range=7d")}, "action": json_mock(days(5, 1), 150)},
        {"id": "stats-30", "match": {"url": api("/stats?range=30d")}, "action": json_mock(days(7, 4), 150)},
        {"id": "stats-30-err", "match": {"url": api("/stats?range=30d&errors=1")}, "action": json_mock(days(3, 1), 150)},
        {"id": "items-1", "match": {"url": api("/items?page=1")}, "action": json_mock(items(0), 80)},
        {"id": "items-2", "match": {"url": api("/items?page=2")}, "action": json_mock(items(1), 80)},
        {"id": "items-3", "match": {"url": api("/items?page=3")}, "action": json_mock(items(2), 80)},
        {"id": "live", "match": {"url": "wss://api.dash.test/live"}, "action": {"type": "stream_mock", "messages": [{"at_ms": 500, "data": "12"}, {"at_ms": 1500, "data": "15"}]}},
        {"id": "billing", "match": {"url": "https://billing.dash.test/widget.html"}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "text/html"}, "body": include_str!("fixtures/dash/widget.html")}}
    ])
}

/// One dashboard run, as `shop_run`.
async fn dash_run(m: &mut support::Mcp, addr: std::net::SocketAddr, n: usize, pause_ms: u64) -> (Vec<String>, Vec<Vec<u8>>, String) {
    let mut log = Vec::new();
    let mut shots = Vec::new();
    let wall = Instant::now();
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/dash/index.html"), "rules": dash_rules()})).await;
    let id = r["launch_id"].as_str().unwrap().to_string();
    log.push(format!("launch t={} settle={}", r["clock"]["t_us"], r["settle"]["type"]));
    macro_rules! step {
        ($name:expr, $tool:expr, $args:expr) => {{
            tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
            let started = Instant::now();
            let mut args: Value = $args;
            args["launch_id"] = json!(id);
            let (err, v) = m.try_tool($tool, args).await;
            eprintln!("dash {n}: {} err={} t_before={} t_after={} t={} outcome={} effects={} wall_ms={}", $name, err,
                v["t_before_us"], v["t_after_us"], v["t_us"], v["outcome"]["type"], v["effects"]["requests"], started.elapsed().as_millis());
            if err { eprintln!("  -> {v}"); }
            log.push(format!("{} err={} {} {} {} {} {}", $name, err, v["t_before_us"], v["t_after_us"], v["t_us"], v["outcome"]["type"], v["effects"]["requests"]));
            v
        }};
    }
    macro_rules! shot {
        ($name:expr) => {{
            let v = step!($name, "capture", json!({"op": {"type": "screenshot"}}));
            if let Some(p) = v["path"].as_str() {
                shots.push(std::fs::read(p).unwrap());
            }
            log.push(format!("{} t={} exact={} current={}", $name, v["t_us"], v["exact"], v["current"]));
        }};
    }
    let until = |p: Value| json!({"op": {"type": "until", "predicate": p, "max_ms": 3000}});
    let text = |sel: &str, t: &str| json!({"is": "text_equals", "target": css(sel), "text": t});
    step!("until live", "wait", until(text("#live", "users: 12")));
    step!("until chart", "wait", until(text("#chart-state", "ready 5")));
    step!("until first page", "wait", until(json!({"is": "count_at_least", "target": css(".item"), "n": 10})));
    step!("select 30d", "act", json!({"action": {"type": "select", "target": role("combobox", "Range"), "values": ["30d"]}}));
    step!("until chart 30d", "wait", until(text("#chart-state", "ready 7")));
    shot!("shot chart");
    step!("only errors", "act", json!({"action": {"type": "click", "target": role("checkbox", "Only errors")}}));
    step!("until chart errors", "wait", until(text("#chart-state", "ready 3")));
    step!("hover label", "act", json!({"action": {"type": "hover", "target": json!({"by": "text", "text": "Tue"})}}));
    step!("until tooltip", "wait", until(json!({"is": "visible", "target": css("#tooltip")})));
    shot!("shot tooltip");
    step!("scroll feed", "act", json!({"action": {"type": "scroll", "target": css("#feed"), "delta_x": 0, "delta_y": 2000}}));
    step!("until page 2", "wait", until(json!({"is": "count_at_least", "target": css(".item"), "n": 20})));
    step!("scroll feed again", "act", json!({"action": {"type": "scroll", "target": css("#feed"), "delta_x": 0, "delta_y": 2000}}));
    step!("until page 3", "wait", until(json!({"is": "count_at_least", "target": css(".item"), "n": 30})));
    step!("until live 15", "wait", until(text("#live", "users: 15")));
    // The billing widget, another origin's frame, and the notes editor, a frame's editable body.
    let snap = step!("snapshot", "observe", json!({"op": {"type": "snapshot"}}));
    let shown = |r: &str, name: &str| snap["nodes"].as_array().is_some_and(|a| a.iter().any(|n| n["role"] == r && n["name"] == name));
    log.push(format!("widget in snapshot: {}", shown("button", "Upgrade")));
    step!("upgrade", "act", json!({"action": {"type": "click", "target": role("button", "Upgrade")}, "then": {"type": "none"}}));
    step!("until thanks", "wait", until(text("#state", "Thanks!")));
    step!("until plan", "wait", until(text("#plan", "Plan: Pro")));
    step!("type note", "act", json!({"action": {"type": "type", "target": css("body.note"), "text": "Ship it"}}));
    step!("until note count", "wait", until(text("#note-count", "7 chars")));
    shot!("shot billing");
    step!("reload", "act", json!({"action": {"type": "reload"}}));
    let q = step!("query range", "observe", json!({"op": {"type": "query", "target": css("#range"), "props": ["value"]}}));
    log.push(format!("range after reload: {}", q["values"]));
    shot!("shot reloaded");
    let stopped = m.call_tool("stop", json!({"launch_id": id})).await;
    eprintln!("dash {n}: whole run {} ms", wall.elapsed().as_millis());
    (log, shots, format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap()))
}

#[tokio::test]
async fn a_dashboard_test_runs_the_same_every_time() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    if !controlled_engine_or_check_baseline(&mut m, addr).await {
        return;
    }
    let mut runs = Vec::new();
    // Two runs back to back, and one with wall time between every step.
    for n in 0..3 {
        runs.push(dash_run(&mut m, addr, n, if n == 2 { 200 } else { 0 }).await);
    }
    let (log, shots, trace) = &runs[0];
    assert!(log.iter().all(|l| !l.contains("err=true") && !l.contains("timed_out")), "{log:#?}");
    // The filters: the checkbox's request answered 150 ms after the click, the chart drawn over 400 ms by
    // animation frames, ready at the frame tick after.
    assert!(log.iter().any(|l| l.starts_with("until chart errors err=false null null 2551000")), "{log:#?}");
    // The tooltip 300 ms after the hover began, at the end of the hover's 250 ms settle.
    assert!(log.iter().any(|l| l.starts_with("until tooltip err=false null null 2851000")), "{log:#?}");
    // Infinite scroll: each scroll loads the next page.
    assert!(log.iter().any(|l| l.contains("scroll feed again") && l.contains("items?page=3")), "{log:#?}");
    // The range chosen before the reload is restored from storage.
    assert!(log.iter().any(|l| l == r#"range after reload: {"value":"30d"}"#), "{log:#?}");
    // The widget's controls are in the page's snapshot; its thanks come 250 ms after the click, and the
    // page hears of it at that instant.
    assert!(log.iter().any(|l| l == "widget in snapshot: true"), "{log:#?}");
    // A step's line is `<name> err=<bool> <t_before> <t_after> <t> ...`.
    let at = |name: &str| -> u64 {
        let line = log.iter().find(|l| l.starts_with(&format!("{name} err=false "))).unwrap_or_else(|| panic!("{name} in {log:#?}"));
        line[name.len()..].split(' ').nth(4).unwrap().parse().unwrap_or_else(|_| panic!("{line}"))
    };
    assert_eq!(at("until thanks") - at("until live 15"), 250_000, "{log:#?}");
    assert_eq!(at("until plan"), at("until thanks"), "{log:#?}");
    assert_eq!(shots.len(), 4);
    let first = std::fs::read_to_string(trace).unwrap();
    // The page's document comes from a server (`passthrough_content`); nothing else taints the run.
    let taints: Vec<&str> = first.lines().filter(|l| l.contains("\"taint\"") && !l.contains("\"passthrough_content\"")).collect();
    assert!(taints.is_empty(), "{taints:#?}");
    for (n, (other_log, other_shots, other_trace)) in runs.iter().enumerate().skip(1) {
        assert_eq!(first_difference(log, other_log), None, "run {n}");
        assert!(other_shots == shots, "run {n}: the same screenshots");
        assert_eq!(frontcap_core::projection::compare_behavior(&first, &std::fs::read_to_string(other_trace).unwrap()), Ok(()), "run {n}");
    }
}

fn notes_rules() -> Value {
    let api = |path: &str| format!("https://api.notes.test{path}");
    json!([
        {"id": "list", "match": {"url": api("/notes")}, "action": {"type": "mock", "status": 200, "headers": {"content-type": "application/json"},
            "body": r#"[{"id":"n1","title":"Ideas","text":"Write more tests"},{"id":"n2","title":"Groceries","text":"eggs"}]"#, "latency_ms": 150}},
        {"id": "save", "match": {"method": "PUT", "url": api("/notes/*")}, "action": {"type": "mock", "status": 200, "body": "{}", "latency_ms": 100}},
        {"id": "delete", "match": {"method": "DELETE", "url": api("/notes/*")}, "action": {"type": "mock", "status": 204, "body": "", "latency_ms": 50}}
    ])
}

/// An agent's walkthrough of a notes app: keyboard shortcuts, an autosave toast, a confirm dialog, a
/// slider, a hover tooltip and a preview window. Each step's outcome is printed; the assertions are what
/// a person testing this app would check.
#[tokio::test]
async fn notes_agent_walkthrough() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/notes/index.html"), "rules": notes_rules()})).await;
    let id = r["launch_id"].as_str().unwrap().to_string();
    let mut problems: Vec<String> = Vec::new();
    macro_rules! step {
        ($name:expr, $tool:expr, $args:expr) => {{
            let mut args: Value = $args;
            args["launch_id"] = json!(id);
            let (err, v) = m.try_tool($tool, args).await;
            eprintln!("STEP {} err={} -> {}", $name, err, serde_json::to_string(&v).unwrap().chars().take(600).collect::<String>());
            if err { problems.push(format!("{}: {}", $name, v)); }
            v
        }};
    }
    let until = |p: Value, ms: u64| json!({"op": {"type": "until", "predicate": p, "max_ms": ms}});
    let text_of = |v: &Value| v["values"]["text"].clone();
    macro_rules! check {
        ($what:expr, $cond:expr) => {{ if !$cond { eprintln!("PROBLEM {}", $what); problems.push($what.to_string()); } }};
    }

    step!("notes listed", "wait", until(json!({"is": "count_at_least", "target": css("#list button"), "n": 2}), 2000));
    step!("open groceries", "act", json!({"action": {"type": "click", "target": role("button", "Groceries")}}));
    let t = step!("title", "observe", json!({"op": {"type": "query", "target": css("#title"), "props": ["text"]}}));
    check!("the opened note's title", text_of(&t) == json!("Groceries"));

    // Autosave: typing, then the toast appears for a moment.
    step!("type", "act", json!({"action": {"type": "type", "target": role("textbox", "Note text"), "text": ", milk"}, "then": {"type": "none"}}));
    let toast = step!("until saved toast", "wait", until(json!({"is": "visible", "target": {"by": "text", "text": "Saved"}}), 2000));
    check!("the toast appears 600 ms after typing (500 debounce + 100 save)", toast["outcome"]["type"] == json!("condition_met") && toast["elapsed_us"] == json!(600000));
    step!("toast fades", "wait", json!({"op": {"type": "advance", "by_ms": 2000}}));
    let gone = step!("toast after fade", "observe", json!({"op": {"type": "query", "target": css("#toast"), "props": ["visible", "style:opacity"]}}));
    check!("the toast has faded", gone["values"]["style:opacity"] == json!("0"));

    // Command palette by keyboard.
    step!("ctrl+k", "act", json!({"action": {"type": "press", "target": role("textbox", "Note text"), "key": "Control+k"}}));
    let pal = step!("palette", "observe", json!({"op": {"type": "query", "target": css("#palette"), "props": ["visible"]}}));
    check!("Ctrl+K opens the palette", pal["values"]["visible"] == json!(true));
    step!("filter", "act", json!({"action": {"type": "type", "target": role("textbox", "Command"), "text": "new"}}));
    step!("enter", "act", json!({"action": {"type": "press", "target": role("textbox", "Command"), "key": "Enter"}}));
    let t = step!("new title", "observe", json!({"op": {"type": "query", "target": css("#title"), "props": ["text"]}}));
    check!("Enter runs the filtered command", text_of(&t) == json!("Untitled 3"));

    // Slider by keyboard and by pointer.
    step!("slider arrows", "act", json!({"action": {"type": "press", "target": role("slider", "Font size"), "key": "ArrowRight"}}));
    let v = step!("slider value", "observe", json!({"op": {"type": "query", "target": css("#size-value"), "props": ["text"]}}));
    check!("ArrowRight moves the slider", text_of(&v) == json!("15"));
    step!("slider drag", "act", json!({"action": {"type": "drag", "target": role("slider", "Font size"), "offset": {"dx": 40, "dy": 0}}}));
    let v = step!("slider value after drag", "observe", json!({"op": {"type": "query", "target": css("#size-value"), "props": ["text"]}}));
    check!("dragging the thumb right raises the size", text_of(&v).as_str().and_then(|s| s.parse::<u32>().ok()).is_some_and(|n| n > 15));

    // Tooltip after a hover delay.
    step!("hover delete", "act", json!({"action": {"type": "hover", "target": role("button", "Delete note")}, "then": {"type": "none"}}));
    step!("tooltip", "wait", until(json!({"is": "visible", "target": css("#tip")}), 1000));

    // Delete with a confirm dialog: the person accepts it.
    step!("accept delete confirm", "session", json!({"operation": "dialog_rules", "rules": [{"kind": "confirm", "message": "Delete*", "answer": "accept"}]}));
    let del = step!("delete", "act", json!({"action": {"type": "click", "target": role("button", "Delete note")}}));
    eprintln!("dialogs: {}", del["effects"]["dialogs"]);
    let t = step!("title after delete", "observe", json!({"op": {"type": "query", "target": css("#title"), "props": ["text"]}}));
    check!("accepting the confirm deletes the note", text_of(&t) == json!("No note"));

    // A preview window: a popup context of the generation.
    let preview = step!("preview", "act", json!({"action": {"type": "click", "target": role("button", "Open preview")}}));
    check!("the preview window opens as a popup context", preview["effects"]["contexts"] == json!(["popup-1 opened by page"]));
    let s = step!("status", "observe", json!({"op": {"type": "query", "target": css("#status"), "props": ["text"]}}));
    check!("the page sees its window", text_of(&s) == json!("Preview open"));
    let contexts = step!("contexts", "session", json!({"operation": "contexts"}));
    check!("the preview window is listed", contexts["contexts"][1]["id"] == "popup-1" && contexts["contexts"][1]["open"] == true);

    let stopped = m.call_tool("stop", json!({"launch_id": id})).await;
    let trace = std::fs::read_to_string(format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap())).unwrap();
    check!("the window's opening is in the trace", trace.contains("\"kind\":\"context\"") && !trace.contains("\"new_window\""));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// An agent's walkthrough of a mail app with held input: Shift+click and Control+click selection while
/// a modifier is held, keyboard hints visible only while Alt is held, a right-click menu, Korean search
/// input composed syllable by syllable, Enter while composing, and a swipe and a long press by touch.
#[tokio::test]
async fn mail_agent_walkthrough() {
    let addr = support::serve();
    let ws = support::workspace();
    let mut m = support::Mcp::start_in(&ws.root()).await;
    let r = m.call_tool("launch", json!({"url": format!("http://{addr}/mail/index.html"), "profile": {"touch": true}})).await;
    let id = r["launch_id"].as_str().unwrap().to_string();
    let mut problems: Vec<String> = Vec::new();
    macro_rules! step {
        ($name:expr, $tool:expr, $args:expr) => {{
            let mut args: Value = $args;
            args["launch_id"] = json!(id);
            let (err, v) = m.try_tool($tool, args).await;
            eprintln!("STEP {} err={} -> {}", $name, err, serde_json::to_string(&v).unwrap().chars().take(600).collect::<String>());
            if err { problems.push(format!("{}: {}", $name, v)); }
            v
        }};
    }
    macro_rules! check {
        ($what:expr, $cond:expr) => {{ if !$cond { eprintln!("PROBLEM {}", $what); problems.push($what.to_string()); } }};
    }
    async fn text_in(m: &mut support::Mcp, id: &str, sel: &str) -> Value {
        m.call_tool("observe", json!({"launch_id": id, "op": {"type": "query", "target": css(sel), "props": ["text"]}})).await["values"]["text"].clone()
    }
    let msg = |subject: &str| role("option", subject);

    // Selection with held modifiers.
    step!("click Invoice", "act", json!({"action": {"type": "click", "target": msg("Invoice")}}));
    let held = step!("hold shift", "input", json!({"operation": "key_down", "key": "Shift"}));
    check!("the held key is reported", held["keys"] == json!(["Shift"]));
    step!("shift+click Weekly report", "act", json!({"action": {"type": "click", "target": msg("Weekly report")}}));
    step!("release shift", "input", json!({"operation": "key_up", "key": "Shift"}));
    check!("Shift+click selects the range", text_in(&mut m, &id, "#selection").await == json!("4 selected"));
    // Command (Meta) on macOS: there Control+click is a right click.
    step!("hold meta", "input", json!({"operation": "key_down", "key": "Meta"}));
    step!("meta+click Lunch", "act", json!({"action": {"type": "click", "target": msg("Lunch")}}));
    step!("release meta", "input", json!({"operation": "key_up", "key": "Meta"}));
    check!("Meta+click toggles one", text_in(&mut m, &id, "#selection").await == json!("3 selected"));

    // Hints only while Alt is held.
    step!("hold alt", "input", json!({"operation": "key_down", "key": "Alt"}));
    let shown = step!("hints", "observe", json!({"op": {"type": "query", "target": css("[data-id='1'] .hint"), "props": ["visible"]}}));
    check!("holding Alt shows the hints", shown["values"]["visible"] == json!(true));
    let status = step!("input status", "input", json!({"operation": "status"}));
    check!("status names the held key", status["keys"] == json!(["Alt"]));
    step!("release alt", "input", json!({"operation": "key_up", "key": "Alt"}));
    let hidden = step!("hints after", "observe", json!({"op": {"type": "query", "target": css("[data-id='1'] .hint"), "props": ["visible"]}}));
    check!("releasing Alt hides the hints", hidden["values"]["visible"] == json!(false));

    // A right click opens the menu.
    let state = step!("status before right click", "input", json!({"operation": "status"}));
    let b = step!("lunch box", "observe", json!({"op": {"type": "query", "target": msg("Lunch"), "props": ["bbox"]}}));
    let point = |b: &Value| [b["values"]["bbox"]["x"].as_f64().unwrap_or(0.0) as i32 + 20, b["values"]["bbox"]["y"].as_f64().unwrap_or(0.0) as i32 + 8];
    let pressed = step!("right press", "input", json!({"operation": "begin", "target": msg("Lunch"), "point": point(&b), "button": "right", "boundary": state["boundary"]}));
    step!("right release", "input", json!({"operation": "end", "handle": pressed["held"]["handle"]}));
    step!("archive from menu", "act", json!({"action": {"type": "click", "target": role("menuitem", "Archive")}}));
    check!("the menu archives the message", text_in(&mut m, &id, "#archived").await == json!("Archived Lunch"));

    // Korean search: composed syllable by syllable; Enter commits the composition without searching.
    step!("focus search", "act", json!({"action": {"type": "click", "target": role("textbox", "Search mail")}}));
    step!("compose ㅅ", "input", json!({"operation": "compose", "text": "ㅅ"}));
    step!("compose 서", "input", json!({"operation": "compose", "text": "서"}));
    let composing = step!("compose 서우", "input", json!({"operation": "compose", "text": "서우"}));
    check!("status shows the composition", composing["composition"] == json!("서우"));
    check!("the app shows the composition", text_in(&mut m, &id, "#composing").await == json!("composing: 서우"));
    step!("compose 서울", "input", json!({"operation": "compose", "text": "서울"}));
    check!("the list is not filtered while composing", text_in(&mut m, &id, "#selection").await == json!("3 selected"));
    let committed = step!("commit with Enter", "input", json!({"operation": "commit", "key": "Enter"}));
    check!("the commit ends the composition", committed["composition"].is_null());
    check!("Enter that commits a composition does not search", text_in(&mut m, &id, "#searched").await == json!(""));
    let list = text_in(&mut m, &id, "#list").await;
    // `text` is the text content: the hidden hint is in it.
    check!(format!("the finished composition filters the list: {list}"), list == json!("서울 출장Alt+1"));
    step!("enter", "act", json!({"action": {"type": "press", "target": role("textbox", "Search mail"), "key": "Enter"}}));
    check!("Enter after the composition searches", text_in(&mut m, &id, "#searched").await == json!("Searched: 서울"));
    step!("clear search", "act", json!({"action": {"type": "fill", "target": role("textbox", "Search mail"), "value": ""}}));

    // Touch: swipe Party left to archive it; long-press Invoice for the menu.
    let state = step!("status before swipe", "input", json!({"operation": "status"}));
    let b = step!("party box", "observe", json!({"op": {"type": "query", "target": msg("Party"), "props": ["bbox"]}}));
    let [x, y] = point(&b);
    let t = step!("touch party", "input", json!({"operation": "begin", "pointer": "touch", "target": msg("Party"), "point": [x + 100, y], "boundary": state["boundary"]}));
    let mut boundary = t["boundary"].clone();
    for dx in [30, 60, 90, 120] {
        let mv = step!("swipe", "input", json!({"operation": "move", "handle": t["held"]["handle"], "point": [x + 100 - dx, y], "boundary": boundary}));
        boundary = mv["boundary"].clone();
    }
    step!("lift", "input", json!({"operation": "end", "handle": t["held"]["handle"]}));
    check!("swiping left archives", text_in(&mut m, &id, "#archived").await == json!("Archived Party"));
    let state = step!("status before long press", "input", json!({"operation": "status"}));
    let b = step!("invoice box", "observe", json!({"op": {"type": "query", "target": msg("Invoice"), "props": ["bbox"]}}));
    let t = step!("press invoice", "input", json!({"operation": "begin", "pointer": "touch", "target": msg("Invoice"), "point": point(&b), "boundary": state["boundary"]}));
    step!("hold 600 ms", "wait", json!({"op": {"type": "advance", "by_ms": 600}}));
    let menu = step!("menu", "observe", json!({"op": {"type": "query", "target": css("#menu"), "props": ["visible"]}}));
    check!("a long press opens the menu", menu["values"]["visible"] == json!(true));
    step!("lift", "input", json!({"operation": "end", "handle": t["held"]["handle"]}));

    let stopped = m.call_tool("stop", json!({"launch_id": id})).await;
    let trace = std::fs::read_to_string(format!("{}/trace.jsonl", stopped["bundle"].as_str().unwrap())).unwrap();
    check!("held keys are in the trace", trace.contains("\"key_down\"") && trace.contains("\"key_up\""));
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}
