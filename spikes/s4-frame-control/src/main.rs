use cdp::protocol::{emulation::VirtualTimePolicy, headless_experimental};
use spike_common::*;

const INTERVAL_MS: f64 = 16.667;

async fn run(browser: &Browser, url: &str, record: bool) -> anyhow::Result<serde_json::Value> {
    let c = &browser.client;
    let p = browser.new_page(true).await?;
    let base = set_policy(c, &p, VirtualTimePolicy::Pause, None).await?;
    navigate(c, &p, url).await?;
    // Let the document load under frame control by pumping frames.
    let mut t = 0.0;
    for _ in 0..30 {
        advance(c, &p, INTERVAL_MS).await?;
        t += INTERVAL_MS;
        c.call(Some(&p.session), &headless_experimental::BeginFrameParams {
            frame_time_ticks: Some(base + t), interval: Some(INTERVAL_MS),
            ..headless_experimental::BeginFrameParams::default()
        }).await?;
    }
    eval(c, &p, "start()").await?;
    let mut hashes = Vec::new();
    let mut frames = 0;
    loop {
        advance(c, &p, INTERVAL_MS).await?;
        t += INTERVAL_MS;
        let r = c.call(Some(&p.session), &headless_experimental::BeginFrameParams {
            frame_time_ticks: Some(base + t), interval: Some(INTERVAL_MS),
            screenshot: record.then(|| headless_experimental::ScreenshotParams {
                format: Some(headless_experimental::ScreenshotParamsFormat::Png), quality: None, optimize_for_speed: None,
            }),
            ..headless_experimental::BeginFrameParams::default()
        }).await?;
        if let Some(data) = r.screenshot_data {
            use std::hash::{Hash, Hasher};
            // DefaultHasher::new() uses fixed keys, so digests are comparable across runs of one build.
            let mut h = std::collections::hash_map::DefaultHasher::new();
            data.0.hash(&mut h);
            hashes.push(format!("{:016x}", h.finish()));
        }
        frames += 1;
        if eval(c, &p, "window.__done").await? == serde_json::Value::Bool(true) || frames > 200 {
            break;
        }
    }
    let raf = eval(c, &p, "JSON.stringify(window.__frames)").await?;
    Ok(serde_json::json!({ "record": record, "frames_pumped": frames, "raf_timestamps": raf, "capture_digest": hashes }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let args = vec!["--enable-begin-frame-control".to_string(), "--run-all-compositor-stages-before-draw".to_string(),
                    "--disable-gpu".to_string(), "--no-sandbox".to_string()];
    let browser = Browser::start(&args).await?;
    let url = format!("http://{addr}/index.html");
    let a = run(&browser, &url, true).await?;
    let b = run(&browser, &url, true).await?;
    let off = run(&browser, &url, false).await?;
    report("s4-frame-control", serde_json::json!({ "run_a": a, "run_b": b, "recording_off": off }));
    Ok(())
}
