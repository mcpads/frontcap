//! Held-request readiness: a request the platform holds does not hold virtual time, yet a new fetch still stops
//! it at its issue. A request (or a stream's handshake) issued while another one's latency runs is
//! handled at its own instant — not at the end of a segment — whatever the slicing of time.
mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

fn rules() -> RuleSet {
    RuleSet::from_json(
        r#"[{"id":"slow","match":{"url":"https://api.example.test/slow"},"action":{"type":"mock","status":200,"body":"s","latency_ms":200}},
            {"id":"fast","match":{"url":"https://api.example.test/fast"},"action":{"type":"mock","status":200,"body":"f"}},
            {"id":"feed","match":{"url":"https://api.example.test/feed"},"action":{"type":"stream_mock","messages":[{"at_ms":30,"data":"a"}]}}]"#,
    )
    .unwrap()
}

async fn run(addr: std::net::SocketAddr, name: &str, slices: u64) -> (Vec<serde_json::Value>, String) {
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), name, format!("http://{addr}/held_clock.html"), rules())).await.unwrap();
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g.eval_main("go(17)").await.unwrap();
    for _ in 0..slices {
        g.advance(VirtualDuration::from_ms(600 / slices)).await.unwrap();
    }
    let log = serde_json::from_str(g.eval_main("JSON.stringify(log)").await.unwrap().as_str().unwrap()).unwrap();
    let trace = std::fs::read_to_string(g.trace_path()).unwrap();
    (log, trace)
}

#[tokio::test]
async fn requests_and_streams_under_a_held_request_are_handled_at_their_issue() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let addr = support::serve();
    let (one, trace) = run(addr, "hold-1", 1).await;
    // Issued at 37, while the slow request's latency runs: the zero-latency response is fulfilled at the
    // instant the request stopped the clock and the page sees it there; the stream opens at its issue and
    // its message comes 30 ms later; the slow one at its issue (20) plus 200.
    assert_eq!(serde_json::Value::Array(one.clone()), serde_json::json!([
        ["issue", 37], ["fast", 37], ["sse-open", 37], ["sse", "a", 67], ["slow", 220]
    ]));
    assert!(!trace.contains("late_delivery"), "{trace}");
    let (many, _) = run(addr, "hold-120", 120).await;
    assert_eq!(one, many, "the same for any slicing of time");
}




fn rules_with(fast_latency_ms: u64) -> RuleSet {
    RuleSet::from_json(&format!(
        r#"[{{"id":"slow","match":{{"url":"https://api.example.test/slow"}},"action":{{"type":"mock","status":200,"body":"s","latency_ms":200}}}},
            {{"id":"held","match":{{"url":"https://api.example.test/held"}},"action":{{"type":"hold"}}}},
            {{"id":"fast","match":{{"url":"https://api.example.test/fast*"}},"action":{{"type":"mock","status":200,"body":"f","latency_ms":{fast_latency_ms}}}}},
            {{"id":"feed","match":{{"url":"https://api.example.test/feed"}},"action":{{"type":"stream_mock","messages":[{{"at_ms":30,"data":"a"}}]}}}}]"#
    ))
    .unwrap()
}

/// `call` at 20 ms, then 600 ms in `slices` advances: the page's log, the generation for more.
async fn scenario(name: &str, rules: RuleSet, call: &str, slices: u64) -> (Vec<serde_json::Value>, Generation, tempfile::TempDir) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), name, format!("http://{addr}/held_clock.html"), rules)).await.unwrap();
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g.eval_main(call).await.unwrap();
    for _ in 0..slices {
        g.advance(VirtualDuration::from_ms(600 / slices)).await.unwrap();
    }
    let log = serde_json::from_str(g.eval_main("JSON.stringify(log)").await.unwrap().as_str().unwrap()).unwrap();
    (log, g, runs)
}

#[tokio::test]
async fn latency_mocks_under_a_held_request_keep_their_instants_for_any_slicing() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // 50 ms, and 5 ms — shorter than the engine's 10 ms completion step, which runs after the page has
    // seen the response.
    for (latency, fast) in [(50u64, 87), (5, 42)] {
        let (one, _, _) = scenario(&format!("lat-{latency}-1"), rules_with(latency), "go(17)", 1).await;
        assert!(serde_json::Value::Array(one.clone()).to_string().contains(&format!("[\"fast\",{fast}]")), "{latency} ms: {one:?}");
        let (many, _, _) = scenario(&format!("lat-{latency}-120"), rules_with(latency), "go(17)", 120).await;
        assert_eq!(one, many, "{latency} ms: the same for any slicing");
    }
}

#[tokio::test]
async fn a_fetch_from_a_response_callback_runs_at_the_same_instant_in_every_run() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let mut seen = Vec::new();
    for (i, slices) in [1u64, 120, 1, 120].into_iter().enumerate() {
        let (log, _, _) = scenario(&format!("chain-{i}"), rules_with(0), "chain()", slices).await;
        seen.push(log);
    }
    assert_eq!(serde_json::Value::Array(seen[0].clone()), serde_json::json!([["a", 20], ["b", 20]]), "{seen:?}");
    assert!(seen.iter().all(|l| *l == seen[0]), "{seen:?}");
}

#[tokio::test]
async fn a_held_request_the_page_aborts_ends_its_hold() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let (log, mut g, _runs) = scenario("abort", rules_with(0), "abortable(30)", 1).await;
    assert_eq!(serde_json::Value::Array(log), serde_json::json!([["held-aborted", 50], ["slow-aborted", 50]]));
    assert!(g.held().is_empty(), "{:?}", g.held());
    // Its due delivery is gone with it: time goes on.
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    let trace = std::fs::read_to_string(g.trace_path()).unwrap();
    assert_eq!(trace.matches("\"source\":\"fail\"").count(), 2, "{trace}");
}

#[tokio::test]
async fn a_held_request_whose_document_goes_ends_its_hold() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let (_, mut g, _runs) = scenario("nav", rules_with(0), "abortable(100000); setTimeout(() => location.reload(), 30); 0", 1).await;
    assert!(g.held().is_empty(), "{:?}", g.held());
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
}
