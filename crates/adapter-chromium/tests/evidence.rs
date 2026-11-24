//! Evidence the trace must hold regardless of what the page does (whole-project review F01, F03, F04).
mod support;

use frontcap_chromium::generation::{Generation, ReconnectOptions};
use frontcap_core::event::{ClockOutcome, EventKind, SampleValue};
use frontcap_core::ids::{RunId, WatchName};
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::{Locator, Predicate, WatchRegistry};
use frontcap_core::rules::RuleSet;
use frontcap_core::time::{VirtualDuration, VirtualTime};

fn second() -> (WatchName, Predicate) {
    (WatchName::new("second").unwrap(), Predicate::Attached { target: Locator::Css { selector: "#second".into() } })
}

fn true_samples_after(g: &Generation, name: &WatchName, from_us: u64) -> Vec<u64> {
    BehaviorLog::read(&g.trace_path())
        .unwrap()
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Watch(s) if &s.name == name && s.value == SampleValue::True && e.t_us.0 >= from_us => Some(e.t_us.0),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn samples_of_a_new_document_are_kept() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "ev-1", format!("http://{addr}/nav.html"), RuleSet::default())).await.unwrap();
    let (name, predicate) = second();
    g.watch_add(name.clone(), predicate).await.unwrap();
    let out = g.until(&name, VirtualDuration::from_ms(5000)).await.unwrap();
    assert_eq!(out, ClockOutcome::ConditionMet { watch: name.clone() });
    assert_eq!(true_samples_after(&g, &name, 500_000).first(), Some(&800_000), "the destination's true sample is in the trace");
}

#[tokio::test]
async fn reconnect_after_a_navigation_uses_the_current_documents_cursor() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let mut opts = support::options(runs.path(), "ev-2", format!("http://{addr}/nav.html"), RuleSet::default());
    profile_dir.apply(&mut opts);
    let profile = opts.profile.clone();
    let mut g = Generation::launch(opts).await.unwrap();
    let (name, predicate) = second();
    g.watch_add(name.clone(), predicate.clone()).await.unwrap();
    g.advance(VirtualDuration::from_ms(600)).await.unwrap(); // navigated at 500; `#second` appears at 800
    let launch = g.launch_id().clone();
    let endpoint = g.advance_and_abandon(VirtualDuration::from_ms(400)).await.unwrap();
    // Shared controller loss freezes publication. Produce the unread report
    // explicitly while the recorder is absent, then recover its document cursor.
    support::shared_time::progress_without_recorder(&endpoint, VirtualTime(800_000), profile.initial_date_ms).await;
    let mut watches = WatchRegistry::default();
    watches.add(name.clone(), predicate).unwrap();
    let (g, _) = Generation::reconnect(ReconnectOptions {
        repo_root: support::repo_root(),
        runs_root: runs.path().to_path_buf(),
        run: RunId::new("ev-2").unwrap(),
        launch_id: launch,
        profile,
        endpoint,
        watches,
        watchdog: std::time::Duration::from_secs(10),
        granted_until: None, dialog_rules: Default::default(),
        upstream: None,
    })
    .await
    .unwrap();
    assert_eq!(true_samples_after(&g, &name, 500_000).first(), Some(&800_000), "replayed from the current document");
    g.stop().await.unwrap();
}

