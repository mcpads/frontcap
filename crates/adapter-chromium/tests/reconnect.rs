mod support;

use frontcap_chromium::generation::{Generation, ReconnectOptions};
use frontcap_core::event::{EventKind, SampleValue, TaintSource};
use frontcap_core::ids::{RunId, WatchName};
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::{Locator, Predicate, WatchRegistry};
use frontcap_core::rules::RuleSet;
use frontcap_core::time::{VirtualDuration, VirtualTime};

/// frontcap's patched engine: a budget's end pauses virtual time even with no session left (engine patch 6),
/// so nothing runs away while the recorder is gone.
fn patched() -> bool {
    browser_pin::engine::patched_binary(&support::repo_root(), browser_pin::current_platform().unwrap()).unwrap().is_some()
}

fn late() -> (WatchName, Predicate) {
    (WatchName::new("late").unwrap(), Predicate::Attached { target: Locator::Css { selector: ".late".into() } })
}

async fn launched(runs: &std::path::Path, profile_dir: &support::DetachedProfile, run: &str) -> Generation {
    let addr = support::serve();
    let mut opts = support::options(runs, run, format!("http://{addr}/appear.html"), RuleSet::default());
    profile_dir.apply(&mut opts);
    let mut g = Generation::launch(opts).await.unwrap();
    let (name, predicate) = late();
    g.watch_add(name, predicate).await.unwrap();
    g
}

fn reconnect_options(runs: &std::path::Path, run: &str, g_launch: frontcap_core::ids::LaunchId, endpoint: frontcap_chromium::generation::Endpoint) -> ReconnectOptions {
    let (name, predicate) = late();
    let mut watches = WatchRegistry::default();
    watches.add(name, predicate).unwrap();
    ReconnectOptions {
        repo_root: support::repo_root(),
        runs_root: runs.to_path_buf(),
        run: RunId::new(run).unwrap(),
        launch_id: g_launch,
        profile: support::options(runs, run, String::new(), RuleSet::default()).profile,
        endpoint,
        watches,
        watchdog: std::time::Duration::from_secs(10),
        granted_until: None, dialog_rules: Default::default(),
        upstream: None,
    }
}

#[tokio::test]
async fn reconnect_continues_time_watches_and_sequence_numbers() {
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let mut g = launched(runs.path(), &profile_dir, "rc-1").await;
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    let endpoint = g.endpoint();
    let launch = g.launch_id().clone();
    let trace = g.trace_path();
    let before = BehaviorLog::read(&trace).unwrap().len();
    g.abandon();

    let (mut g, reconnected) = Generation::reconnect(reconnect_options(runs.path(), "rc-1", launch, endpoint)).await.unwrap();
    assert!(reconnected.affected.is_empty(), "{:?}", reconnected.affected);
    assert_eq!(g.now(), VirtualTime(1_000_000));
    let (name, _) = late();
    g.until(&name, VirtualDuration::from_ms(1000)).await.unwrap();
    assert_eq!(g.now(), VirtualTime(1_234_000), "the watch registered before the fault still stops the clock");
    let manifest = g.stop().await.unwrap();
    assert_eq!(manifest.finalization, frontcap_core::bundle::Finalization::Finalized);
    let events = BehaviorLog::read(&trace).unwrap();
    assert!(events.len() > before);
    assert!(events.windows(2).all(|w| w[1].seq == w[0].seq + 1), "sequence numbers continue without reuse");
    assert!(events.windows(2).all(|w| w[0].t_us <= w[1].t_us));
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Taint(t) if t.source == TaintSource::RecorderFault)));
}

#[tokio::test]
async fn samples_reported_while_disconnected_are_replayed_once() {
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let g = launched(runs.path(), &profile_dir, "rc-2").await;
    let launch = g.launch_id().clone();
    // Grant 2000 ms and drop the connection before any of the budget's reports are read.
    let endpoint = g.advance_and_abandon(VirtualDuration::from_ms(2000)).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let options = ReconnectOptions { granted_until: Some(VirtualTime(2_000_000)), ..reconnect_options(runs.path(), "rc-2", launch, endpoint) };
    let (mut g, reconnected) = Generation::reconnect(options).await.unwrap();
    assert!(reconnected.replayed_samples >= 1);
    // The patched shared controller suspends publication on disconnect. The
    // baseline still reports elapsed time beyond its interrupted budget.
    if patched() {
        // Losing the shared controller retains a native publication hold. The
        // endpoint survives, but reaching it requires an explicit new grant.
        assert_eq!(reconnected.ran_past, None);
        let stopped = g.now();
        assert!(stopped <= VirtualTime(2_000_000));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(g.evaluate("Date.now()").await.unwrap(), serde_json::json!(support::options(runs.path(), "epoch", String::new(), RuleSet::default()).profile.initial_date_ms + stopped.0 / 1000));
        g.advance(VirtualDuration::from_micros(2_000_000 - stopped.0)).await.unwrap();
        assert_eq!(g.now(), VirtualTime(2_000_000));
    } else {
        let (granted, reached) = reconnected.ran_past.expect("the runaway is reported");
        assert!(reached > granted);
    }
    let trace = g.trace_path();
    g.stop().await.unwrap();
    let events = BehaviorLog::read(&trace).unwrap();
    let samples: Vec<_> = events.iter().filter_map(|e| match &e.kind { EventKind::Watch(s) => Some((e.t_us, s.clone())), _ => None }).collect();
    let first_true = samples.iter().find(|(_, s)| s.value == SampleValue::True).expect("the insertion was observed");
    assert_eq!(first_true.0, VirtualTime(1_234_000));
    let rseqs: Vec<u64> = samples.iter().map(|(_, s)| s.rseq).filter(|r| *r > 0).collect();
    assert!(rseqs.windows(2).all(|w| w[0] < w[1]), "no sample recorded twice: {rseqs:?}");
}

