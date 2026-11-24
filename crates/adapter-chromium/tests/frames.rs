mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::event::EventKind;
use frontcap_core::ids::WatchName;
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::{Locator, Predicate};
use frontcap_core::rules::RuleSet;
use frontcap_core::time::{VirtualDuration, VirtualTime};
use frontcap_core::event::ClockOutcome;

async fn launched(run: &str, page: &str) -> (tempfile::TempDir, Generation) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/{page}"), RuleSet::default())).await.unwrap();
    (runs, g)
}

fn json(v: serde_json::Value) -> serde_json::Value {
    serde_json::from_str(v.as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn animation_frames_run_once_per_frame_tick() {
    let (_runs, mut g) = launched("frames-1", "frames.html").await;
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    let frames: Vec<u64> = serde_json::from_value(json(g.eval_main("JSON.stringify(window.__frames)").await.unwrap())).unwrap();
    assert!((59..=60).contains(&frames.len()), "{frames:?}");
    assert!(frames.windows(2).all(|w| (16..=17).contains(&(w[1] - w[0]))), "{frames:?}");
}

/// Opacity of the transitioning box 134 ms after the transition was requested, advancing in `slices`.
async fn transition_midpoint(run: &str, slices: &[u64]) -> (String, Vec<String>) {
    let (_runs, mut g) = launched(run, "frames.html").await;
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    g.eval_main("startTransition()").await.unwrap();
    for ms in slices {
        g.advance(VirtualDuration::from_ms(*ms)).await.unwrap();
    }
    let mid = g.eval_main("getComputedStyle(document.getElementById('box')).opacity").await.unwrap().as_str().unwrap().to_string();
    g.advance(VirtualDuration::from_ms(500)).await.unwrap();
    let events = json(g.eval_main("JSON.stringify(window.__events)").await.unwrap());
    (mid, events.as_array().unwrap().iter().map(|e| format!("{}@{}", e[0].as_str().unwrap(), e[1])).collect())
}

#[tokio::test]
async fn transition_values_follow_virtual_time() {
    let (mid, events) = transition_midpoint("frames-2", &[134]).await;
    let opacity: f64 = mid.parse().unwrap();
    assert!(opacity > 0.3 && opacity < 0.8, "{mid}");
    // An animation starts, and its events are dispatched, at the next native rendering frame: within one
    // frame interval of the virtual instant at level `time`, exact only at level `frame`.
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events[0].starts_with("transitionrun@") && events[1].starts_with("transitionend@"), "{events:?}");
    let (sliced, _) = transition_midpoint("frames-2b", &[5, 29, 16, 1, 50, 33]).await;
    let sliced: f64 = sliced.parse().unwrap();
    assert!((sliced - opacity).abs() <= 16.667 / 200.0 + 1e-9, "{sliced} vs {opacity}");
}

#[tokio::test]
async fn page_clock_override_does_not_move_the_recorder_clock() {
    let (_runs, mut g) = launched("frames-3", "clock-override.html").await;
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    assert_eq!(g.now(), VirtualTime::ZERO.saturating_add(VirtualDuration::from_ms(1000)));
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.windows(2).all(|w| w[0].t_us <= w[1].t_us));
    let console = events.iter().find(|e| matches!(&e.kind, EventKind::Console(_))).expect("console record");
    assert_eq!(console.t_us, VirtualTime::ZERO.saturating_add(VirtualDuration::from_ms(300)));
}

