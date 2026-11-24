mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::event::{ClockOutcome, EventKind};
use frontcap_core::log::BehaviorLog;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::{VirtualDuration, VirtualTime};
use std::time::Instant;

#[tokio::test]
async fn launch_returns_frozen_at_zero_and_advance_is_exact() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "clock-1", format!("http://{addr}/timer.html"), RuleSet::default()))
        .await
        .unwrap();
    assert_eq!(g.now(), VirtualTime::ZERO);
    let wall = Instant::now();
    let outcome = g.advance(VirtualDuration::from_ms(60_001)).await.unwrap();
    assert_eq!(outcome, ClockOutcome::Reached);
    assert_eq!(g.now(), VirtualTime(60_001_000));
    assert!(wall.elapsed().as_secs() < 5, "60 s of virtual time took {:?}", wall.elapsed());
    assert_eq!(g.eval_main("document.title").await.unwrap(), "fired");
    let fired = g.eval_main("window.__fired").await.unwrap().as_u64().unwrap();
    assert_eq!(fired - 946_684_800_000, 60_000);
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.iter().any(|e| matches!(e.kind, EventKind::Navigation(_))));
    assert!(events.windows(2).all(|w| w[0].t_us <= w[1].t_us));
}

#[tokio::test]
async fn busy_loop_trips_the_watchdog() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut opts = support::options(runs.path(), "clock-2", format!("http://{addr}/busy.html"), RuleSet::default());
    opts.watchdog = std::time::Duration::from_secs(2);
    let mut g = Generation::launch(opts).await.unwrap();
    let wall = Instant::now();
    let err = g.advance(VirtualDuration::from_ms(1000)).await.unwrap_err();
    assert!(matches!(err, frontcap_chromium::ChromiumError::Unresponsive { .. }), "{err:?}");
    assert!(wall.elapsed().as_secs() < 6);
    let again = Instant::now();
    let err = g.advance(VirtualDuration::from_ms(1)).await.unwrap_err();
    assert!(matches!(err, frontcap_chromium::ChromiumError::Unresponsive { .. }), "{err:?}");
    assert!(again.elapsed().as_millis() < 100, "a dead generation answers at once");
}