/// A cursor restored on reconnect belongs to the current document: one carried over from an earlier
/// document would discard the current observer's lower-numbered reports.
#[tokio::test]
async fn reconnect_does_not_carry_an_earlier_documents_cursor() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let mut opts = support::options(runs.path(), "ev-5", format!("http://{addr}/churn.html"), RuleSet::default());
    profile_dir.apply(&mut opts);
    let profile = opts.profile.clone();
    let mut g = Generation::launch(opts).await.unwrap();
    let flip = WatchName::new("flip").unwrap();
    g.watch_add(flip.clone(), Predicate::Attached { target: Locator::Css { selector: ".odd".into() } }).await.unwrap();
    g.advance(VirtualDuration::from_ms(450)).await.unwrap(); // 40 changes of `flip` in the first document
    g.watch_remove(&flip).await.unwrap();
    g.advance(VirtualDuration::from_ms(150)).await.unwrap(); // navigated at 500; `#second` appears at 800
    let launch = g.launch_id().clone();
    let endpoint = g.endpoint();
    g.abandon();
    let (mut g, _) = Generation::reconnect(ReconnectOptions {
        repo_root: support::repo_root(),
        runs_root: runs.path().to_path_buf(),
        run: RunId::new("ev-5").unwrap(),
        launch_id: launch,
        profile,
        endpoint,
        watches: WatchRegistry::default(),
        watchdog: std::time::Duration::from_secs(10),
        granted_until: None, dialog_rules: Default::default(),
        upstream: None,
    })
    .await
    .unwrap();
    let (name, predicate) = second();
    g.watch_add(name.clone(), predicate).await.unwrap();
    let out = g.until(&name, VirtualDuration::from_ms(1000)).await.unwrap();
    assert_eq!(out, ClockOutcome::ConditionMet { watch: name.clone() });
    assert_eq!(g.now(), VirtualTime(800_000));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_page_cannot_forge_a_condition_stop() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "ev-3", format!("http://{addr}/appear.html"), RuleSet::default())).await.unwrap();
    let never = WatchName::new("never").unwrap();
    g.watch_add(never.clone(), Predicate::Attached { target: Locator::Css { selector: "#absent".into() } }).await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    g.eval_main("setTimeout(() => fetch('https://frontcap.invalid/stop/never').catch(() => {}), 100)").await.unwrap();
    let out = g.until(&never, VirtualDuration::from_ms(1000)).await.unwrap();
    assert_eq!(out, ClockOutcome::ConditionTimedOut { watch: never });
    assert_eq!(g.now(), VirtualTime(1_001_000));
}

#[tokio::test]
async fn crypto_randomness_follows_the_seed() {
    async fn values(seed: u64, run: &str) -> String {
        let addr = support::serve();
        let runs = tempfile::tempdir().unwrap();
        let mut opts = support::options(runs.path(), run, format!("http://{addr}/appear.html"), RuleSet::default());
        opts.profile.random_seed = seed;
        let mut g = Generation::launch(opts).await.unwrap();
        g.advance(VirtualDuration::from_ms(1)).await.unwrap();
        let v = g.eval_main("JSON.stringify([Array.from(crypto.getRandomValues(new Uint32Array(4))), crypto.randomUUID(), Array.from(crypto.getRandomValues(new Uint8Array(3)))])").await.unwrap();
        v.as_str().unwrap().to_string()
    }
    let a = values(7, "ev-4a").await;
    let b = values(7, "ev-4b").await;
    let c = values(8, "ev-4c").await;
    assert_eq!(a, b, "the same seed gives the same values");
    assert_ne!(a, c, "another seed gives other values");
    let v: serde_json::Value = serde_json::from_str(&a).unwrap();
    let uuid = v[1].as_str().unwrap();
    assert_eq!(uuid.len(), 36);
    assert_eq!(&uuid[14..15], "4", "a version 4 UUID: {uuid}");
    assert!(matches!(&uuid[19..20], "8" | "9" | "a" | "b"), "{uuid}");
}

async fn unseeded_taints(call: &str, run: &str) -> Vec<(u64, String)> {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/unseeded.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    g.eval_main(call).await.unwrap();
    g.advance(VirtualDuration::from_ms(200)).await.unwrap();
    let trace = g.trace_path();
    g.stop().await.unwrap();
    BehaviorLog::read(&trace)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.kind {
            EventKind::Taint(t) if t.source == frontcap_core::event::TaintSource::UnseededRandomness => Some((e.t_us.0, t.detail)),
            _ => None,
        })
        .collect()
}

/// Randomness the profile's seed does not reach is recorded as taint, at the instant the page called for
/// it: a worker's realm, once, when the page constructed it, and the Web Crypto operations that draw
/// their own random numbers.
#[tokio::test]
async fn randomness_outside_the_seed_is_taint() {
    assert_eq!(unseeded_taints("void 0", "ev-u0").await, Vec::new());
    let worker = unseeded_taints("worker()", "ev-u1").await;
    assert_eq!(worker.len(), 1, "{worker:?}");
    assert_eq!(worker[0].0, 10_000, "{worker:?}");
    assert!(worker[0].1.starts_with("worker blob:http://"), "named as the browser names its target: {worker:?}");
    let subtle = unseeded_taints("subtle()", "ev-u2").await;
    assert_eq!(subtle.iter().map(|(t, _)| *t).collect::<Vec<_>>(), [10_000], "crypto.subtle.generateKey: {subtle:?}");
}

