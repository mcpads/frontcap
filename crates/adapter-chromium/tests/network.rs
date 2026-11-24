mod support;

use frontcap_chromium::{ChromiumError, generation::Generation};
use frontcap_core::event::{EventKind, ResponseSource};
use frontcap_core::log::BehaviorLog;
use frontcap_core::rules::{MockSpec, RuleSet};
use frontcap_core::time::VirtualDuration;
use std::collections::BTreeMap;

/// A worker's fetches have no document loader, but still have a renderer issue order.
/// Nested worker scripts are browser-issued and must remain serviceable first.
#[tokio::test]
async fn a_nested_workers_requests_keep_their_issue_order() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":"https://order.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><script>new Worker('/parent.js')</script>"}},
        {"id":"parent","match":{"url":"https://order.example.test/parent.js"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/javascript"},"body":"new Worker('/child.js')"}},
        {"id":"child","match":{"url":"https://order.example.test/child.js"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/javascript"},"body":"setTimeout(()=>{for(let i=0;i<8;i++)fetch('/req/'+i)},50)"}},
        {"id":"requests","match":{"url":"https://order.example.test/req/*"},"action":{"type":"mock","status":200,"body":"ok"}}
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "worker-order", "https://order.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let requests: Vec<_> = BehaviorLog::read(&g.trace_path()).unwrap().into_iter().filter_map(|e| match e.kind {
        EventKind::Request(r) if r.url.contains("/req/") => Some((r.url, e.t_us.0)),
        _ => None,
    }).collect();
    let expected: Vec<_> = (0..8).map(|i| (format!("https://order.example.test/req/{i}"), 50_000)).collect();
    assert_eq!(requests, expected);
    g.stop().await.unwrap();
}

fn mock_rules(latency_ms: u64) -> RuleSet {
    RuleSet::from_json(&format!(
        r#"[{{"id":"api","match":{{"url":"https://api.example.test/*"}},
             "action":{{"type":"mock","status":200,"headers":{{"access-control-allow-origin":"*"}},
                        "body":"ok","latency_ms":{latency_ms}}}}}]"#
    ))
    .unwrap()
}

async fn launched(run: &str, rules: RuleSet) -> (tempfile::TempDir, Generation) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/fetch.html"), rules)).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    (runs, g)
}

fn log(v: serde_json::Value) -> Vec<serde_json::Value> {
    serde_json::from_str(v.as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn mocked_latency_is_exact() {
    let (_runs, mut g) = launched("net-1", mock_rules(500)).await;
    g.eval_main("go(['https://api.example.test/a'])").await.unwrap();
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    let entries = log(g.eval_main("JSON.stringify(window.__log)").await.unwrap());
    let issued = entries[0][2].as_u64().unwrap();
    let done = entries.iter().find(|e| e[0] == "done").expect("done");
    assert_eq!(done[2], 200);
    assert_eq!(done[3], "ok");
    assert_eq!(done[4].as_u64().unwrap() - issued, 500);
}

#[tokio::test]
async fn unmatched_external_request_is_blocked() {
    let (_runs, mut g) = launched("net-2", RuleSet::default()).await;
    g.eval_main("go(['https://cdn.example.test/x.js'])").await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let entries = log(g.eval_main("JSON.stringify(window.__log)").await.unwrap());
    assert!(entries.iter().any(|e| e[0] == "failed"));
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Response(r)
        if r.source == ResponseSource::Fail && r.rule.as_ref().map(|r| r.as_str()) == Some("default-block"))));
}

#[tokio::test]
async fn same_origin_document_passes_through() {
    let (_runs, g) = launched("net-3", RuleSet::default()).await;
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Response(r)
        if r.source == ResponseSource::Passthrough && r.rule.as_ref().map(|r| r.as_str()) == Some("default-passthrough-0"))));
}

