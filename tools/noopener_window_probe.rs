//! Engine probe (P06): can a window opened without an opener join the generation deterministically?
//! Launches the patched engine as a generation does (one renderer process), holds new pages with a
//! browser-level `Target.setAutoAttach {waitForDebuggerOnStart}` and, for windows opened by a timer
//! during a granted step (`window.open(…, 'noopener')`, a `target=_blank` link click):
//! (a) whether the held window's attach always arrives before the budget expiry of the step it was
//!     opened in (arrival order on the one connection);
//! (b) whether, released with the opener's clock stopped and its document request answered, the window
//!     commits without any grant;
//! (c) whether instrumentation sent while it was held runs in its first document, and at which instant
//!     its document's scripts run.
mod support;
use cdp::protocol::{emulation, fetch, page, runtime, target};
use serde_json::{Value, json};
use std::time::Duration;

const OPENER: &str = r#"<!doctype html><title>opener</title><a id=blank href="/popup.html?link" target=_blank>link</a>
<script>window.later = (ms, how) => setTimeout(() => how === 'link' ? document.getElementById('blank').click() : window.open('/popup.html?noopener', '_blank', 'noopener'), ms);</script>"#;
const POPUP: &str = r#"<!doctype html><title>popup</title><script>window.loadedAt = Date.now(); window.sawInstrumentation = window.__probeInstrumented ?? null;</script>"#;

fn serve() -> u16 {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let port = server.server_addr().to_ip().unwrap().port();
    std::thread::spawn(move || {
        for req in server.incoming_requests() {
            let r = tiny_http::Response::from_string(OPENER)
                .with_header(tiny_http::Header::from_bytes("Content-Type", "text/html; charset=utf-8").unwrap());
            let _ = req.respond(r);
        }
    });
    port
}

async fn eval(c: &cdp::Client, session: &str, expression: &str) -> Value {
    let r = tokio::time::timeout(Duration::from_secs(5), c.call(Some(session), &runtime::EvaluateParams {
        return_by_value: Some(true), await_promise: Some(true),
        ..runtime::EvaluateParams::new(expression.into())
    })).await;
    match r {
        Ok(Ok(r)) => match r.exception_details { Some(e) => json!({"exception": e.text}), None => r.result.value.unwrap_or(Value::Null) },
        Ok(Err(e)) => json!({"error": e.to_string()}),
        Err(_) => json!({"error": "no answer within 5 s"}),
    }
}

async fn now_us(c: &cdp::Client, session: &str) -> Option<String> {
    tokio::time::timeout(Duration::from_secs(5), c.call(Some(session), &emulation::GetVirtualTimeParams::default())).await
        .ok().and_then(|r| r.ok()).map(|t| t.unix_time_us)
}

