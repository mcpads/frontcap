//! Requests the page's main thread waits for (a synchronous XHR): the page answers no command until they
//! end, so the platform answers them from the browser side at the instant their rule names, and the
//! page's clock moves to that instant before it sees the response (synchronous request completion).
mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::event::{ClockOp, ClockPhase, EventKind, ResponseSource};
use frontcap_core::ids::RequestId;
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::{VirtualDuration, VirtualTime};

fn rules(json: &str) -> RuleSet {
    RuleSet::from_json(json).unwrap()
}

fn mock(id: &str, url: &str, body: &str, latency_ms: u64) -> String {
    format!(r#"{{"id":"{id}","match":{{"url":"{url}"}},"action":{{"type":"mock","status":200,"body":"{body}","latency_ms":{latency_ms}}}}}"#)
}

/// The page, launched and started; none on a baseline engine, which cannot mark such a request.
async fn launched(run: &str, rules: RuleSet) -> Option<(tempfile::TempDir, Generation)> {
    if !support::controlled_engine_or_check_baseline().await {
        return None;
    }
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/sync-xhr.html"), rules)).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    Some((runs, g))
}

fn css(s: &str) -> Locator {
    Locator::Css { selector: s.into() }
}

async fn page_log(g: &Generation) -> Vec<serde_json::Value> {
    serde_json::from_str(g.eval_main("JSON.stringify(window.__log)").await.unwrap().as_str().unwrap()).unwrap()
}

fn waits(g: &Generation) -> Vec<(ClockPhase, VirtualTime, VirtualTime)> {
    BehaviorLog::read(&g.trace_path()).unwrap().into_iter().filter_map(|e| match e.kind {
        EventKind::Clock(c) if c.op == ClockOp::SynchronousRequest => Some((c.phase, e.t_us, c.goal_us.unwrap())),
        _ => None,
    }).collect()
}

#[tokio::test]
async fn a_click_waits_for_its_synchronous_request_for_the_rule_latency() {
    let Some((_runs, mut g)) = launched("sync-1", rules(&format!("[{}]", mock("one", "https://api.example.test/one", "first", 300)))).await else { return };
    let start = g.now();
    g.click(RequestId::new("c-1").unwrap(), &css("#one")).await.unwrap();
    let log = page_log(&g).await;
    assert_eq!(log[0][2], 200, "{log:?}");
    assert_eq!(log[0][3], "first");
    let (before, after) = (log[0][4].as_f64().unwrap(), log[0][5].as_f64().unwrap());
    assert_eq!(after - before, 300.0, "the page saw the response 300 ms after it asked: {log:?}");
    assert_eq!(g.now(), start.saturating_add(VirtualDuration::from_ms(300)));
    let arrival = start.saturating_add(VirtualDuration::from_ms(300));
    assert_eq!(waits(&g), vec![(ClockPhase::Start, start, arrival), (ClockPhase::End, arrival, arrival)]);
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let request = events.iter().find(|e| matches!(&e.kind, EventKind::Request(r) if r.url.ends_with("/one"))).expect("request");
    let response = events.iter().find(|e| matches!(&e.kind, EventKind::Response(r) if r.source == ResponseSource::Mock)).expect("response");
    assert_eq!((request.t_us, response.t_us), (start, arrival));
    // The click is recorded at the instant it was dispatched, ahead of what it caused.
    let action = events.iter().find(|e| matches!(&e.kind, EventKind::Action(_))).expect("action");
    assert_eq!(action.t_us, start);
    assert!(action.seq < request.seq);
    // The page runs on: a timer and further progress work from the arrival.
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(g.now(), arrival.saturating_add(VirtualDuration::from_ms(100)));
}

#[tokio::test]
async fn an_answer_at_the_issue_instant_moves_no_time() {
    let Some((_runs, mut g)) = launched("sync-2", rules(&format!("[{}]", mock("one", "https://api.example.test/one", "now", 0)))).await else { return };
    let start = g.now();
    g.click(RequestId::new("c-1").unwrap(), &css("#one")).await.unwrap();
    let log = page_log(&g).await;
    assert_eq!((log[0][2].as_u64(), log[0][4].as_f64() == log[0][5].as_f64()), (Some(200), true), "{log:?}");
    assert_eq!(g.now(), start);
    assert!(waits(&g).is_empty());
}

#[tokio::test]
async fn consecutive_requests_each_take_their_latency() {
    let json = format!("[{},{}]", mock("a", "https://api.example.test/a", "a", 100), mock("b", "https://api.example.test/b", "b", 200));
    let Some((_runs, mut g)) = launched("sync-3", rules(&json)).await else { return };
    let start = g.now();
    g.click(RequestId::new("c-1").unwrap(), &css("#two")).await.unwrap();
    let log = page_log(&g).await;
    let after = |i: usize| log[i][5].as_f64().unwrap() - log[0][4].as_f64().unwrap();
    assert_eq!((after(0), after(1)), (100.0, 300.0), "{log:?}");
    assert_eq!(g.now(), start.saturating_add(VirtualDuration::from_ms(300)));
}

#[tokio::test]
async fn an_earlier_asynchronous_request_waits_for_the_page_and_keeps_its_time() {
    let json = format!("[{},{}]", mock("one", "https://api.example.test/one", "sync", 200), mock("async", "https://api.example.test/async", "async", 50));
    let Some((_runs, mut g)) = launched("sync-4", rules(&json)).await else { return };
    let start = g.now();
    g.click(RequestId::new("c-1").unwrap(), &css("#mixed")).await.unwrap();
    g.settle(VirtualDuration::from_ms(100), VirtualDuration::from_ms(2000)).await.unwrap();
    let log = page_log(&g).await;
    assert_eq!(log[0][3], "sync", "{log:?}");
    // Due 50 ms after the click, while the page waited: it runs once the page does, at the arrival.
    let async_at = log.iter().find(|e| e[0] == "async").expect("async answered")[2].as_f64().unwrap();
    assert_eq!(async_at - log[0][4].as_f64().unwrap(), 200.0, "{log:?}");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let issued: Vec<_> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Request(r) if r.url.starts_with("https://api.example.test/") => Some((r.url.clone(), e.t_us)),
        _ => None,
    }).collect();
    assert_eq!(issued.len(), 2, "{issued:?}");
    assert!(issued.iter().all(|(_, t)| *t == start), "both issued at the click: {issued:?}");
}