#[tokio::test]
async fn second_request_during_latency_segment() {
    let (_runs, mut g) = launched("net-4", mock_rules(500)).await;
    g.eval_main("go(['https://api.example.test/a']); setTimeout(() => go(['https://api.example.test/b']), 200)").await.unwrap();
    g.advance(VirtualDuration::from_ms(1500)).await.unwrap();
    let entries = log(g.eval_main("JSON.stringify(window.__log)").await.unwrap());
    let done: Vec<_> = entries.iter().filter(|e| e[0] == "done").collect();
    assert_eq!(done.len(), 2, "{entries:?}");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.windows(2).all(|w| w[0].t_us <= w[1].t_us));
}

#[tokio::test]
async fn held_request_waits_for_the_agent() {
    let rules = RuleSet::from_json(r#"[{"id":"api","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}]"#).unwrap();
    let (_runs, mut g) = launched("net-5", rules).await;
    g.eval_main("go(['https://api.example.test/slow'])").await.unwrap();
    g.advance(VirtualDuration::from_ms(5000)).await.unwrap();
    assert_eq!(log(g.eval_main("JSON.stringify(window.__log)").await.unwrap()).len(), 1, "still pending");
    let held = g.held();
    assert_eq!(held.len(), 1);
    let mut headers = BTreeMap::new();
    headers.insert("access-control-allow-origin".to_string(), "*".to_string());
    g.respond(&held[0], MockSpec { status: 201, headers, body: "late".into(), body_base64: None, latency_ms: 0 }).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let entries = log(g.eval_main("JSON.stringify(window.__log)").await.unwrap());
    let done = entries.iter().find(|e| e[0] == "done").expect("done");
    assert_eq!(done[2], 201);
    assert!(g.held().is_empty());
}

#[tokio::test]
async fn second_request_latency_is_exact() {
    let (_runs, mut g) = launched("net-6", mock_rules(500)).await;
    g.eval_main("go(['https://api.example.test/a']); setTimeout(() => go(['https://api.example.test/b']), 203)").await.unwrap();
    g.advance(VirtualDuration::from_ms(1500)).await.unwrap();
    let entries = log(g.eval_main("JSON.stringify(window.__log)").await.unwrap());
    let at = |kind: &str, url: &str, i: usize| {
        entries.iter().find(|e| e[0] == kind && e[1] == url).unwrap_or_else(|| panic!("{kind} {url}: {entries:?}"))[i].as_u64().unwrap()
    };
    let b = "https://api.example.test/b";
    assert_eq!(at("done", b, 4) - at("issued", b, 2), 500, "{entries:?}");
}

async fn network_run(run: &str) -> Vec<serde_json::Value> {
    let (_runs, mut g) = launched(run, mock_rules(120)).await;
    g.eval_main("go(['https://api.example.test/a']); setTimeout(() => go(['https://api.example.test/b', 'https://api.example.test/c']), 37)").await.unwrap();
    g.advance(VirtualDuration::from_ms(400)).await.unwrap();
    log(g.eval_main("JSON.stringify(window.__log)").await.unwrap())
}

#[tokio::test]
async fn repeated_network_runs_are_identical() {
    let first = network_run("net-7a").await;
    assert_eq!(first.iter().filter(|e| e[0] == "done").count(), 3, "{first:?}");
    for i in 0..3 {
        assert_eq!(network_run(&format!("net-7{i}")).await, first);
    }
}

#[tokio::test]
async fn failed_first_navigation_is_an_error() {
    let runs = tempfile::tempdir().unwrap();
    let opts = support::options(runs.path(), "net-8", "https://blocked.example.test/".into(), RuleSet::default());
    let opts = frontcap_chromium::generation::LaunchOptions { passthrough_origins: Some(vec![]), ..opts };
    match Generation::launch(opts).await {
        Err(ChromiumError::Navigation(text)) => assert!(text.starts_with("net::"), "{text}"),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("navigation to a blocked origin succeeded"),
    }
}

#[tokio::test]
async fn latency_on_a_document_is_unsupported() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(&format!(
        r#"[{{"id":"doc","match":{{"url":"http://{addr}/fetch.html"}},
             "action":{{"type":"mock","status":200,"headers":{{"content-type":"text/html"}},"body":"<p>mock</p>","latency_ms":50}}}}]"#
    ))
    .unwrap();
    match Generation::launch(support::options(runs.path(), "net-9", format!("http://{addr}/fetch.html"), rules)).await {
        Err(ChromiumError::Unsupported(why)) => assert!(why.contains("latency"), "{why}"),
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("latency on the document was accepted"),
    }
}

