use cdp::protocol::{emulation::VirtualTimePolicy, page};
use spike_common::*;
use std::time::Duration;

async fn run(browser: &Browser, url: &str, capture: bool) -> anyhow::Result<serde_json::Value> {
    let c = &browser.client;
    let p = browser.new_page(false).await?;
    navigate(c, &p, url).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    set_policy(c, &p, VirtualTimePolicy::Pause, None).await?;
    eval(c, &p, "go()").await?;
    let mut samples = Vec::new();
    for step in 0..8 {
        eprintln!("[s5 capture={capture}] step {step}: advance");
        advance(c, &p, 50.0).await?;
        eprintln!("[s5 capture={capture}] step {step}: sample");
        let sample = eval(c, &p, "sample()").await?;
        eprintln!("[s5 capture={capture}] step {step}: fence");
        // Candidate completion barrier: does a double-rAF fence resolve while paused?
        let fence = tokio::time::timeout(Duration::from_secs(2), eval(c, &p, "rafFence()")).await;
        eprintln!("[s5 capture={capture}] step {step}: screenshot");
        let shot = if capture {
            let params = page::CaptureScreenshotParams {
                clip: Some(page::Viewport { x: 0.0, y: 0.0, width: 50.0, height: 50.0, scale: 1.0 }),
                ..page::CaptureScreenshotParams::default()
            };
            match tokio::time::timeout(Duration::from_secs(3), c.call(Some(&p.session), &params)).await {
                Ok(r) => Some(serde_json::json!(r?.data.0.len())),
                Err(_) => Some(serde_json::json!("timeout")),
            }
        } else {
            None
        };
        samples.push(serde_json::json!({ "sample": sample, "fence": fence.ok().map(|r| r.ok()), "shot_bytes": shot }));
    }
    let log = eval(c, &p, "JSON.stringify(window.__log)").await?;
    Ok(serde_json::json!({ "capture": capture, "samples": samples, "log": log }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(&[]).await?;
    let url = format!("http://{addr}/index.html");
    let with = run(&browser, &url, true).await?;
    let without = run(&browser, &url, false).await?;
    report("s5-time-level-capture", serde_json::json!({ "with_capture": with, "without_capture": without }));
    Ok(())
}
