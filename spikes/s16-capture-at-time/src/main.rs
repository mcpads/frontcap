//! Capture at level `time`: does a screenshot (or a screencast) change what the page does — transition
//! progress, frame callbacks, a fetch after input (S15) — and does it answer?
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, fetch, input, network, page};
use spike_common::*;
use std::time::{Duration, Instant};

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn run(b: &Browser, url: &str, mode: &str) -> anyhow::Result<serde_json::Value> {
    let c = &b.client;
    let p = b.new_page(false).await?;
    let s = Some(p.session.as_str());
    c.call(s, &fetch::EnableParams { patterns: Some(vec![fetch::RequestPattern { url_pattern: Some("https://api.example.test/*".into()), ..Default::default() }]), ..Default::default() }).await?;
    c.call(s, &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(c, &p, url).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut frames = c.subscribe::<page::ScreencastFrameEvent>(s);
    if mode == "screencast" {
        c.call(s, &page::StartScreencastParams { format: Some(page::StartScreencastParamsFormat::Png), every_nth_frame: Some(1), ..Default::default() }).await?;
    }
    let mut opacity = vec![];
    let mut capture_ms = vec![];
    let mut screencast_frames = 0;
    for _ in 0..6 {
        advance(c, &p, 50.0).await?;
        if mode == "screenshot" {
            let t = Instant::now();
            let r = tokio::time::timeout(Duration::from_secs(3), c.call(s, &page::CaptureScreenshotParams::default())).await;
            capture_ms.push(if r.is_ok() { t.elapsed().as_millis() as i64 } else { -1 });
        }
        while let Some(f) = frames.next_timeout(Duration::from_millis(30)).await? {
            screencast_frames += 1;
            c.call(s, &page::ScreencastFrameAckParams::new(f.session_id)).await?;
        }
        opacity.push(eval(c, &p, "getComputedStyle(document.getElementById('box')).opacity").await?);
    }
    // Input then a fetch: does it get through (S15)?
    let r = eval(c, &p, "(() => { const r = document.getElementById('save').getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; })()").await?;
    let (x, y) = (r[0].as_f64().unwrap(), r[1].as_f64().unwrap());
    let m = |k, n| input::DispatchMouseEventParams { button: Some(input::MouseButton::Left), click_count: Some(n), ..input::DispatchMouseEventParams::new(k, x, y) };
    let mut paused = c.subscribe::<fetch::RequestPausedEvent>(s);
    c.call(s, &m(input::DispatchMouseEventParamsType::MousePressed, 1)).await?;
    c.call(s, &m(input::DispatchMouseEventParamsType::MouseReleased, 1)).await?;
    let mut expired = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(s);
    c.call(s, &emulation::SetVirtualTimePolicyParams { budget: Some(200.0), max_virtual_time_task_starvation_count: Some(1000), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::PauseIfNetworkFetchesPending) }).await?;
    let mut fetch_log = vec![];
    loop {
        tokio::select! {
            ev = paused.next() => { let ev = ev?; c.call(s, &fetch::FulfillRequestParams { response_headers: Some(vec![fetch::HeaderEntry { name: "access-control-allow-origin".into(), value: "*".into() }]), body: Some(cdp::Binary("b2s=".into())), ..fetch::FulfillRequestParams::new(ev.request_id, 200) }).await?; fetch_log.push("answered"); }
            e = expired.next() => { e?; fetch_log.push("expired"); break; }
            _ = tokio::time::sleep(Duration::from_secs(2)) => { fetch_log.push("stalled"); break; }
        }
    }
    let rafs = eval(c, &p, "rafs.slice(0, 30)").await?;
    let resp = eval(c, &p, "window.__resp ?? null").await?;
    Ok(serde_json::json!({"opacity": opacity, "capture_ms": capture_ms, "screencast_frames": screencast_frames, "rafs": rafs, "fetch": fetch_log, "response_at": resp}))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let url = format!("http://{addr}/index.html");
    let b = Browser::start(&["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string(), "--disable-gpu".to_string()]).await?;
    let mut out = serde_json::Map::new();
    for mode in ["none", "screenshot", "screencast", "none"] {
        let key = if out.contains_key(mode) { format!("{mode}_again") } else { mode.to_string() };
        out.insert(key, run(&b, &url, mode).await?);
    }
    report("s16-capture-at-time", serde_json::Value::Object(out));
    Ok(())
}