#[tokio::test]
async fn long_advance_while_holding_is_not_unresponsive() {
    let rules = RuleSet::from_json(r#"[{"id":"api","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}]"#).unwrap();
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut opts = support::options(runs.path(), "net-10", format!("http://{addr}/fetch.html"), rules);
    // Short, so many segments together outlast it; long enough for the document's load under a busy host
    // (100 and 500 ms ones ran out there).
    let watchdog = std::time::Duration::from_secs(2);
    opts.watchdog = watchdog;
    let mut g = Generation::launch(opts).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    g.eval_main("go(['https://api.example.test/slow'])").await.unwrap();
    let wall = std::time::Instant::now();
    g.advance(VirtualDuration::from_ms(20_000)).await.unwrap();
    let patched = browser_pin::engine::patched_binary(&support::repo_root(), browser_pin::current_platform().unwrap()).unwrap().is_some();
    if patched {
        // Engine patch 1: the engine released the held request, so no segments are needed.
        assert_eq!(g.held_released(), 1);
    } else {
        // Many short segments: far more wall time in total than the watchdog, but progress all along.
        assert!(wall.elapsed() > watchdog, "the run must outlast the watchdog to test it: {:?}", wall.elapsed());
    }
    assert_eq!(g.held().len(), 1);
}

#[tokio::test]
async fn an_interrupted_budget_does_not_end_a_later_one() {
    // A request interrupts a 500 ms budget at 100; the engine keeps that budget and reports its expiry
    // at 600, the end of the next advance as well. The advance after that must not take it for its own.
    let rules = RuleSet::from_json(
        r#"[{"id":"held","match":{"url":"https://api.example.test/held"},"action":{"type":"hold"}},
            {"id":"api","match":{"url":"https://api.example.test/*"},
             "action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"ok","latency_ms":0}}]"#,
    )
    .unwrap();
    let (_runs, mut g) = launched("net-11", rules).await;
    g.eval_main("setTimeout(() => go(['https://api.example.test/a']), 50); setTimeout(() => go(['https://api.example.test/held']), 150); setTimeout(() => go(['https://api.example.test/b']), 2600)").await.unwrap();
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    assert_eq!(g.now().0, 600_000);
    let held = g.held();
    assert_eq!(held.len(), 1);
    let mut headers = BTreeMap::new();
    headers.insert("access-control-allow-origin".to_string(), "*".to_string());
    g.respond(&held[0], MockSpec { status: 200, headers, body: "late".into(), body_base64: None, latency_ms: 0 }).await.unwrap();
    g.advance(VirtualDuration::from_ms(5000)).await.unwrap();
    assert_eq!(g.now().0, 5_600_000);
    let engine = g.eval_main("Date.now() - 946684800000").await.unwrap();
    assert_eq!(engine, 5600, "the engine agrees with the recorder's clock");
    let entries = log(g.eval_main("JSON.stringify(window.__log)").await.unwrap());
    let b_issued = entries.iter().find(|e| e[0] == "issued" && e[1] == "https://api.example.test/b").expect("b issued");
    assert_eq!(b_issued[2], 2700, "{entries:?}");
    assert!(entries.iter().any(|e| e[0] == "done" && e[1] == "https://api.example.test/b"), "{entries:?}");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.windows(2).all(|w| w[0].t_us <= w[1].t_us));
}

#[tokio::test]
async fn a_rejected_response_keeps_the_hold() {
    let rules = RuleSet::from_json(r#"[{"id":"api","match":{"url":"https://api.example.test/*"},"action":{"type":"hold"}}]"#).unwrap();
    let (_runs, mut g) = launched("net-12", rules).await;
    g.eval_main("go(['https://api.example.test/slow'])").await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let held = g.held();
    let bad = MockSpec { status: 0, headers: BTreeMap::new(), body: String::new(), body_base64: None, latency_ms: 0 };
    assert!(g.respond(&held[0], bad).await.is_err());
    assert_eq!(g.held(), held, "the hold is still there to answer");
    let mut headers = BTreeMap::new();
    headers.insert("access-control-allow-origin".to_string(), "*".to_string());
    g.respond(&held[0], MockSpec { status: 200, headers, body: "ok".into(), body_base64: None, latency_ms: 0 }).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert!(g.held().is_empty());
    let entries = log(g.eval_main("JSON.stringify(window.__log)").await.unwrap());
    assert_eq!(entries.iter().filter(|e| e[0] == "done").count(), 1, "answered exactly once");
}

/// A request is written at its issue time even when its interception reaches the recorder after later
/// events were buffered. While a request is held the clock does not wait for interceptions, so a stream
/// of requests interleaved with small clock steps exercises exactly that; the trace stays in order and
/// every issued request has its record.
#[tokio::test]
async fn requests_are_recorded_in_order_while_another_is_held() {
    let rules = RuleSet::from_json(
        r#"[{"id":"held","match":{"url":"https://api.example.test/held"},"action":{"type":"hold"}},
            {"id":"api","match":{"url":"https://api.example.test/*"},
             "action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"ok"}}]"#,
    )
    .unwrap();
    let (_runs, mut g) = launched("net-order", rules).await;
    g.eval_main("go(['https://api.example.test/held'])").await.unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    g.eval_main("let n = 0; const id = setInterval(() => { go(['https://api.example.test/' + n]); if (++n === 60) clearInterval(id); }, 10)")
        .await
        .unwrap();
    for _ in 0..70 {
        g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    }
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.windows(2).all(|w| w[0].t_us <= w[1].t_us));
    let requests = events.iter().filter(|e| matches!(&e.kind, EventKind::Request(r) if r.url.starts_with("https://api.example.test/"))).count();
    assert_eq!(requests, 61);
}

/// All five events share one virtual instant. Neither draining all reports first nor draining all
/// requests first preserves the page's order, even though each subscription is individually ordered.
#[tokio::test]
async fn observer_samples_and_requests_keep_their_same_instant_emission_order() {
    use frontcap_core::ids::WatchName;
    use frontcap_core::predicate::{Locator, Predicate};
    use frontcap_core::event::{PointKind, SampleValue};

    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "net-watch-order", format!("http://{addr}/watch_order.html"), mock_rules(20))).await.unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    for text in ["before", "between", "after"] {
        g.watch_add(WatchName::new(text).unwrap(), Predicate::TextEquals {
            target: Locator::Css { selector: "#status".into() }, text: text.into(),
        }).await.unwrap();
    }
    let issued = g.now();
    g.eval_main("go(); void 0").await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let order: Vec<_> = events.iter().filter_map(|event| match &event.kind {
        EventKind::Request(r) if r.url.starts_with("https://api.example.test/") => Some((event.t_us, r.url.rsplit('/').next().unwrap().to_string())),
        EventKind::Watch(s) if s.point == PointKind::Mutation && s.value == SampleValue::True => Some((event.t_us, s.name.as_str().to_string())),
        _ => None,
    }).collect();
    assert_eq!(order, ["before", "first", "between", "second", "after"].map(|name| (issued, name.to_string())));
}