#[tokio::test]
async fn noopener_window_probe() {
    let root = support::repo_root();
    let platform = browser_pin::current_platform().unwrap();
    let (binary, manifest) = browser_pin::engine::patched_binary(&root, platform).unwrap().expect("a patched engine build");
    let port = serve();
    let args: Vec<String> = [
        "--disable-frame-rate-limit", "--disable-gpu-vsync",
        "--disable-features=DeferRendererTasksAfterInput,PaintHolding",
        "--disable-site-isolation-trials", "--renderer-process-limit=1", "--disable-smooth-scrolling",
        "--enable-features=FrontcapTimeSeed:seed/1,FrontcapDeferredCompletion,FrontcapFramesWhileFrozen,FrontcapBeginFramesOnDemand,FrontcapVirtualNavigation,FrontcapVirtualResourceTiming,FrontcapVirtualWorkers,FrontcapVirtualImageAnimation,FrontcapObserverFrames,FrontcapFramesAfterWorkers",
        "--disable-threaded-animation", "--run-all-compositor-stages-before-draw",
    ].iter().map(|s| s.to_string()).collect();
    let launched = browser_pin::launch(&binary, &args, Duration::from_secs(30)).unwrap();
    let c = cdp::Client::connect(&launched.browser_ws_url).await.unwrap();
    let mut attached = c.subscribe::<target::AttachedToTargetEvent>(None);
    c.call(None, &target::SetAutoAttachParams {
        flatten: Some(true),
        filter: Some(target::TargetFilter(vec![
            target::FilterEntry { r#type: Some("page".into()), exclude: Some(false) },
            target::FilterEntry { r#type: None, exclude: Some(true) },
        ])),
        ..target::SetAutoAttachParams::new(true, true)
    }).await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(5), attached.next()).await.unwrap().unwrap();
    let opener = first.session_id.0.clone();
    c.call(Some(&opener), &page::EnableParams::default()).await.unwrap();
    c.call(Some(&opener), &runtime::EnableParams::default()).await.unwrap();
    c.call(Some(&opener), &emulation::SetVirtualTimePolicyParams {
        initial_virtual_time: Some(cdp::protocol::network::TimeSinceEpoch(1_700_000_000.0)),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)
    }).await.unwrap();
    let mut loaded = c.subscribe::<page::LoadEventFiredEvent>(Some(&opener));
    c.call(Some(&opener), &page::NavigateParams::new(format!("http://127.0.0.1:{port}/opener.html"))).await.unwrap();
    for _ in 0..40 {
        if tokio::time::timeout(Duration::from_millis(50), loaded.next()).await.is_ok() { break; }
        let _ = c.call(Some(&opener), &emulation::SetVirtualTimePolicyParams { budget: Some(50.0), ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending) }).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    println!("NOOPENER_PROBE {}", json!({"engine": manifest.patchset, "opener": eval(&c, &opener, "({now: Date.now(), ready: document.readyState})").await}));

    let iterations: usize = std::env::var("PROBE_ITERATIONS").ok().and_then(|s| s.parse().ok()).unwrap_or(10);
    for how in ["noopener", "link"] {
        for iteration in 0..iterations {
            // A timer opens the window 30 ms into a series of 16 ms steps.
            eval(&c, &opener, &format!("later(30, '{how}'); true")).await;
            let mut expired = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(Some(&opener));
            let mut step = 0;
            let mut found = None;
            while step < 10 && found.is_none() {
                step += 1;
                c.call(Some(&opener), &emulation::SetVirtualTimePolicyParams { budget: Some(16.0), ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending) }).await.unwrap();
                let end = tokio::time::timeout(Duration::from_secs(5), expired.next_tagged()).await.unwrap().unwrap();
                // Every attach that arrived before this expiry is already queued in its subscription.
                while let Ok(Some(a)) = attached.try_next_tagged() {
                    found = Some((step, a.arrival.0 < end.arrival.0, a.event));
                }
            }
            let Some((step, before_expiry, a)) = found else {
                println!("NOOPENER_PROBE {}", json!({"how": how, "iteration": iteration, "attached": false}));
                continue;
            };
            // Arrivals that came later than the step's expiry would show up only now.
            let late = tokio::time::timeout(Duration::from_millis(300), attached.next_tagged()).await.is_ok();
            let session = a.session_id.0.clone();
            let boundary = now_us(&c, &opener).await;
            // Enrolled while held: the renderer-bound commands are queued until the window has a frame.
            let s = Some(session.as_str());
            let _ = c.send(s, &page::EnableParams::default());
            let _ = c.send(s, &runtime::EnableParams::default());
            let _ = c.send(s, &page::AddScriptToEvaluateOnNewDocumentParams::new("window.__probeInstrumented = Date.now();".into()));
            let mut paused = c.subscribe::<fetch::RequestPausedEvent>(s);
            let mut navigated = c.subscribe::<page::FrameNavigatedEvent>(s);
            let fetch_enabled = tokio::time::timeout(Duration::from_secs(5), c.call(s, &fetch::EnableParams::default())).await.map(|r| r.is_ok());
            let _ = c.send(s, &runtime::RunIfWaitingForDebuggerParams::default());
            let request = tokio::time::timeout(Duration::from_secs(5), paused.next()).await;
            let request_url = request.as_ref().ok().and_then(|r| r.as_ref().ok()).map(|r| r.request.url.clone());
            if let Ok(Ok(r)) = request {
                let _ = c.call(s, &fetch::FulfillRequestParams {
                    response_headers: Some(vec![fetch::HeaderEntry { name: "content-type".into(), value: "text/html".into() }]),
                    body: Some(cdp::Binary(base64(POPUP.as_bytes()))),
                    ..fetch::FulfillRequestParams::new(r.request_id, 200)
                }).await;
            }
            // No grant: does the window commit with the clock stopped?
            let committed = tokio::time::timeout(Duration::from_secs(5), navigated.next()).await.map(|n| n.map(|n| n.frame.url));
            let clock_after_commit = now_us(&c, &opener).await;
            let before_grant = eval(&c, &session, "({ready: document.readyState, loadedAt: window.loadedAt ?? null})").await;
            c.call(Some(&opener), &emulation::SetVirtualTimePolicyParams { budget: Some(16.0), ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending) }).await.unwrap();
            let _ = tokio::time::timeout(Duration::from_secs(5), expired.next()).await;
            let after_grant = eval(&c, &session, "({ready: document.readyState, loadedAt: window.loadedAt ?? null, instrumented: window.__probeInstrumented ?? null, saw: window.sawInstrumentation, opener: !!window.opener})").await;
            println!("NOOPENER_PROBE {}", json!({
                "how": how, "iteration": iteration, "opened_in_step": step, "attach_before_expiry": before_expiry, "late_attach": late,
                "waiting": a.waiting_for_debugger, "can_access_opener": a.target_info.can_access_opener,
                "boundary_us": boundary, "fetch_enabled": format!("{fetch_enabled:?}"), "document_request": request_url,
                "committed_without_grant": format!("{committed:?}"), "clock_after_commit": clock_after_commit,
                "before_grant": before_grant, "after_grant": after_grant,
            }));
            let _ = c.call(None, &target::CloseTargetParams::new(a.target_info.target_id.clone())).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    drop(launched);
}

fn base64(bytes: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() { out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char) } else { out.push('=') }
        }
    }
    out
}
