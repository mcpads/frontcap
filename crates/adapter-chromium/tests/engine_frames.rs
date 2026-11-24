//! Controlled frame delivery: the renderer's compositor runs frames by the real clock, and the platform decides when
//! the page renders — at frame ticks, and while input is dispatched. Input that needs a frame is handled at
//! its instant while virtual time is paused (S14), and animations start and dispatch their events at frame
//! ticks for any slicing of time.
mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::action::Action;
use frontcap_core::ids::RequestId;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

#[tokio::test]
async fn a_move_after_a_click_is_handled_at_the_same_frozen_instant() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "frames", format!("http://{addr}/wheel_frozen.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let before = g.now();
    let button = Locator::Css { selector: "#b".into() };
    g.click(RequestId::new("c-1").unwrap(), &button).await.unwrap();
    // S14: on the unpatched engine the renderer had no frame left for a move after a click at one instant.
    g.act_input(RequestId::new("h-1").unwrap(), &Action::Hover { target: Locator::Css { selector: "#b".into() } }).await.unwrap();
    let events: Vec<String> = serde_json::from_str(g.eval_main("JSON.stringify(events)").await.unwrap().as_str().unwrap()).unwrap();
    // The click moves the mouse to the button first; the hover's move is the last one.
    let at = |name: &str| events.iter().rposition(|e| e == name);
    assert!(at("click").is_some() && at("pointermove") > at("click"), "the move after the click: {events:?}");
    assert_eq!(g.now(), before, "frozen throughout");
}

/// The transition's events: started two frames after the call, ended 200 ms later.
async fn transition(run: &str, slices: &[u64]) -> String {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/frames.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    g.eval_main("startTransition()").await.unwrap();
    for ms in slices {
        g.advance(VirtualDuration::from_ms(*ms)).await.unwrap();
    }
    g.eval_main("JSON.stringify(window.__events)").await.unwrap().as_str().unwrap().to_string()
}

#[tokio::test]
async fn animation_events_land_on_frame_ticks_for_any_slicing() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let one = transition("tr-1", &[600]).await;
    assert_eq!(one, transition("tr-2", &[5, 29, 16, 1, 50, 33, 7, 300, 159]).await, "the same for any slicing");
    assert_eq!(one, transition("tr-3", &[1; 600]).await);
    let v: Vec<(String, u64)> = serde_json::from_str(&one).unwrap();
    // Frame ticks are multiples of 16.667 ms rounded up to whole milliseconds.
    let ticks: Vec<u64> = (1..60).map(|k: u64| (k * 16_667).div_ceil(1000)).collect();
    assert!(v.iter().all(|(_, t)| ticks.contains(t)), "{one}");
    assert_eq!(v.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["transitionrun", "transitionend"]);
}

/// CPU seconds used so far by the browser and every process under it (renderers, GPU process, and on Linux
/// the zygote's children).
fn browser_cpu(browser: u32) -> f64 {
    let out = std::process::Command::new("ps").args(["-Ao", "pid=,ppid=,time="]).output().unwrap();
    let rows: Vec<(u32, u32, f64)> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (pid, ppid) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?);
            // [[dd-]hh:]mm:ss.cc
            let cpu = f.next()?.split([':', '-']).fold(0.0, |acc, p| acc * 60.0 + p.parse::<f64>().unwrap_or(0.0));
            Some((pid, ppid, cpu))
        })
        .collect();
    let mut tree = vec![browser];
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i];
        tree.extend(rows.iter().filter(|r| r.1 == parent).map(|r| r.0));
        i += 1;
    }
    rows.iter().filter(|r| tree.contains(&r.0)).map(|r| r.2).sum()
}

#[tokio::test]
async fn an_idle_page_costs_no_cpu_between_commands() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "idle", format!("http://{addr}/wheel_frozen.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let before = browser_cpu(g.browser_pid());
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    // Full-pipeline compositors (the renderer's and the browser's) subscribed to back-to-back begin frames,
    // and viz waiting on a tab nothing rendered, each spun about a core: 3 s or more here even under load.
    // Idle, a few hundredths.
    let used = browser_cpu(g.browser_pid()) - before;
    assert!(used < 1.0, "{used} s of CPU in 3 s idle");
}

fn ticks() -> Vec<u64> {
    // Frame ticks are multiples of 16.667 ms rounded up to whole milliseconds.
    (1..2000).map(|k: u64| (k * 16_667).div_ceil(1000)).collect()
}