#[tokio::test]
async fn observer_samples_and_child_navigations_keep_their_same_instant_emission_order() {
    use frontcap_core::ids::WatchName;
    use frontcap_core::predicate::{Locator, Predicate};
    use frontcap_core::event::{PointKind, SampleValue};

    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "net-child-watch-order", format!("http://{addr}/watch_order.html"), mock_rules(0))).await.unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    for text in ["before", "between", "after"] {
        g.watch_add(WatchName::new(text).unwrap(), Predicate::TextEquals {
            target: Locator::Css { selector: "#status".into() }, text: text.into(),
        }).await.unwrap();
    }
    let issued = g.now();
    g.eval_main("go(\"frame\"); void 0").await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let order: Vec<_> = events.iter().filter_map(|event| match &event.kind {
        EventKind::Request(r) if r.url.starts_with("https://api.example.test/") => Some((event.t_us, r.url.rsplit('/').next().unwrap().to_string())),
        EventKind::Watch(s) if s.point == PointKind::Mutation && s.value == SampleValue::True => Some((event.t_us, s.name.as_str().to_string())),
        _ => None,
    }).collect();
    assert_eq!(&order[..5], &["before", "first", "between", "second", "after"].map(|name| (issued, name.to_string())));
    assert_eq!(order.iter().filter(|(_, name)| name == "first" || name == "second").count(), 2);
    // Later child parser mutations may resample the unchanged final predicate.
    assert!(order[5..].iter().all(|(_, name)| name == "after"));
}

