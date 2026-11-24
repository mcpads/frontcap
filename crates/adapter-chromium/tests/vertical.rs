mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::bundle::{Bundle, Finalization};
use frontcap_core::clock::SettleOutcome;
use frontcap_core::event::{ClockOutcome, EventKind, SampleValue};
use frontcap_core::ids::{RequestId, RunId, WatchName};
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::{Locator, Predicate};
use frontcap_core::projection::compare_behavior;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::{VirtualDuration, VirtualTime};
use std::time::Instant;

fn rules() -> RuleSet {
    RuleSet::from_json(
        r#"[{"id":"toast-api","match":{"url":"https://api.example.test/toast"},
             "action":{"type":"mock","status":200,
                       "headers":{"content-type":"application/json","access-control-allow-origin":"*"},
                       "body":"{\"text\":\"saved\"}","latency_ms":500}}]"#,
    )
    .unwrap()
}

/// Criterion 1 at level `time`; returns (trace text, first-true time − click time, wall of act→until).
async fn scenario(addr: std::net::SocketAddr, runs: &std::path::Path, run: &str) -> (String, u64, std::time::Duration) {
    let mut g = Generation::launch(support::options(runs, run, format!("http://{addr}/toast.html"), rules())).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let shown = WatchName::new("toast-shown").unwrap();
    g.watch_add(shown.clone(), Predicate::Visible { target: Locator::Css { selector: ".toast".into() } }).await.unwrap();
    let wall = Instant::now();
    let click_t = g.now();
    g.click(RequestId::new("click-go").unwrap(), &Locator::Css { selector: "#go".into() }).await.unwrap();
    let settled = g.settle(VirtualDuration::from_ms(250), VirtualDuration::from_ms(2000)).await.unwrap();
    assert!(matches!(settled, SettleOutcome::Quiescent { .. }), "{settled:?}");
    let until = g.until(&shown, VirtualDuration::from_ms(10_000)).await.unwrap();
    let elapsed = wall.elapsed();
    assert_eq!(until, ClockOutcome::ConditionMet { watch: shown.clone() });
    let trace = g.trace_path();
    let manifest = g.stop().await.unwrap();
    assert_eq!(manifest.finalization, Finalization::Finalized);
    let events = BehaviorLog::read(&trace).unwrap();
    let first_true = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::Watch(s) if s.name == shown && s.value == SampleValue::True => Some(e.t_us),
            _ => None,
        })
        .expect("toast became visible");
    (std::fs::read_to_string(&trace).unwrap(), first_true.since(click_t).as_micros(), elapsed)
}

#[tokio::test]
async fn toast_scenario_is_exact_and_fast() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let (_trace, delta, wall) = scenario(addr, runs.path(), "vertical-1").await;
    assert_eq!(delta, 3_500_000, "toast must appear exactly 3000 ms + 500 ms latency after the click");
    eprintln!("act → until wall time: {wall:?}");
    assert!(wall.as_millis() < 1000, "act → until took {wall:?}");
}

#[tokio::test]
async fn repeated_runs_have_equal_behavior() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut traces = Vec::new();
    for i in 0..3 {
        traces.push(scenario(addr, runs.path(), &format!("vertical-r{i}")).await.0);
    }
    for t in &traces[1..] {
        assert_eq!(compare_behavior(&traces[0], t), Ok(()));
    }
}

#[tokio::test]
async fn finalized_bundle_inspects_clean() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    scenario(addr, runs.path(), "vertical-b").await;
    let b = Bundle::open(runs.path(), &RunId::new("vertical-b").unwrap()).unwrap();
    let i = b.inspect().unwrap();
    assert!(i.finalized && i.mismatched.is_empty() && i.unexpected.is_empty(), "{i:?}");
    let _ = VirtualTime::ZERO;
}