/// The transition's events on the page without an animation frame loop, after `setup` and the advances.
async fn idle_transition(run: &str, setup: &str, slices: &[u64]) -> Vec<(String, u64)> {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(r#"[{"id":"go","match":{"url":"https://api.example.test/go"},"action":{"type":"mock","status":200,"body":"g","latency_ms":50}}]"#).unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/frames_idle.html"), rules)).await.unwrap();
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    g.eval_main(setup).await.unwrap();
    for ms in slices {
        g.advance(VirtualDuration::from_ms(*ms)).await.unwrap();
    }
    serde_json::from_str(g.eval_main("JSON.stringify(window.__events)").await.unwrap().as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn idle_ticks_skipped_do_not_move_what_a_timer_or_a_response_starts() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // A timer: the ticks before it are skipped in one advance, stopped at in many.
    let one = idle_transition("idle-1", "later(500)", &[1000]).await;
    assert_eq!(one, idle_transition("idle-2", "later(500)", &[100; 10]).await, "the same for any slicing");
    assert_eq!(one, idle_transition("idle-3", "later(500)", &[7, 300, 193, 13, 487]).await);
    assert!(one.iter().all(|(_, t)| ticks().contains(t)), "{one:?}");
    assert_eq!(one.len(), 2, "{one:?}");
    // A task a tick's timer posts at that instant (set at 17 for 50 ms: 67), after the budget's end.
    let one = idle_transition("idle-6", "viaChannel(50)", &[1000]).await;
    assert_eq!(one, idle_transition("idle-7", "viaChannel(50)", &[1; 400]).await, "the same for any slicing");
    assert_eq!(one, idle_transition("idle-8", "viaChannel(50)", &[50, 950]).await);
    assert!(one.iter().all(|(_, t)| ticks().contains(t)), "{one:?}");
    assert_eq!(one.len(), 2, "{one:?}");
    // A response delivered at a tick (issued at 17, 50 ms latency: 67) starts the transition.
    let one = idle_transition("idle-4", "void fetched()", &[1000]).await;
    assert_eq!(one, idle_transition("idle-5", "void fetched()", &[1; 400]).await, "the same for any slicing");
    assert!(one.iter().all(|(_, t)| ticks().contains(t)), "{one:?}");
    assert_eq!(one.len(), 2, "{one:?}");
}

/// The transition's events in a document a navigation brought (`cross_site`: another site's, in a new
/// renderer), after the advances, with wall time between the operations.
async fn transition_after_navigation(run: &str, cross_site: bool, slices: &[u64], pause_ms: u64) -> Vec<(String, u64)> {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut o = support::options(runs.path(), run, format!("http://{addr}/wheel_frozen.html"), RuleSet::default());
    o.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port())]);
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
    let host = if cross_site { format!("localhost:{}", addr.port()) } else { addr.to_string() };
    g.navigate_action(&RequestId::new("n-1").unwrap(), &Action::Navigate { url: format!("http://{host}/frames_idle.html") }).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
    g.advance(VirtualDuration::from_ms(20)).await.unwrap();
    g.eval_main("start()").await.unwrap();
    for ms in slices {
        g.advance(VirtualDuration::from_ms(*ms)).await.unwrap();
    }
    serde_json::from_str(g.eval_main("JSON.stringify(window.__events)").await.unwrap().as_str().unwrap()).unwrap()
}

#[tokio::test]
async fn a_new_document_renders_at_frame_ticks_only() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // Navigation clock continuity: the new document's time origin and clock follow the virtual clock, in this renderer
    // or another site's, so where the transition's end falls is the same too.
    for cross_site in [false, true] {
        let one = transition_after_navigation("nav-1", cross_site, &[600], 0).await;
        assert_eq!(one, transition_after_navigation("nav-2", cross_site, &[3, 40, 1, 200, 56, 300], 150).await, "the same for any slicing and wall time");
        assert_eq!(one, transition_after_navigation("nav-3", cross_site, &[1; 600], 40).await);
        assert_eq!(one.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), ["transitionrun", "transitionend"], "{one:?}");
        assert!(one.iter().all(|(_, t)| ticks().contains(t)), "cross_site {cross_site}: {one:?}");
    }
}

