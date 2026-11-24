//! S16 follow-up: which virtual instant does each screencast frame show? The page paints a different
//! colour every 50 ms of virtual time; frames are saved for pixel checks.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, network, page};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let b = Browser::start(&["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string(), "--disable-gpu".to_string()]).await?;
    let c = &b.client;
    let p = b.new_page(false).await?;
    let s = Some(p.session.as_str());
    c.call(s, &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(c, &p, &format!("http://{addr}/color.html")).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut frames = c.subscribe::<page::ScreencastFrameEvent>(s);
    c.call(s, &page::StartScreencastParams { format: Some(page::StartScreencastParamsFormat::Png), every_nth_frame: Some(1), ..Default::default() }).await?;
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../results/s16-frames");
    std::fs::create_dir_all(&dir)?;
    let mut out = vec![];
    for step in 0..7 {
        advance(c, &p, if step == 0 { 1.0 } else { 50.0 }).await?;
        let t = eval(c, &p, "Date.now() - 946684800000").await?;
        let bg = eval(c, &p, "document.documentElement.style.background").await?;
        let mut got = vec![];
        while let Some(f) = frames.next_timeout(Duration::from_millis(100)).await? {
            let name = format!("step{step}-{}.png", got.len());
            use base64::Engine as _;
            let bytes = base64::engine::general_purpose::STANDARD.decode(f.data.0.as_bytes())?;
            std::fs::write(dir.join(&name), bytes)?;
            got.push(serde_json::json!({"file": name}));
            c.call(s, &page::ScreencastFrameAckParams::new(f.session_id)).await?;
        }
        out.push(serde_json::json!({"step": step, "virtual_ms": t, "background": bg, "frames": got}));
    }
    report("s16-frames", serde_json::json!({"steps": out}));
    Ok(())
}