/// The determinism profile bypasses Service Workers: a worker that answers a request cannot replace the
/// platform's network rules, so the request reaches the network path.
#[tokio::test]
async fn service_workers_are_bypassed() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "net-sw", format!("http://{addr}/sw.html"), RuleSet::default())).await.unwrap();
    let mut probe = serde_json::Value::Null;
    for _ in 0..50 {
        g.advance(VirtualDuration::from_ms(100)).await.unwrap();
        probe = g.eval_main("window.__probe").await.unwrap();
        if probe != "pending" {
            break;
        }
    }
    assert_eq!(probe, "from the network\n");
}

fn post_rule(extra: &str) -> RuleSet {
    RuleSet::from_json(&format!(
        r#"[{{"id":"save","match":{{"url":"https://api.example.test/profile","method":"POST"}},
             "action":{{"type":"mock","status":200,"body":"saved"{extra}}}}}]"#
    ))
    .unwrap()
}

async fn save_click(run: &str, rules: RuleSet) -> (tempfile::TempDir, Generation, String) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/preflight.html"), rules)).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    g.click(frontcap_core::ids::RequestId::new("c-1").unwrap(), &frontcap_core::predicate::Locator::Css { selector: "#save".into() }).await.unwrap();
    g.advance(VirtualDuration::from_ms(200)).await.unwrap();
    let out = g.eval_main("document.getElementById('out').textContent").await.unwrap().as_str().unwrap().to_string();
    (runs, g, out)
}

/// A cross-origin request that needs a CORS preflight, to an origin the rules handle: the platform
/// answers the preflight for the mock, the mock gets the CORS headers a real server would send, and
/// every event is stamped in virtual time.
#[tokio::test]
async fn a_mocked_cross_origin_post_needs_no_cors_knowledge() {
    let (_runs, g, out) = save_click("net-cors", post_rule("")).await;
    assert_eq!(out, "status 200 saved");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.iter().all(|e| e.t_us.0 < 10_000_000), "virtual instants only");
    assert!(events.windows(2).all(|w| w[0].t_us <= w[1].t_us));
}

/// A preflight the rules would block is blocked at a virtual instant and reported like any request.
#[tokio::test]
async fn an_unmatched_preflight_is_blocked_in_virtual_time() {
    let (_runs, g, out) = save_click("net-cors-2", RuleSet::default()).await;
    assert_eq!(out, "failed");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.iter().all(|e| e.t_us.0 < 10_000_000), "virtual instants only: {:?}", events.last());
}

/// A fetch started by input after the accessibility tree was read (a role locator, or a snapshot): its
/// response's task is not deferred until a frame (S15: Blink's `DeferRendererTasksAfterInput`, which the
/// platform disables), so the clock goes on and nothing starves.
#[tokio::test]
async fn a_fetch_held_for_a_frame_after_input_does_not_stall_the_clock() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(r#"[{"id":"save","match":{"url":"https://api.example.test/profile"},"action":{"type":"mock","status":200,"body":"{}"}}]"#).unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "net-starve", format!("http://{addr}/profile.html"), rules)).await.unwrap();
    g.settle(VirtualDuration::from_ms(250), VirtualDuration::from_ms(10_000)).await.unwrap();
    // Wall time between operations, as between an agent's calls; without it the engine still has a frame.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let save = frontcap_core::predicate::Locator::Role { role: "button".into(), name: Some("Save".into()) };
    g.click(frontcap_core::ids::RequestId::new("c-1").unwrap(), &save).await.unwrap();
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    assert_eq!(g.eval_main("document.querySelector('.toast') && document.querySelector('.toast').textContent").await.unwrap(), "Saved");
    let starved: Vec<u64> = BehaviorLog::read(&g.trace_path())
        .unwrap()
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Taint(t) if t.source == frontcap_core::event::TaintSource::FrameStarvation => Some(e.t_us.0),
            _ => None,
        })
        .collect();
    assert_eq!(starved, Vec::<u64>::new());
}