#[tokio::test]
async fn the_arrival_field_never_reaches_the_page() {
    let Some((_runs, mut g)) = launched("sync-5", rules(&format!("[{}]", mock("same", "*/same", "same", 100)))).await else { return };
    g.click(RequestId::new("c-1").unwrap(), &css("#same")).await.unwrap();
    let log = page_log(&g).await;
    assert_eq!(log[0][3], "same", "{log:?}");
    assert!(!log[0][6].as_str().unwrap().contains("x-frontcap-arrival-us"), "{log:?}");
}

#[tokio::test]
async fn a_cross_origin_preflight_is_answered_while_the_page_waits() {
    let Some((_runs, mut g)) = launched("sync-6", rules(&format!("[{}]", mock("pre", "https://api.example.test/pre", "posted", 100)))).await else { return };
    let start = g.now();
    g.click(RequestId::new("c-1").unwrap(), &css("#pre")).await.unwrap();
    let log = page_log(&g).await;
    assert_eq!((log[0][2].as_u64(), log[0][3].as_str()), (Some(200), Some("posted")), "{log:?}");
    assert_eq!(g.now(), start.saturating_add(VirtualDuration::from_ms(100)));
}

#[tokio::test]
async fn a_failed_or_passed_through_request_ends_the_wait_at_once() {
    let json = r#"[{"id":"down","match":{"url":"https://api.example.test/one"},"action":{"type":"fail","reason":"connection_refused"}}]"#;
    let Some((_runs, mut g)) = launched("sync-7", rules(json)).await else { return };
    let start = g.now();
    g.click(RequestId::new("c-1").unwrap(), &css("#one")).await.unwrap();
    // `/same` is passed through to the fixture server by the launch defaults (it answers 404).
    g.click(RequestId::new("c-2").unwrap(), &css("#same")).await.unwrap();
    let log = page_log(&g).await;
    assert_eq!(log[0][0], "error", "{log:?}");
    assert_eq!(log[1][0], "sync", "{log:?}");
    assert_eq!(g.now(), start);
}

#[tokio::test]
async fn a_timer_waiting_during_progress_moves_the_clock_past_the_bound() {
    let Some((_runs, mut g)) = launched("sync-8", rules(&format!("[{}]", mock("t", "https://api.example.test/timer", "timer", 600)))).await else { return };
    let start = g.now();
    g.eval_main("later(100, 'https://api.example.test/timer')").await.unwrap();
    // The page waits from 100 ms to 700 ms; there is no serviceable instant inside the wait.
    g.advance(VirtualDuration::from_ms(200)).await.unwrap();
    assert_eq!(g.now(), start.saturating_add(VirtualDuration::from_ms(700)));
    let log = page_log(&g).await;
    assert_eq!(log[0][5].as_f64().unwrap() - log[0][4].as_f64().unwrap(), 600.0, "{log:?}");
    g.settle(VirtualDuration::from_ms(50), VirtualDuration::from_ms(500)).await.unwrap();
}

#[tokio::test]
async fn an_evaluated_script_that_waits_is_answered() {
    let Some((_runs, mut g)) = launched("sync-9", rules(&format!("[{}]", mock("one", "https://api.example.test/one", "eval", 250)))).await else { return };
    let start = g.now();
    let elapsed = g.evaluate("(() => { const t = Date.now(); sync('https://api.example.test/one'); return Date.now() - t; })()").await.unwrap();
    assert_eq!(elapsed.as_f64(), Some(250.0));
    assert_eq!(g.now(), start.saturating_add(VirtualDuration::from_ms(250)));
}

