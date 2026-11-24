use cdp::protocol::emulation::VirtualTimePolicy;
use spike_common::*;
use std::time::Instant;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(&[]).await?;
    let c = &browser.client;
    let p = browser.new_page(false).await?;
    navigate(c, &p, &format!("http://{addr}/index.html")).await?;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    set_policy(c, &p, VirtualTimePolicy::Pause, None).await?;
    eval(c, &p, "run()").await?;
    let wall = Instant::now();
    advance(c, &p, 60_001.0).await?;
    let wall_ms = wall.elapsed().as_millis();
    let done = eval(c, &p, "JSON.stringify(window.__done)").await?;
    report("s3-macos-virtual-time", serde_json::json!({ "wall_ms": wall_ms, "done": done }));
    Ok(())
}