/// `scroll` while frozen (a native wheel): the page is scrolled and hears `scroll` at the next frame.
#[tokio::test]
async fn a_scroll_while_frozen_scrolls_and_the_page_hears_it() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "wheel", format!("http://{addr}/wheel_frozen.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    g.act_input(RequestId::new("s-1").unwrap(), &Action::Scroll { target: None, delta_x: 0, delta_y: 300 }).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    // The next frame dispatches the page's `scroll` event.
    assert_eq!(g.eval_main("scrollY").await.unwrap(), serde_json::json!(300));
    let events = g.eval_main("JSON.stringify(events)").await.unwrap();
    assert!(events.as_str().unwrap().contains("scroll\""), "{events}");
}

/// A child frame's 200 ms timer, in the page `oopif.html?{query}`: frozen it never fires; it fires by the
/// virtual clock.
async fn child_timer(run: &str, query: &str, origins: Vec<String>) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut o = support::options(runs.path(), run, format!("http://{addr}/oopif.html?{}", query.replace("{port}", &addr.port().to_string())), RuleSet::default());
    o.passthrough_origins = Some([vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port())], origins].concat());
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    // Frozen: a frame on the real clock would fire its 200 ms timer in this second of wall time.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    assert_eq!(g.eval_main("JSON.stringify(got)").await.unwrap(), serde_json::json!("[]"), "{query}");
    g.advance(VirtualDuration::from_ms(400)).await.unwrap();
    let got: Vec<(String, u64)> = serde_json::from_str(g.eval_main("JSON.stringify(got)").await.unwrap().as_str().unwrap()).unwrap();
    assert_eq!(got.len(), 1, "{query}: {got:?}");
    assert!((200..=450).contains(&got[0].1), "{query}: fired by the virtual clock: {got:?}");
}

#[tokio::test]
async fn a_frame_isolation_would_put_elsewhere_runs_by_the_virtual_clock() {
    // Not an engine patch: with site isolation off (`--disable-site-isolation-trials`) every frame shares
    // the page's renderer and its clock.
    child_timer("oopif-site", "", vec![]).await;
    child_timer("oopif-sandbox", "child=oopif_child.html&sandbox", vec![]).await;
    // Same site (an IP), another origin (another port), origin-keyed.
    let other = support::serve();
    child_timer("oopif-oac", &format!("child=http://{other}/oopif_child.html%3Foac"), vec![format!("http://{other}")]).await;
}

/// `scrollY` after a PageDown pressed while frozen and the advances.
async fn page_down(run: &str, slices: &[u64]) -> serde_json::Value {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/wheel_frozen.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    g.act_input(RequestId::new("p-1").unwrap(), &Action::Press { target: None, key: "PageDown".into() }).await.unwrap();
    let mut seen = Vec::new();
    for ms in slices {
        g.advance(VirtualDuration::from_ms(*ms)).await.unwrap();
        seen.push(g.eval_main("scrollY").await.unwrap());
    }
    serde_json::Value::Array(seen.split_off(seen.len() - 1))
}

#[tokio::test]
async fn a_key_scroll_lands_the_same_for_any_slicing() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // The compositor would animate it by the real clock; it jumps (`--disable-smooth-scrolling`).
    let one = page_down("pd-1", &[300]).await;
    assert_eq!(one, page_down("pd-2", &[1; 300]).await);
    assert_eq!(one, page_down("pd-3", &[17, 200, 83]).await);
    assert_ne!(one, serde_json::json!([0]), "it scrolled");
}

#[tokio::test]
async fn a_new_document_continues_the_virtual_clock() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // Navigation clock continuity: after a navigation at 50 ms — in this renderer, or another site's — the document's
    // clock reads 50 ms and its time origin is the instant it committed.
    for (run, cross_site) in [("clock-same", false), ("clock-cross", true)] {
        let addr = support::serve();
        let runs = tempfile::tempdir().unwrap();
        let mut o = support::options(runs.path(), run, format!("http://{addr}/wheel_frozen.html"), RuleSet::default());
        o.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port())]);
        let mut g = Generation::launch(o).await.unwrap();
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let host = if cross_site { format!("localhost:{}", addr.port()) } else { addr.to_string() };
        g.navigate_action(&RequestId::new("n-1").unwrap(), &Action::Navigate { url: format!("http://{host}/frames_idle.html") }).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        g.advance(VirtualDuration::from_ms(30)).await.unwrap();
        let clock = g.eval_main("JSON.stringify([Date.now() - 946684800000, Math.round(performance.now()), Math.round(performance.timeOrigin - 946684800000)])").await.unwrap();
        assert_eq!(clock, serde_json::json!("[80,30,50]"), "cross_site {cross_site}");
    }
}