#[tokio::test]
async fn navigation_mid_run_keeps_time_and_watches() {
    let (_runs, mut g) = launched("frames-4", "nav.html").await;
    let second = WatchName::new("second").unwrap();
    g.watch_add(second.clone(), Predicate::Attached { target: Locator::Css { selector: "#second".into() } }).await.unwrap();
    let outcome = g.until(&second, VirtualDuration::from_ms(5000)).await.unwrap();
    assert_eq!(outcome, ClockOutcome::ConditionMet { watch: second.clone() });
    let now = g.now().0;
    assert!((800_000..900_000).contains(&now), "{now}");
    let events = BehaviorLog::read(&g.trace_path()).unwrap();
    assert!(events.windows(2).all(|w| w[0].t_us <= w[1].t_us));
    let navs: Vec<u64> = events.iter().filter_map(|e| match &e.kind { EventKind::Navigation(_) => Some(e.t_us.0), _ => None }).collect();
    assert_eq!(navs.len(), 2, "{events:?}");
    assert_eq!(navs[1], 500_000);
}


/// Animation-frame callbacks, measured against the pinned Chromium without frontcap: the order of the
/// callbacks and of the microtasks they queue, a microtask that cancels a later callback, and one that
/// changes what a later callback reads. Every callback is followed by a microtask checkpoint, as in a
/// browser.
#[tokio::test]
async fn animation_frames_keep_the_browsers_microtask_checkpoints() {
    use cdp::protocol::{runtime, target};
    // The baseline keeps its replaced animation frames (time-model.md): only the patched engine's are native.
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    const PROBE: &str = r#"(() => {
        const order = [];
        let flag = "unset";
        const a = requestAnimationFrame(() => { order.push("first"); queueMicrotask(() => { order.push("microtask"); flag = "set"; cancelAnimationFrame(c); }); });
        requestAnimationFrame(() => { order.push("second:" + flag); });
        const c = requestAnimationFrame(() => { order.push("cancelled one ran"); });
        requestAnimationFrame(() => { Promise.resolve().then(() => order.push("promise")); order.push("fourth"); });
        void a;
        return new Promise((resolve) => setTimeout(() => resolve(JSON.stringify(order)), 200));
    })()"#;
    let root = support::repo_root();
    let pin = browser_pin::load_pin(&root).unwrap();
    let native_binary = browser_pin::install(&root, &pin, browser_pin::current_platform().unwrap()).unwrap();
    let browser = browser_pin::launch(&native_binary, &[], std::time::Duration::from_secs(20)).unwrap();
    let client = cdp::Client::connect(&browser.browser_ws_url).await.unwrap();
    let targets = client.call(None, &target::GetTargetsParams::default()).await.unwrap();
    let page = targets.target_infos.into_iter().find(|t| t.r#type == "page").unwrap();
    let session = client.call(None, &target::AttachToTargetParams { flatten: Some(true), ..target::AttachToTargetParams::new(page.target_id) }).await.unwrap().session_id;
    let native = client
        .call(Some(&session.0), &runtime::EvaluateParams { await_promise: Some(true), return_by_value: Some(true), ..runtime::EvaluateParams::new(PROBE.into()) })
        .await
        .unwrap()
        .result
        .value
        .unwrap();
    drop(browser);
    assert_eq!(native, r#"["first","microtask","second:set","fourth","promise"]"#, "the browser's own order");
    let (_runs, mut g) = launched("frames-native", "frames.html").await;
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g.eval_main(&format!("window.probe = {PROBE}; 0")).await.unwrap();
    g.advance(VirtualDuration::from_ms(300)).await.unwrap();
    assert_eq!(g.eval_main("window.probe").await.unwrap(), native);
}

/// Idle callbacks run at the frame tick after their request, each a task of its own: a microtask one queues
/// runs before the next callback.
#[tokio::test]
async fn idle_callbacks_run_at_the_next_tick_each_on_its_own() {
    let (_runs, mut g) = launched("frames-idle", "frames.html").await;
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g.eval_main(r#"window.idle = []; const at = () => Date.now() - 946684800000;
        requestIdleCallback(() => { idle.push("first@" + at()); queueMicrotask(() => idle.push("microtask")); });
        requestIdleCallback(() => idle.push("second@" + at())); 0"#).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    // Requested at 20: the next tick is 34.
    assert_eq!(g.eval_main("JSON.stringify(idle)").await.unwrap(), r#"["first@34","microtask","second@34"]"#);
}