/// Browser-side worker script announcements cannot outrun the constructor's renderer evidence.
#[tokio::test]
async fn a_workers_construction_taint_precedes_its_script_request() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"page","match":{"url":"https://workers.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><script>fetch('/before');for(let i=0;i<8;i++)new Worker('/worker-'+i+'.js');fetch('/after')</script>"}},
        {"id":"worker","match":{"url":"https://workers.example.test/worker-*"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/javascript"},"body":"postMessage('ready')"}}
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "worker-construction", "https://workers.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    for i in 0..8 {
        let url = format!("https://workers.example.test/worker-{i}.js");
        let taint = events.iter().position(|e| matches!(&e.kind, EventKind::Taint(t) if t.detail == format!("worker {url}"))).unwrap();
        let request = events.iter().position(|e| matches!(&e.kind, EventKind::Request(r) if r.url == url)).unwrap();
        assert!(taint < request, "{url}: constructor at {taint}, script request at {request}");
        assert_eq!(events[taint].t_us, VirtualTime::ZERO);
    }
    g.stop().await.unwrap();
}

/// A renderer that crashes answers nothing: every later command fails at once as a crash (not after the
/// call timeout, as an internal fault), and stopping still finalizes the evidence, which ends with the
/// crash.
#[tokio::test]
async fn a_renderer_crash_fails_commands_at_once_and_its_evidence_is_finalized() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "ev-crash", format!("http://{addr}/cover.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.crash_renderer().unwrap();
    let started = std::time::Instant::now();
    let read = g.query(&Locator::Css { selector: "body".into() }, &[]).await;
    assert!(matches!(read, Err(frontcap_chromium::ChromiumError::RendererCrashed)), "{:?}", read.map(|_| ()));
    let moved = g.advance(VirtualDuration::from_ms(50)).await;
    assert!(matches!(moved, Err(frontcap_chromium::ChromiumError::RendererCrashed)), "{moved:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    let trace = g.trace_path();
    let manifest = g.stop().await.unwrap();
    assert!(manifest.determinism.taints.iter().any(|t| t.source == frontcap_core::event::TaintSource::RendererCrashed), "{manifest:?}");
    let last = BehaviorLog::read(&trace).unwrap().pop().unwrap();
    assert!(matches!(&last.kind, EventKind::Taint(t) if t.source == frontcap_core::event::TaintSource::RendererCrashed), "{last:?}");
}

/// The browser's own console messages are the page's evidence too, at their virtual instants and with
/// their source: a frame refused by `X-Frame-Options` (which shows an empty error page, with nothing
/// else to say why) and a resource the server did not find.
#[tokio::test]
async fn the_browsers_own_messages_are_recorded_with_their_source() {
    let page = r#"<!doctype html><button id=go onclick="fetch('https://cdn.example.test/lib.js').catch(() => {})">go</button><iframe src="https://deny.example.test/"></iframe>"#;
    let mock = |id: &str, url: &str, headers: serde_json::Value, body: &str| serde_json::json!({"id": id, "match": {"url": url}, "action": {"type": "mock", "status": 200, "headers": headers, "body": body}});
    let rules = RuleSet::from_json(&serde_json::json!([
        mock("app", "https://app.example.test/", serde_json::json!({"content-type": "text/html"}), page),
        mock("deny", "https://deny.example.test/", serde_json::json!({"content-type": "text/html", "x-frame-options": "DENY"}), "<p>denied</p>"),
        serde_json::json!({"id": "missing", "match": {"url": "https://cdn.example.test/lib.js"}, "action": {"type": "mock", "status": 404, "body": "not found"}}),
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "ev-log", "https://app.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.click(frontcap_core::ids::RequestId::new("c-1").unwrap(), &Locator::Css { selector: "#go".into() }).await.unwrap();
    let clicked = g.now();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    let browser: Vec<(u64, String, String, String)> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Console(c) => c.source.clone().map(|s| (e.t_us.0, s, c.level.clone(), c.text.clone())),
        _ => None,
    }).collect();
    let refused = browser.iter().find(|(_, _, _, text)| text.contains("X-Frame-Options")).unwrap_or_else(|| panic!("{browser:?}"));
    assert!(refused.0 <= 50_000 && refused.2 == "error", "{refused:?}");
    let failed = browser.iter().find(|(_, _, _, text)| text.contains("Failed to load resource")).unwrap_or_else(|| panic!("{browser:?}"));
    assert_eq!((failed.0, failed.1.as_str(), failed.2.as_str()), (clicked.0, "network", "error"), "at the request's failure: {failed:?}");
    g.stop().await.unwrap();
}