/// The renderer processes under the browser.
/// The renderer processes under the browser: its children, and on Linux the zygote's.
fn renderers(browser: u32) -> std::collections::BTreeSet<u32> {
    let out = std::process::Command::new("ps").args(["-Ao", "pid=,ppid=,args="]).output().unwrap();
    let rows: Vec<(u32, u32, bool)> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let (pid, ppid): (u32, u32) = (f.next()?.parse().ok()?, f.next()?.parse().ok()?);
            Some((pid, ppid, l.contains("--type=renderer")))
        })
        .collect();
    let mut tree = vec![browser];
    let mut i = 0;
    while i < tree.len() {
        let parent = tree[i];
        tree.extend(rows.iter().filter(|r| r.1 == parent).map(|r| r.0));
        i += 1;
    }
    rows.iter().filter(|r| r.2 && tree.contains(&r.0)).map(|r| r.0).collect()
}

/// After the page navigates by itself at 300 ms, in the middle of one advance to 1000 ms: where the clock
/// ends, the new document's clock, its time origin, and its navigation entry.
async fn navigation_mid_budget(run: &str, to: &str, pause_ms: u64) -> (u64, serde_json::Value, bool) {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let to = to.replace("{port}", &addr.port().to_string()).replace("{addr}", &addr.to_string());
    let mut o = support::options(runs.path(), run, format!("http://{addr}/nav_later.html?to={to}"), RuleSet::default());
    // Passed through: the refused connection (port 1) gives the browser's error page.
    o.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port()), "http://127.0.0.1:1".into()]);
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(10)).await.unwrap();
    let before = renderers(g.browser_pid());
    tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
    g.advance(VirtualDuration::from_ms(990)).await.unwrap_or_else(|error| panic!("{run}: {error:?}"));
    let new_renderer = !renderers(g.browser_pid()).is_subset(&before);
    let clock = g
        .eval_main(
            "(() => { const n = performance.getEntriesByType('navigation')[0] || {}; \
             return JSON.stringify([Date.now() - 946684800000, Math.round(performance.timeOrigin - 946684800000), \
             [n.fetchStart, n.requestStart, n.responseStart, n.responseEnd].map(Math.round)]); })()",
        )
        .await
        .unwrap();
    (g.now().0 / 1000, clock, new_renderer)
}

#[tokio::test]
async fn a_navigation_in_the_middle_of_a_budget_keeps_the_clock() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // Another site's document and the same site's (both stay in this renderer: no site isolation), and a
    // failed one, whose error page comes in another renderer.
    for (run, to, elsewhere) in [("mid-cross", "http://localhost:{port}/frames_idle.html", false), ("mid-same", "http://{addr}/frames_idle.html", false), ("mid-fail", "http://127.0.0.1:1/x", true)] {
        let (end, clock, new_renderer) = navigation_mid_budget(run, to, 0).await;
        assert_eq!(end, 1000, "{run}: the advance ends at its goal");
        assert_eq!(new_renderer, elsewhere, "{run}: which renderer the document came in");
        // An error page has no navigation entry: its fields read null.
        let v: (u64, u64, Vec<Option<u64>>) = serde_json::from_str(clock.as_str().unwrap()).unwrap();
        assert_eq!(v.0, 1000, "{run}: the document reads the page's clock: {clock}");
        assert!((300..=1000).contains(&v.1), "{run}: its time origin is when it committed, after the navigation started: {clock}");
        // The fetch, request and response at its start; the body ends when it reached the page (virtual).
        assert!(v.2[..3].iter().all(|t| t.is_none_or(|t| t == 0)), "{run}: the navigation's steps at its start: {clock}");
        assert_eq!(navigation_mid_budget(&format!("{run}-2"), to, 300).await.1, clock, "{run}: the same with wall time between");
    }
}