#[tokio::test]
async fn earlier_fetches_keep_sequence_positions_and_latency_across_synchronous_calls() {
    for preflight in [false, true] {
        let json = r#"[{"id":"sequence","match":{"url":"https://api.example.test/sequence"},"action":{"type":"sequence","after_last":"fail","steps":[
            {"status":200,"body":"first","latency_ms":50},
            {"status":200,"body":"second","latency_ms":200},
            {"status":200,"body":"third"}]}}]"#;
        let Some((_runs, mut g)) = launched("sync-prefix-sequence", rules(json)).await else { return };
        let start = g.now();
        let headers = if preflight { ",headers:{'x-prefix-test':'yes'}" } else { "" };
        g.evaluate(&format!(r#"window.got={{}};
fetch('https://api.example.test/sequence',{{method:'POST',body:'async'{headers}}}).then(r=>r.text()).then(s=>got.async=s);
const x=new XMLHttpRequest();x.open('POST','https://api.example.test/sequence',false);x.send('sync');got.sync=x.responseText;
fetch('https://api.example.test/sequence',{{method:'POST',body:'after'}}).then(r=>r.text()).then(s=>got.after=s);true"#)).await.unwrap();
        g.advance(VirtualDuration::from_ms(0)).await.unwrap();
        assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!({"async":"first","sync":"second","after":"third"}), "preflight={preflight}");
        let arrival = start.saturating_add(VirtualDuration::from_ms(200));
        assert_eq!(g.now(), arrival, "the earlier fetch's latency must not move the blocked page");
        let issued: Vec<_> = BehaviorLog::read(&g.trace_path()).unwrap().into_iter().filter_map(|e| match e.kind {
            EventKind::Request(r) if r.url.ends_with("/sequence") && r.method == "POST" => Some(e.t_us),
            _ => None,
        }).collect();
        assert_eq!(issued, vec![start, start, arrival]);
        g.stop().await.unwrap();
    }
}

#[tokio::test]
async fn an_earlier_hold_is_not_the_request_blocking_the_page() {
    let json = format!("[{{\"id\":\"hold\",\"match\":{{\"url\":\"https://api.example.test/held\"}},\"action\":{{\"type\":\"hold\"}}}},{}]", mock("sync", "https://api.example.test/sync", "done", 0));
    let Some((_runs, mut g)) = launched("sync-prefix-hold", rules(&json)).await else { return };
    let start = g.now();
    let holds = g.blocking_holds();
    g.evaluate("window.got={};fetch('https://api.example.test/held').then(r=>r.text()).then(s=>got.async=s);const x=new XMLHttpRequest();x.open('GET','https://api.example.test/sync',false);x.send();got.sync=x.responseText;true").await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!({"sync":"done"}));
    assert!(holds.pending().is_none(), "an earlier asynchronous hold must not suspend the operation");
    let held = g.held();
    assert_eq!(held.len(), 1);
    let answer = serde_json::from_value(serde_json::json!({"status":200,"body":"released"})).unwrap();
    g.respond(&held[0], answer).await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.eval_main("got").await.unwrap(), serde_json::json!({"async":"released","sync":"done"}));
    assert_eq!(g.now(), start);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_timer_prefix_stays_visible_while_the_pause_services_its_synchronous_request() {
    let rules = rules(r#"[{"id":"order","match":{"url":"https://api.example.test/*"},"action":{"type":"sequence","steps":[{"status":200,"body":"prefix"},{"status":200,"body":"sync","latency_ms":3}],"after_last":"fail"}}]"#);
    let Some((_runs, mut g)) = launched("timer-prefix", rules).await else { return };
    let start = g.now();
    g.evaluate("window.answers=[];setTimeout(()=>{fetch('https://api.example.test/prefix').then(r=>r.text()).then(v=>answers.push(v));const x=new XMLHttpRequest();x.open('GET','https://api.example.test/sync',false);x.send();answers.push(x.responseText)},10);true").await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(g.eval_main("answers").await.unwrap(), serde_json::json!(["sync", "prefix"]));
    assert_eq!(g.now(), start.saturating_add(VirtualDuration::from_ms(50)));
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let requests: Vec<_> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Request(r) if r.rule.as_ref().is_some_and(|r| r.as_str() == "order") => Some((r, e.t_us)), _ => None,
    }).collect();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].0.url.ends_with("/prefix"));
    assert!(requests[1].0.url.ends_with("/sync"));
    assert_eq!(requests[0].1, start.saturating_add(VirtualDuration::from_ms(10)));
    assert_eq!(requests[1].1, requests[0].1);
    let answers: Vec<_> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Response(r) if r.id == requests[1].0.id => Some(e.t_us), _ => None,
    }).collect();
    assert_eq!(answers, vec![start.saturating_add(VirtualDuration::from_ms(13))]);
    g.stop().await.unwrap();
}
