//! Engine probe (P06): what a window the page opens does when the browser holds it at creation.
//! Launches the patched engine as a generation does, but without `--block-new-web-contents`, holds new
//! pages with a browser-level `Target.setAutoAttach {waitForDebuggerOnStart}` and reports, per opening:
//! whether the new page is held, whether its first request waits for the release, which renderer
//! process it runs in, and whether its clock and timers follow the opener's virtual time.
mod support;
use cdp::protocol::{emulation, fetch, network as cnet, page, runtime, system_info, target};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const OPENER: &str = r#"<!doctype html><title>opener</title><a id=blank href="/popup.html?link" target=_blank>link</a>
<script>window.messages = []; addEventListener('message', e => messages.push(String(e.data)));</script>"#;
const POPUP: &str = r#"<!doctype html><title>popup</title><script>
window.loadedAt = Date.now();
try { window.opener && window.opener.postMessage('hello from ' + location.search, '*'); } catch (e) {}
</script>"#;

fn serve(log: Arc<Mutex<Vec<(String, Instant)>>>) -> u16 {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            log.lock().unwrap().push((req.url().to_string(), Instant::now()));
            let body = if req.url().starts_with("/popup.html") { POPUP } else { OPENER };
            let r = tiny_http::Response::from_string(body)
                .with_header(tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap());
            let _ = req.respond(r);
        }
    });
    port
}

async fn eval(c: &cdp::Client, session: &str, expression: &str) -> Value {
    let r = tokio::time::timeout(Duration::from_secs(5), c.call(Some(session), &runtime::EvaluateParams {
        return_by_value: Some(true), user_gesture: Some(true), await_promise: Some(true),
        ..runtime::EvaluateParams::new(expression.into())
    })).await;
    let Ok(r) = r else { return json!({"error": "no answer within 5 s"}) };
    match r {
        Ok(r) => match r.exception_details {
            Some(e) => json!({"exception": e.text}),
            None => r.result.value.unwrap_or(Value::Null),
        },
        Err(e) => json!({"error": e.to_string()}),
    }
}

async fn renderers(c: &cdp::Client) -> Vec<i64> {
    let Ok(Ok(info)) = tokio::time::timeout(Duration::from_secs(5), c.call(None, &system_info::GetProcessInfoParams::default())).await else { return vec![-1] };
    let mut ids: Vec<i64> = info.process_info.into_iter().filter(|p| p.r#type == "renderer").map(|p| p.id).collect();
    ids.sort();
    ids
}

/// Grants `ms` of virtual time on `session` and waits (bounded by wall time) for the budget to expire.
async fn grant(c: &cdp::Client, session: &str, ms: f64) -> bool {
    let mut expired = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(Some(session));
    let set = tokio::time::timeout(Duration::from_secs(5), c.call(Some(session), &emulation::SetVirtualTimePolicyParams {
        budget: Some(ms),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending)
    })).await;
    if !matches!(set, Ok(Ok(_))) { println!("POPUP_STEP grant refused or unanswered: {set:?}"); return false; }
    tokio::time::timeout(Duration::from_secs(10), expired.next()).await.is_ok()
}

fn requested(log: &Arc<Mutex<Vec<(String, Instant)>>>, needle: &str) -> Option<Instant> {
    log.lock().unwrap().iter().find(|(u, _)| u.contains(needle)).map(|(_, t)| *t)
}