/// The page's resource timing entries after loading two mocked subresources (a passed-through one ends when
/// the real network delivers it, like a timer: `passthrough_content`).
async fn resource_timings(run: &str, pause_ms: u64) -> String {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(&format!(
        r#"[{{"id":"r","match":{{"url":"https://api.example.test/r"}},"action":{{"type":"mock","status":200,"body":"r","latency_ms":50}}}},
            {{"id":"own","match":{{"url":"http://{addr}/mocked.txt"}},"action":{{"type":"mock","status":200,"body":"m","latency_ms":30}}}},
            {{"id":"redir","match":{{"url":"http://{addr}/redir"}},"action":{{"type":"mock","status":302,"headers":{{"location":"mocked.txt"}},"latency_ms":10}}}},
            {{"id":"frame","match":{{"url":"http://{addr}/frame_mock.html"}},"action":{{"type":"mock","status":200,"headers":{{"content-type":"text/html"}},"body":"<p>f</p>"}}}},
            {{"id":"worker","match":{{"url":"http://{addr}/worker_mock.js"}},"action":{{"type":"mock","status":200,"headers":{{"content-type":"text/javascript"}},"body":"postMessage(1)","latency_ms":60}}}}]"#
    ))
    .unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/resources.html"), rules)).await.unwrap();
    // Wall time while the requests are in flight (issued at 20 ms).
    g.advance(VirtualDuration::from_ms(25)).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
    g.advance(VirtualDuration::from_ms(175)).await.unwrap();
    g.eval_main("timings()").await.unwrap().as_str().unwrap().to_string()
}

#[tokio::test]
async fn resource_timing_is_on_the_virtual_clock() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let one = resource_timings("rt-1", 0).await;
    assert_eq!(one, resource_timings("rt-2", 300).await, "the same with wall time between");
    let v: Vec<(String, u64, u64, u64, u64, u64)> = serde_json::from_str(&one).unwrap();
    let mocked = v.iter().find(|e| e.0 == "r").unwrap_or_else(|| panic!("{one}"));
    // Issued at 20 ms (the time origin is 0), delivered 50 ms later; cross-origin without
    // Timing-Allow-Origin, so its request and response starts read 0, as the spec says.
    assert_eq!((mocked.1, mocked.2, mocked.3, mocked.4, mocked.5), (20, 20, 0, 0, 70), "{one}");
    // Same-origin: no network timings — every step at its issue — and its end at its delivery.
    // Same-origin: sent at its issue, headers and body at its delivery (issue + latency).
    let own = v.iter().find(|e| e.0 == "mocked.txt").unwrap_or_else(|| panic!("{one}"));
    assert_eq!((own.1, own.2, own.3, own.4, own.5), (20, 20, 20, 50, 50), "{one}");
    // The frame's document (a document request takes no latency) and the worker's script end at their
    // deliveries.
    let end = |name: &str| v.iter().find(|e| e.0 == name).map(|e| e.5).unwrap_or_else(|| panic!("{name}: {one}"));
    assert_eq!((end("frame_mock.html"), end("worker_mock.js")), (20, 80), "{one}");
    // A redirect at 30 ms, then the target's 30 ms: the entry ends at 60.
    assert_eq!(end("redir"), 60, "{one}");
    assert!(v.iter().all(|e| e.1 <= e.5), "{one}");
}

/// The page's record of its worker's messages (`[what, worker's clock, page's clock]`) after the advances.
async fn worker_run(run: &str, page: &str, rules: RuleSet, slices: &[u64], pause_ms: u64) -> serde_json::Value {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/{page}"), rules)).await.unwrap();
    g.advance(VirtualDuration::from_ms(20)).await.unwrap_or_else(|error| panic!("{page}, {run}, initial 20 ms: {error}"));
    tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
    for ms in slices {
        g.advance(VirtualDuration::from_ms(*ms)).await.unwrap_or_else(|error| panic!("{page}, {run}, slice {ms} ms: {error}"));
    }
    g.eval_main("JSON.stringify(got)").await.unwrap()
}

