//! S14 follow-up: how many main frames run while frozen, and what consumes them.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, input, network};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let url = format!("http://{addr}/index.html");
    let b = Browser::start(&["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string(), "--disable-gpu".to_string()]).await?;
    let mut out = serde_json::Map::new();
    for seq in ["raf,raf,raf", "wheel,wheel", "raf,wheel", "wheel,raf", "dom,raf,raf", "style,raf"] {
        let c = b.client.clone();
        let p = b.new_page(false).await?;
        let s = Some(p.session.as_str());
        c.call(s, &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
        navigate(&c, &p, &url).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        advance(&c, &p, 50.0).await?;
        let mut results = vec![];
        for op in seq.split(',') {
            let ok = match op {
                "raf" => tokio::time::timeout(Duration::from_millis(800), eval(&c, &p, "new Promise(r => requestAnimationFrame(() => r(1)))")).await.is_ok(),
                "wheel" => tokio::time::timeout(Duration::from_millis(800), c.call(s, &input::DispatchMouseEventParams { delta_x: Some(0.0), delta_y: Some(100.0), ..input::DispatchMouseEventParams::new(input::DispatchMouseEventParamsType::MouseWheel, 300.0, 300.0) })).await.is_ok(),
                "dom" => { eval(&c, &p, "document.body.appendChild(document.createElement('p')).textContent = 'x'").await?; true }
                "style" => { eval(&c, &p, "document.body.style.background = 'red'").await?; true }
                _ => unreachable!(),
            };
            results.push(format!("{op}:{ok}"));
            if !ok { break; }
        }
        out.insert(seq.into(), serde_json::json!(results));
    }
    report("s14-frames-budget", serde_json::Value::Object(out));
    Ok(())
}
