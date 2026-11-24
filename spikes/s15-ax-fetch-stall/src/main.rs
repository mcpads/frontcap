//! A fetch started by input after the accessibility tree was read while frozen, following some wall
//! time: does it complete under `pauseIfNetworkFetchesPending`, and what unblocks it?
use cdp::protocol::{accessibility, emulation::{self, VirtualTimePolicy}, fetch, input, network};
use spike_common::*;
use std::time::{Duration, Instant};

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn run(b: &Browser, url: &str, gap_ms: u64, before_click: &str, policy: VirtualTimePolicy, unblock: &str) -> anyhow::Result<serde_json::Value> {
    let c = &b.client;
    let p = b.new_page(false).await?;
    let s = Some(p.session.as_str());
    c.call(s, &fetch::EnableParams { patterns: Some(vec![fetch::RequestPattern { url_pattern: Some("https://api.example.test/*".into()), ..Default::default() }]), ..Default::default() }).await?;
    c.call(s, &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(c, &p, url).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    advance(c, &p, 500.0).await?;
    tokio::time::sleep(Duration::from_millis(gap_ms)).await;
    match before_click {
        "ax" | "ax_script" => { c.call(s, &accessibility::GetFullAxTreeParams::default()).await?; }
        r if r.starts_with("ax_pre_") => {
            c.call(s, &accessibility::GetFullAxTreeParams::default()).await?;
            match &r[7..] {
                "screenshot" => { let ok = tokio::time::timeout(Duration::from_secs(2), c.call(s, &cdp::protocol::page::CaptureScreenshotParams::default())).await.is_ok(); eprintln!("screenshot answered {ok}"); }
                "pause_again" => { c.call(s, &emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause)).await?; }
                "metrics" => { c.call(s, &emulation::SetDeviceMetricsOverrideParams::new(800, 600, 1.0, false)).await?; }
                "advance_1" => { advance(c, &p, 1.0).await?; }
                "budget_zero_pif" => {
                    let mut ex = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(s);
                    c.call(s, &emulation::SetVirtualTimePolicyParams { budget: Some(0.001), max_virtual_time_task_starvation_count: Some(1000), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::PauseIfNetworkFetchesPending) }).await?;
                    let _ = tokio::time::timeout(Duration::from_secs(1), ex.next()).await;
                }
                _ => {}
            }
        }
        "ax_advance" => { c.call(s, &accessibility::GetFullAxTreeParams::default()).await?; advance(c, &p, 20.0).await?; }
        "raf" => { let _ = tokio::time::timeout(Duration::from_millis(300), eval(c, &p, "new Promise(r => requestAnimationFrame(() => r(1)))")).await; }
        _ => {}
    }
    let mut paused = c.subscribe::<fetch::RequestPausedEvent>(s);
    let r = eval(c, &p, "(() => { const r = document.getElementById('save').getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; })()").await?;
    let (x, y) = (r[0].as_f64().unwrap(), r[1].as_f64().unwrap());
    let m = |k, n| input::DispatchMouseEventParams { button: Some(input::MouseButton::Left), click_count: Some(n), ..input::DispatchMouseEventParams::new(k, x, y) };
    if before_click == "ax_script" {
        eval(c, &p, "document.getElementById('save').click(); 0").await?;
    } else {
        c.call(s, &m(input::DispatchMouseEventParamsType::MousePressed, 1)).await?;
        c.call(s, &m(input::DispatchMouseEventParamsType::MouseReleased, 1)).await?;
    }
    // The fetch is issued while frozen; let it reach interception under the chosen policy.
    let mut expired = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(s);
    c.call(s, &emulation::SetVirtualTimePolicyParams { budget: Some(1000.0), max_virtual_time_task_starvation_count: Some(1000), ..emulation::SetVirtualTimePolicyParams::new(policy) }).await?;
    let wall = Instant::now();
    let mut log = vec![];
    loop {
        tokio::select! {
            ev = paused.next() => {
                let ev = ev?;
                log.push(format!("paused {} @{}ms", ev.request.method, wall.elapsed().as_millis()));
                c.call(s, &fetch::FulfillRequestParams { response_headers: Some(vec![fetch::HeaderEntry { name: "access-control-allow-origin".into(), value: "*".into() }]), body: Some(cdp::Binary("e30=".into())), ..fetch::FulfillRequestParams::new(ev.request_id, 200) }).await?;
            }
            e = expired.next() => { e?; log.push(format!("expired @{}ms", wall.elapsed().as_millis())); break; }
            _ = tokio::time::sleep(Duration::from_secs(3)) => { log.push("stalled 3 s".into()); break; }
        }
    }
    if log.last().map(|l| l.starts_with("stalled")).unwrap_or(false) && unblock != "none" {
        match unblock {
            "wall_2s" => tokio::time::sleep(Duration::from_secs(2)).await,
            "screenshot" => { let _ = tokio::time::timeout(Duration::from_secs(2), c.call(s, &cdp::protocol::page::CaptureScreenshotParams::default())).await; }
            "bring_to_front" => { c.call(s, &cdp::protocol::page::BringToFrontParams::default()).await?; }
            "focus_emulation" => { c.call(s, &emulation::SetFocusEmulationEnabledParams::new(true)).await?; }
            "lifecycle_active" => { c.call(s, &cdp::protocol::page::SetWebLifecycleStateParams::new(cdp::protocol::page::SetWebLifecycleStateParamsState::Active)).await?; }
            "mouse_move" => { c.call(s, &input::DispatchMouseEventParams::new(input::DispatchMouseEventParamsType::MouseMoved, 1.0, 1.0)).await?; }
            "frame_segments" => {
                for i in 0..20 {
                    advance(c, &p, 16.667).await?;
                    let toast = eval(c, &p, "!!document.querySelector('.toast')").await?;
                    let now = eval(c, &p, "Date.now() - 946684800000").await?;
                    if toast == true { log.push(format!("toast after segment {i} at {now}")); break; }
                }
            }
            _ => {}
        }
        let mut expired2 = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(s);
        c.call(s, &emulation::SetVirtualTimePolicyParams { budget: Some(1000.0), max_virtual_time_task_starvation_count: Some(1000), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::PauseIfNetworkFetchesPending) }).await?;
        let r = tokio::time::timeout(Duration::from_secs(3), expired2.next()).await;
        log.push(format!("after {unblock}: {}", if r.is_ok() { "expired" } else { "still stalled" }));
    }
    let t = eval(c, &p, "Date.now() - 946684800000").await?;
    let toast = eval(c, &p, "!!document.querySelector('.toast')").await?;
    let resp = eval(c, &p, "window.__resp ?? null").await?;
    Ok(serde_json::json!({"log": log, "virtual_now": t, "toast": toast, "response_at": resp}))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let url = format!("http://{addr}/index.html");
    let b = Browser::start(&["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string(), "--disable-gpu".to_string()]).await?;
    let mut out = serde_json::Map::new();
    use VirtualTimePolicy::*;
    for i in 0..4 {
        out.insert(format!("ax_frame_segments_{i}"), run(&b, &url, 0, "ax", PauseIfNetworkFetchesPending, "frame_segments").await?);
    }
    for i in 0..2 {
        out.insert(format!("no_ax_{i}"), run(&b, &url, 0, "none", PauseIfNetworkFetchesPending, "none").await?);
    }
    report("s15-ax-fetch-stall", serde_json::Value::Object(out));
    Ok(())
}