async fn save_twice(run: &str, rules: &str) -> (tempfile::TempDir, Generation, Vec<String>) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/preflight.html"), RuleSet::from_json(rules).unwrap())).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let mut outs = vec![];
    for i in 0..2 {
        g.click(frontcap_core::ids::RequestId::new(format!("c-{i}")).unwrap(), &frontcap_core::predicate::Locator::Css { selector: "#save".into() }).await.unwrap();
        g.advance(VirtualDuration::from_ms(200)).await.unwrap();
        outs.push(g.eval_main("document.getElementById('out').textContent").await.unwrap().as_str().unwrap().to_string());
        // Wall time between saves, under the engine's default preflight cache lifetime (5 s).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    (runs, g, outs)
}

/// A preflight is a browser mechanism: decided on method and URL (it carries neither the request's
/// headers nor its body), never consuming a sequence step, and sent for every request (no wall-clock
/// preflight cache), so traces do not depend on how fast the agent called.
#[tokio::test]
async fn preflights_follow_method_and_url_and_never_consume_a_sequence_step() {
    let rules = r#"[{"id":"save","match":{"url":"https://api.example.test/profile","method":"POST","headers":{"content-type":"application/json"}},
        "action":{"type":"sequence","after_last":"fail","steps":[{"status":200,"body":"first"},{"status":200,"body":"second"}]}}]"#;
    let (_runs, g, outs) = save_twice("net-pre", rules).await;
    assert_eq!(outs, vec!["status 200 first", "status 200 second"]);
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let options: Vec<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Request(r) if r.method == "OPTIONS" => Some(r.id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(options.len(), 2, "one preflight per request");
    // Told apart from the rule's own answers: the platform answered them for the rule.
    let sources: Vec<_> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Response(r) if options.contains(&r.id.as_str()) => Some((r.source, r.status)),
            _ => None,
        })
        .collect();
    assert_eq!(sources, vec![(ResponseSource::Preflight, 204); 2]);
}

/// Headers a mock sets are readable by cross-origin page script, as a server exposing them allows, and
/// a `vary` it sets is kept.
#[tokio::test]
async fn mocked_headers_are_exposed_to_cross_origin_script() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(r#"[{"id":"items","match":{"url":"https://api.example.test/items"},"action":{"type":"mock","status":200,"headers":{"x-total-count":"2","vary":"accept"},"body":"[]"}}]"#).unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "net-expose", format!("http://{addr}/headers.html"), rules)).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.eval_main("go(); 0").await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(g.eval_main("document.getElementById('out').textContent").await.unwrap(), "2 accept, origin");
}

/// The HTTP cache would answer by the real clock's freshness, bypassing the rules: a reload requests a
/// cacheable script again through them (scenario 3 consumer run: a reload's script left no request).
#[tokio::test]
async fn a_cacheable_response_is_requested_again_through_the_rules() {
    let page = r#"<!doctype html><script src="/app.js"></script>"#;
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":page}},
        {"id":"script","match":{"url":"https://app.example.test/app.js"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/javascript","cache-control":"max-age=3600"},"body":"window.loaded = true"}}
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "net-cache", "https://app.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.navigate_action(&frontcap_core::ids::RequestId::new("r-1").unwrap(), &frontcap_core::action::Action::Reload {}).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(g.eval_main("window.loaded === true").await.unwrap(), true);
    let scripts = BehaviorLog::read(&g.trace_path()).unwrap().iter()
        .filter(|e| matches!(&e.kind, EventKind::Request(r) if r.url.ends_with("/app.js")))
        .count();
    assert_eq!(scripts, 2, "the reload's script reaches the rules");
}