#[tokio::test]
async fn a_worker_runs_by_the_page_virtual_clock() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // Worker virtual time: the worker's timers fire at their virtual instants and the page hears them then,
    // for any slicing and any wall time between operations.
    let one = worker_run("wk-1", "worker_clock.html", RuleSet::default(), &[300], 0).await;
    assert_eq!(one, serde_json::json!(r#"[["start",0,0],["t30",30,30],["t200",200,200]]"#));
    assert_eq!(one, worker_run("wk-2", "worker_clock.html", RuleSet::default(), &[7, 100, 43, 150], 400).await);
    assert_eq!(one, worker_run("wk-3", "worker_clock.html", RuleSet::default(), &[1; 300], 0).await);
}

#[tokio::test]
async fn a_worker_request_is_answered_by_the_rules() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // The worker's fetch at 30 ms is the platform's to answer (mocked, 40 ms), the same for any slicing and
    // wall time between operations.
    let rules = || RuleSet::from_json(r#"[{"id":"w","match":{"url":"https://api.example.test/w"},"action":{"type":"mock","status":200,"body":"mocked","latency_ms":40}}]"#).unwrap();
    let one = worker_run("wr-1", "worker_fetch.html", rules(), &[300], 0).await;
    let v: Vec<(String, u64, u64)> = serde_json::from_str(one.as_str().unwrap()).unwrap();
    assert_eq!(v.len(), 1, "{one}");
    assert_eq!(v[0].0, "mocked", "{one}");
    // Issued at 30 by the worker, which stops virtual time there like the page's own fetch; delivered 40 ms
    // later, and the page hears it at once.
    assert_eq!((v[0].1, v[0].2), (70, 70), "{one}");
    assert_eq!(one, worker_run("wr-2", "worker_fetch.html", rules(), &[7, 100, 43, 150], 400).await);
    assert_eq!(one, worker_run("wr-3", "worker_fetch.html", rules(), &[1; 300], 0).await);
}

/// Rules for the worker shapes: a classic and a module script to import, two short responses.
fn worker_rules() -> RuleSet {
    RuleSet::from_json(r#"[
        {"id":"lib","match":{"url":"https://api.example.test/lib.js"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/javascript"},"body":"var lib = 7;","latency_ms":20}},
        {"id":"mjs","match":{"url":"https://api.example.test/lib.mjs"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/javascript","access-control-allow-origin":"*"},"body":"export const lib = 8;","latency_ms":20}},
        {"id":"z","match":{"url":"https://api.example.test/z"},"action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"z"}},
        {"id":"a","match":{"url":"https://api.example.test/a"},"action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"a","latency_ms":2}},
        {"id":"b","match":{"url":"https://api.example.test/b"},"action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"b","latency_ms":5}}]"#).unwrap()
}

/// One worker shape: `[what, worker's clock, (lib,) page's clock]`, the same for any slicing and wall time
/// between operations.
async fn worker_shape(page: &str, expected: &str) {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    for (n, slices, pause) in [(1, vec![300u64], 0u64), (2, vec![1; 300], 0), (3, vec![7, 100, 43, 150], 400)] {
        let got = worker_run(&format!("ws-{n}"), page, worker_rules(), &slices, pause).await;
        assert_eq!(got, serde_json::json!(expected), "{page}, run {n}");
    }
}

/// Each shape is its own test, so the shapes run beside each other.
mod workers_of_every_shape_run_by_the_page_virtual_clock {
    use super::worker_shape;

    /// A worker's own worker is joined through its session.
    #[tokio::test]
    async fn worker_nested() {
        worker_shape("worker_nested.html", r#"[["inner30",30,30]]"#).await;
    }

    /// A synchronous import at the worker's start: issued at 0, 20 ms of latency.
    #[tokio::test]
    async fn worker_import() {
        worker_shape("worker_import.html", r#"[["imported",20,7,20],["t30",50,7,50]]"#).await;
    }

    /// A module graph's import at the worker's start.
    #[tokio::test]
    async fn worker_module() {
        worker_shape("worker_module.html", r#"[["imported",20,8,20],["t30",50,8,50]]"#).await;
    }

    /// Two responses shorter than the engine's completion step, overlapping: each at its own instant.
    #[tokio::test]
    async fn worker_two_fetches() {
        worker_shape("worker_two_fetches.html", r#"[["a",32,32],["b",35,35]]"#).await;
    }

    /// A classic script over the network: its start holds the clock until it runs.
    #[tokio::test]
    async fn worker_network() {
        worker_shape("worker_network.html", r#"[["start",0,0],["t30",30,30]]"#).await;
    }

    /// Terminated before it started, or its frame removed in the same task: the page's clock goes on.
    #[tokio::test]
    async fn worker_gone() {
        worker_shape("worker_gone.html", r#"[["page50",50,50]]"#).await;
    }

    /// A fetch in flight while the worker blocks in a synchronous import: its response is handled once
    /// the import returned (the thread could not before).
    #[tokio::test]
    async fn worker_fetch_then_import() {
        worker_shape("worker_fetch_then_import.html", r#"[["imported",20,20],["a",20,20]]"#).await;
    }

    /// A worker started just before its parent blocks in a synchronous import starts once the import
    /// returned (its start goes on on the parent's thread).
    #[tokio::test]
    async fn worker_nested_import() {
        worker_shape("worker_nested_import.html", r#"[["imported",20,20],["inner",20,20],["inner30",50,50]]"#).await;
    }

    /// Answered at once (0 ms) while the worker blocks in an import: handled once the import returned.
    #[tokio::test]
    async fn worker_fetch0_then_import() {
        worker_shape("worker_fetch0_then_import.html", r#"[["imported",20,20],["z",20,20]]"#).await;
    }

    /// Released by the platform before the import starts (at 1 ms): its 5 ms answer is handled once the
    /// import returned, at 21.
    #[tokio::test]
    async fn worker_released_then_import() {
        worker_shape("worker_released_then_import.html", r#"[["imported",21,21],["b",21,21]]"#).await;
    }

    /// A network-script child already past its parent's thread runs on while the parent imports; its
    /// first message reaches the page through the parent once the import returned.
    #[tokio::test]
    async fn worker_netchild_import() {
        worker_shape("worker_netchild_import.html", r#"[["imported",20,20],["start",0,20],["t30",30,30]]"#).await;
    }

    /// No timer separates construction from the synchronous import. The
    /// browser can announce the child script, but its thread starts at 20.
    #[tokio::test]
    async fn worker_netchild_immediate_import() {
        worker_shape("worker_netchild_immediate_import.html", r#"[["imported",20,20],["start",20,20],["t30",50,50]]"#).await;
    }

    /// A module script over the network: its import is the worker's own load.
    #[tokio::test]
    async fn worker_net_module() {
        worker_shape("worker_net_module.html", r#"[["imported",20,8,20],["t30",50,8,50]]"#).await;
    }

    /// Terminated at 50 while busy: the page's clock goes on.
    #[tokio::test]
    async fn worker_terminate() {
        worker_shape("worker_terminate.html", r#"[["tick",5,5],["tick",10,10],["tick",15,15],["tick",20,20],["tick",25,25],["tick",30,30],["tick",35,35],["tick",40,40],["tick",45,45],["page120",120,120]]"#).await;
    }
}

#[tokio::test]
async fn a_worker_started_between_operations_runs_from_that_instant() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // Created while the platform holds the clock (between operations), with wall time before the next advance:
    // its first message is heard at the instant it was created, its timer 30 ms later.
    for (n, pause) in [(1, 0u64), (2, 400)] {
        let addr = support::serve();
        let runs = tempfile::tempdir().unwrap();
        let mut g = Generation::launch(support::options(runs.path(), &format!("wb-{n}"), format!("http://{addr}/frames_idle.html"), RuleSet::default())).await.unwrap();
        g.advance(VirtualDuration::from_ms(20)).await.unwrap();
        g.eval_main(r#"window.got = []; { const t = () => Date.now() - 946684800000; const src = `postMessage(["start", Date.now() - 946684800000]); setTimeout(() => postMessage(["t30", Date.now() - 946684800000]), 30);`; const w = new Worker(URL.createObjectURL(new Blob([src], { type: "text/javascript" }))); w.onmessage = (e) => got.push([...e.data, t()]); } 1"#).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(pause)).await;
        g.advance(VirtualDuration::from_ms(100)).await.unwrap();
        let got = g.eval_main("JSON.stringify(got)").await.unwrap();
        assert_eq!(got, serde_json::json!(r#"[["start",20,20],["t30",50,50]]"#), "run {n}");
    }
}

/// Screenshots of an animated GIF (8 frames of 100 ms) at virtual instants, after `slices` of advance with
/// `pause_ms` of wall time between them.
async fn gif_shots(run: &str, slices: &[u64], pause_ms: u64) -> Vec<(u64, Vec<u8>)> {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/gif_show.html"), RuleSet::default())).await.unwrap();
    let mut shots = Vec::new();
    for ms in slices {
        g.advance(VirtualDuration::from_ms(*ms)).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(pause_ms)).await;
        if g.now().0 % 50_000 == 0 {
            let shot = g.screenshot().await.unwrap();
            shots.push((shot.t_us.0, std::fs::read(&shot.path).unwrap()));
        }
    }
    shots
}

#[tokio::test]
async fn animated_images_run_by_the_virtual_clock() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // Virtual image animation: an animated image shows the frame its virtual time says, the same for any slicing
    // and any wall time between operations (cc animated it by the real clock).
    let one = gif_shots("gif-1", &[50; 12], 0).await;
    let two = gif_shots("gif-2", &[10; 60], 120).await;
    let three = gif_shots("gif-3", &[25; 24], 0).await;
    assert_eq!(one.len(), 12);
    let times = |v: &Vec<(u64, Vec<u8>)>| v.iter().map(|(t, _)| *t).collect::<Vec<_>>();
    assert_eq!(times(&one), times(&two));
    assert_eq!(one, two, "the same frames for any slicing and wall time");
    assert_eq!(one, three);
    // It animates: frames 100 ms apart differ.
    assert_ne!(one[1].1, one[3].1, "the image moved between 100 and 200 ms");
}

#[tokio::test]
async fn toggling_an_animated_image_never_stalls_the_clock() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // An animated image shown and hidden at varying intervals: an impl-side invalidation by the real clock
    // used to block the platform's frame from drawing, and virtual time stopped (Plan 23).
    let addr = support::serve();
    for n in 0..3 {
        let runs = tempfile::tempdir().unwrap();
        let mut g = Generation::launch(support::options(runs.path(), &format!("gt-{n}"), format!("http://{addr}/gif_toggle.html?only=gif"), RuleSet::default())).await.unwrap();
        g.advance(VirtualDuration::from_ms(20_000)).await.unwrap();
        assert_eq!(g.eval_main("document.querySelector('#n').textContent").await.unwrap(), serde_json::json!("241"));
    }
}


/// The animation frames a dedicated worker's messages ask for, after `slices` of advance.
async fn worker_frames(run: &str, slices: &[u64]) -> String {
    worker_frames_of(run, "worker_raf.js", slices).await
}

async fn worker_frames_of(run: &str, worker: &str, slices: &[u64]) -> String {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/worker_raf.html?worker={worker}"), RuleSet::default())).await.unwrap();
    for ms in slices {
        g.advance(VirtualDuration::from_ms(*ms)).await.unwrap();
    }
    g.eval_main("JSON.stringify(got)").await.unwrap().as_str().unwrap().to_string()
}

/// A worker's timer can request a frame before the page's own next task. The
/// native wake bound preserves those exact ticks when idle frames are skipped.
#[tokio::test]
async fn frames_a_workers_message_asks_for_come_at_the_same_ticks_for_any_slicing() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let one = worker_frames("wraf-1", &[1000]).await;
    assert_eq!(one, worker_frames("wraf-2", &[100; 10]).await);
    assert_eq!(one, worker_frames("wraf-3", &[300, 700]).await);
    assert_eq!(one, worker_frames("wraf-4", &[7; 143]).await);
    let got: Vec<(String, u64)> = serde_json::from_str(&one).unwrap();
    // At the tick after each message: 100 ms (tick 101) and 437 ms (tick 451).
    assert_eq!(got, vec![("a".to_string(), 101), ("b".to_string(), 451)], "{one}");
}

/// Worker/frame readiness: a worker's message posted at a frame tick reaches the page before that tick's frame,
/// however the threads were scheduled: the frame it asks for is that tick's, in every run.
#[tokio::test]
async fn a_workers_message_at_a_tick_is_before_that_ticks_frame() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    for run in 0..8 {
        let got = worker_frames_of(&format!("wtick-{run}"), "worker_tick.js", &[300]).await;
        // Posted at the ticks 51 and 101: their animation frames run in those ticks' frames.
        assert_eq!(got, r#"[["a",51],["b",101]]"#, "run {run}");
    }
}

/// Idle worker intervals need no per-frame round trips, while the worker's
/// messages still reach the page before the exact frame they request.
#[tokio::test]
async fn idle_frame_ticks_are_skipped_between_worker_messages() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "worker-idle-frames", format!("http://{addr}/worker_raf.html"), RuleSet::default())).await.unwrap();
    let before = g.frame_requests();
    g.advance(VirtualDuration::from_ms(10_000)).await.unwrap();
    assert_eq!(g.eval_main("JSON.stringify(got)").await.unwrap(), serde_json::json!(r#"[["a",101],["b",451]]"#));
    let frames = g.frame_requests() - before;
    // 600 frame ticks elapse. This permits engine maintenance wakes but rules
    // out polling every tick merely because a worker remains alive.
    assert!(frames < 60, "{frames} native frame requests for two worker messages");
    assert_eq!(g.now(), frontcap_core::time::VirtualTime(10_000_000));
}

#[tokio::test]
async fn frames_requested_by_a_nested_worker_keep_the_same_ticks() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let one = worker_frames_of("nested-raf-1", "worker_raf_nested.js", &[1000]).await;
    assert_eq!(one, r#"[["a",101],["b",451]]"#);
    assert_eq!(one, worker_frames_of("nested-raf-2", "worker_raf_nested.js", &[7; 143]).await);
}