/// The bound a runaway is measured against is the budget granted to the engine, not the interrupted
/// operation's goal: a fault in a short segment of a long wait still reports the excess.
#[tokio::test]
async fn a_runaway_below_the_operations_goal_is_reported() {
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let g = launched(runs.path(), &profile_dir, "rc-run").await;
    let launch = g.launch_id().clone();
    let endpoint = g.advance_and_abandon(VirtualDuration::from_ms(2000)).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let goal = VirtualTime(3_600_000_000_000); // a thousand hours away
    let options = ReconnectOptions { granted_until: Some(goal), ..reconnect_options(runs.path(), "rc-run", launch, endpoint) };
    let (mut g, reconnected) = Generation::reconnect(options).await.unwrap();
    if patched() {
        // Losing the shared controller retains a native publication hold. The
        // endpoint survives, but reaching it requires an explicit new grant.
        assert_eq!(reconnected.ran_past, None);
        let stopped = g.now();
        assert!(stopped <= VirtualTime(2_000_000));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(g.evaluate("Date.now()").await.unwrap(), serde_json::json!(support::options(runs.path(), "epoch", String::new(), RuleSet::default()).profile.initial_date_ms + stopped.0 / 1000));
        g.advance(VirtualDuration::from_micros(2_000_000 - stopped.0)).await.unwrap();
        assert_eq!(g.now(), VirtualTime(2_000_000));
    } else {
        let (granted, reached) = reconnected.ran_past.expect("the runaway is reported");
        assert_eq!(granted, VirtualTime(2_000_000));
        assert!(reached > granted && reached < goal);
    }
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_fault_at_the_start_boundary_keeps_the_timeline() {
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let mut g = launched(runs.path(), &profile_dir, "rc-3").await;
    for _ in 0..2 {
        let endpoint = g.endpoint();
        let target = endpoint.target_id.clone();
        let launch = g.launch_id().clone();
        if patched() {
            assert_eq!(g.evaluate("document.readyState").await.unwrap(), "loading");
        }
        g.abandon(); // The committed document has not run its first script.
        (g, _) = Generation::reconnect(reconnect_options(runs.path(), "rc-3", launch, endpoint)).await.unwrap();
        if patched() {
            assert_eq!(g.endpoint().target_id, target);
            assert_eq!(g.now(), VirtualTime::ZERO);
            assert_eq!(g.evaluate("document.readyState").await.unwrap(), "loading");
        }
    }
    let (name, _) = late();
    g.until(&name, VirtualDuration::from_ms(2000)).await.unwrap();
    assert_eq!(g.now(), VirtualTime(1_234_000), "the same instant as without the fault");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn the_same_page_without_a_fault() {
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let mut g = launched(runs.path(), &profile_dir, "rc-4").await;
    let (name, _) = late();
    g.until(&name, VirtualDuration::from_ms(2000)).await.unwrap();
    assert_eq!(g.now(), VirtualTime(1_234_000));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_watch_already_true_before_the_fault_stops_until_at_once() {
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let mut g = launched(runs.path(), &profile_dir, "rc-5").await;
    g.advance(VirtualDuration::from_ms(1500)).await.unwrap(); // `.late` appeared at 1234
    let endpoint = g.endpoint();
    let launch = g.launch_id().clone();
    g.abandon();
    let (mut g, _) = Generation::reconnect(reconnect_options(runs.path(), "rc-5", launch, endpoint)).await.unwrap();
    let (name, _) = late();
    g.until(&name, VirtualDuration::from_ms(1000)).await.unwrap();
    assert_eq!(g.now(), VirtualTime(1_500_000), "the condition already holds; no time passes");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_stop_whose_finalization_fails_still_ends_the_browser() {
    use std::os::unix::fs::PermissionsExt;
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let g = launched(runs.path(), &profile_dir, "rc-6").await;
    let browser = frontcap_core::process::ProcessIdentity::of(g.endpoint().browser_pid).unwrap().unwrap();
    let root = g.trace_path().parent().unwrap().to_path_buf();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
    let result = g.stop().await;
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(result.is_err(), "finalization cannot write the manifest");
    assert!(!browser.is_alive(), "the browser is ended even when finalization fails");
}

#[tokio::test]
async fn reconnect_restores_the_determinism_profile() {
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let mut g = launched(runs.path(), &profile_dir, "rc-7").await;
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let read = "JSON.stringify([Intl.DateTimeFormat().resolvedOptions().timeZone, navigator.language, innerWidth, innerHeight])";
    let before = g.eval_main(read).await.unwrap();
    let endpoint = g.endpoint();
    let launch = g.launch_id().clone();
    g.abandon();
    let (g, _) = Generation::reconnect(reconnect_options(runs.path(), "rc-7", launch, endpoint)).await.unwrap();
    assert_eq!(g.eval_main(read).await.unwrap(), before, "the profile survives a dropped connection");
    assert_eq!(before, r#"["UTC","en-US",1280,720]"#);
    g.stop().await.unwrap();
}