#[tokio::test]
async fn popup_enrollment_probe() {
    let root = support::repo_root();
    let platform = browser_pin::current_platform().unwrap();
    let (binary, manifest) = browser_pin::engine::patched_binary(&root, platform).unwrap().expect("a patched engine build");
    let log: Arc<Mutex<Vec<(String, Instant)>>> = Default::default();
    let port = serve(log.clone());
    // As a generation launches the patched engine, without the new-window block.
    let args: Vec<String> = [
        "--disable-frame-rate-limit", "--disable-gpu-vsync",
        "--disable-features=DeferRendererTasksAfterInput,PaintHolding",
        "--disable-site-isolation-trials", "--disable-smooth-scrolling",
        "--enable-features=FrontcapTimeSeed:seed/1,FrontcapDeferredCompletion,FrontcapFramesWhileFrozen,FrontcapBeginFramesOnDemand,FrontcapVirtualNavigation,FrontcapVirtualResourceTiming,FrontcapVirtualWorkers,FrontcapVirtualImageAnimation,FrontcapObserverFrames,FrontcapFramesAfterWorkers",
        "--disable-threaded-animation", "--run-all-compositor-stages-before-draw",
    ].iter().map(|s| s.to_string()).collect();
    let mut args = args;
    // `PROBE_PROCESS_LIMIT=1`: one renderer process for every page, popups without an opener included.
    if let Ok(limit) = std::env::var("PROBE_PROCESS_LIMIT") {
        args.push(format!("--renderer-process-limit={limit}"));
    }
    println!("POPUP_PROBE {}", json!({"args": args}));
    let launched = browser_pin::launch(&binary, &args, Duration::from_secs(30)).unwrap();
    let c = cdp::Client::connect(&launched.browser_ws_url).await.unwrap();
    let mut attached = c.subscribe::<target::AttachedToTargetEvent>(None);
    let mut created = c.subscribe::<target::TargetCreatedEvent>(None);
    c.call(None, &target::SetDiscoverTargetsParams::new(true)).await.unwrap();
    c.call(None, &target::SetAutoAttachParams {
        flatten: Some(true),
        filter: Some(target::TargetFilter(vec![
            target::FilterEntry { r#type: Some("page".into()), exclude: Some(false) },
            target::FilterEntry { r#type: None, exclude: Some(true) },
        ])),
        ..target::SetAutoAttachParams::new(true, true)
    }).await.unwrap();
    // The existing tab is attached at once, not held.
    let first = tokio::time::timeout(Duration::from_secs(5), attached.next()).await.unwrap().unwrap();
    let opener = first.session_id.0.clone();
    let opener_target = first.target_info.target_id.0.clone();
    c.call(Some(&opener), &page::EnableParams::default()).await.unwrap();
    c.call(Some(&opener), &runtime::EnableParams::default()).await.unwrap();
    c.call(Some(&opener), &cnet::EnableParams::default()).await.unwrap();
    c.call(Some(&opener), &emulation::SetVirtualTimePolicyParams {
        initial_virtual_time: Some(cnet::TimeSinceEpoch(1_700_000_000.0)),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)
    }).await.unwrap();
    let mut loaded = c.subscribe::<page::LoadEventFiredEvent>(Some(&opener));
    c.call(Some(&opener), &page::NavigateParams::new(format!("http://127.0.0.1:{port}/opener.html"))).await.unwrap();
    let mut grants = 0;
    while tokio::time::timeout(Duration::from_millis(50), loaded.next()).await.is_err() && grants < 40 {
        grant(&c, &opener, 50.0).await;
        grants += 1;
    }
    let opener_clock = eval(&c, &opener, "({now: Date.now(), perf: performance.now(), ready: document.readyState})").await;
    let renderers_before = renderers(&c).await;
    println!("POPUP_PROBE {}", json!({"engine": manifest.patchset, "opener": opener_clock, "grants_to_load": grants, "renderers": renderers_before}));

    let cases = [
        ("same_origin", format!("window.w = window.open('/popup.html?same', 'same'); String(w === null ? 'null' : typeof w)")),
        ("cross_origin", format!("window.w = window.open('http://localhost:{port}/popup.html?cross', 'cross'); String(w === null ? 'null' : typeof w)")),
        ("noopener", format!("window.w = window.open('/popup.html?noopener', '_blank', 'noopener'); String(w === null ? 'null' : typeof w)")),
        ("link", "document.getElementById('blank').click(); 'clicked'".to_string()),
    ];
    for (name, open) in cases {
        let needle = format!("?{}", if name == "link" { "link" } else if name == "same_origin" { "same" } else if name == "cross_origin" { "cross" } else { "noopener" });
        let opened_at = Instant::now();
        let returned = eval(&c, &opener, &open).await;
        println!("POPUP_STEP {name}: opened, returned {returned}");
        let popup = loop {
            match tokio::time::timeout(Duration::from_secs(5), attached.next()).await {
                Ok(Ok(a)) if a.target_info.target_id.0 != opener_target => break Some(a),
                Ok(Ok(_)) => continue,
                _ => break None,
            }
        };
        let Some(popup) = popup else {
            println!("POPUP_PROBE {}", json!({"case": name, "returned": returned, "attached": false}));
            continue;
        };
        let mut created_infos = Vec::new();
        while let Ok(Ok(t)) = tokio::time::timeout(Duration::from_millis(100), created.next()).await {
            created_infos.push(json!({"type": t.target_info.r#type, "url": t.target_info.url, "opener": t.target_info.opener_id.map(|o| o.0 == opener_target)}));
        }
        let session = popup.session_id.0.clone();
        println!("POPUP_STEP {name}: attached, waiting {}", popup.waiting_for_debugger);
        // Held: give the browser time to send the first request if it would.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let before_release = requested(&log, &needle).map(|t| t.duration_since(opened_at).as_millis());
        let opener_view = eval(&c, &opener, "(() => { try { return {href: w && w.location.href, ready: w && w.document.readyState}; } catch (e) { return {error: String(e)}; } })()").await;
        let renderers_held = renderers(&c).await;
        println!("POPUP_STEP {name}: opener view while held {opener_view}");
        let enabled = tokio::time::timeout(Duration::from_secs(5), c.call(Some(&session), &runtime::EnableParams::default())).await;
        println!("POPUP_STEP {name}: Runtime.enable {:?}", enabled.as_ref().map(|r| r.is_ok()));
        let enabled = tokio::time::timeout(Duration::from_secs(5), c.call(Some(&session), &page::EnableParams::default())).await;
        println!("POPUP_STEP {name}: Page.enable {:?}", enabled.as_ref().map(|r| r.is_ok()));
        // The popup's own requests: intercepted on its session before it runs.
        let mut paused = c.subscribe::<fetch::RequestPausedEvent>(Some(&session));
        let fetching = tokio::time::timeout(Duration::from_secs(5), c.call(Some(&session), &fetch::EnableParams::default())).await;
        println!("POPUP_STEP {name}: Fetch.enable {:?}", fetching.as_ref().map(|r| r.is_ok()));
        let released_at = Instant::now();
        let released = tokio::time::timeout(Duration::from_secs(5), c.call(Some(&session), &runtime::RunIfWaitingForDebuggerParams::default())).await;
        println!("POPUP_STEP {name}: released {:?}", released.as_ref().map(|r| r.is_ok()));
        let intercepted = tokio::time::timeout(Duration::from_secs(3), paused.next()).await;
        let (paused_url, paused_id) = match &intercepted {
            Ok(Ok(p)) => (Some(p.request.url.clone()), Some(p.request_id.clone())),
            _ => (None, None),
        };
        // While the popup's document request is held, does the opener answer?
        let opener_while_request_held = eval(&c, &opener, "({now: Date.now(), w: typeof w})").await;
        // Does the held document request stop the clock, as a page's own pending fetch does?
        let clock_before = c.call(Some(&opener), &emulation::GetVirtualTimeParams::default()).await.map(|t| t.unix_time_us).ok();
        let expired = grant(&c, &opener, 100.0).await;
        let clock_after = c.call(Some(&opener), &emulation::GetVirtualTimeParams::default()).await.map(|t| t.unix_time_us).ok();
        let popup_clock_held = tokio::time::timeout(Duration::from_secs(2), c.call(Some(&session), &emulation::GetVirtualTimeParams::default())).await.ok().and_then(|r| r.ok()).map(|t| t.unix_time_us);
        println!("POPUP_STEP {name}: grant 100 ms with the document request held: expired {expired}, clock {clock_before:?} -> {clock_after:?}, popup clock {popup_clock_held:?}");
        let request_while_held = requested(&log, &needle).is_some();
        println!("POPUP_STEP {name}: paused {paused_url:?}; opener {opener_while_request_held}; reached server {request_while_held}");
        if let Some(id) = paused_id {
            let _ = c.call(Some(&session), &fetch::ContinueRequestParams::new(id)).await;
        }
        // Later requests of the popup pass through.
        let continuing = c.clone();
        let popup_session = session.clone();
        tokio::spawn(async move {
            while let Ok(p) = paused.next().await {
                let _ = continuing.call(Some(&popup_session), &fetch::ContinueRequestParams::new(p.request_id)).await;
            }
        });
        // With the opener's virtual time paused: does the popup load by wall time alone?
        tokio::time::sleep(Duration::from_millis(700)).await;
        let after_release_paused = requested(&log, &needle).map(|t| t.duration_since(released_at).as_millis() as i64);
        println!("POPUP_STEP {name}: request after release {after_release_paused:?}");
        let popup_paused = eval(&c, &session, "({href: location.href, ready: document.readyState, now: Date.now(), loadedAt: window.loadedAt ?? null, opener: !!window.opener})").await;
        // Grant the opener's session virtual time and look again.
        let mut granted = 0;
        for _ in 0..20 {
            if eval(&c, &session, "document.readyState").await == json!("complete") { break; }
            grant(&c, &opener, 50.0).await;
            granted += 1;
        }
        let popup_after = eval(&c, &session, "({href: location.href, ready: document.readyState, now: Date.now(), perf: performance.now(), loadedAt: window.loadedAt ?? null, opener: !!window.opener})").await;
        let opener_after = eval(&c, &opener, "({now: Date.now(), messages})").await;
        // A popup timer: wall time alone, then the opener's grant.
        eval(&c, &session, "window.fired = 0; setTimeout(() => window.fired = Date.now(), 200); true").await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        let fired_wall = eval(&c, &session, "window.fired").await;
        grant(&c, &opener, 250.0).await;
        let fired_granted = eval(&c, &session, "window.fired").await;
        let renderers_after = renderers(&c).await;
        println!("POPUP_PROBE {}", json!({
            "case": name, "returned": returned, "waiting_for_debugger": popup.waiting_for_debugger,
            "document_request_paused": paused_url, "opener_while_request_held": opener_while_request_held,
            "server_saw_held_request": request_while_held,
            "target": {"type": popup.target_info.r#type, "url": popup.target_info.url,
                       "opener_is_page": popup.target_info.opener_id.as_ref().map(|o| o.0 == opener_target),
                       "can_access_opener": popup.target_info.can_access_opener},
            "created": created_infos,
            "request_before_release_ms": before_release, "opener_view_while_held": opener_view,
            "renderers_held": renderers_held, "request_after_release_ms": after_release_paused,
            "popup_while_paused": popup_paused, "grants_to_complete": granted, "popup_after_grants": popup_after,
            "opener_after": opener_after, "timer_after_wall_600ms": fired_wall, "timer_after_grant_250ms": fired_granted,
            "renderers_after": renderers_after,
        }));
        let _ = c.call(None, &target::CloseTargetParams::new(popup.target_info.target_id.clone())).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    drop(launched);
}
