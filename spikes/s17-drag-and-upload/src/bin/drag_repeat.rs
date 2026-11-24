//! Is a held drag repeatable? `instant`: every move at one frozen instant (released together);
//! `stepped`: one frame interval of virtual time after each move. Each mode runs `RUNS` pages at once
//! (load), and the distinct pointer logs are counted.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, input, network};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn one(b: std::sync::Arc<Browser>, url: String, stepped: bool, moves: usize) -> anyhow::Result<String> {
    use input::DispatchMouseEventParamsType as M;
    let c = b.client.clone();
    let p = b.new_page(false).await?;
    let s = p.session.clone();
    c.call(Some(&s), &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(&c, &p, &url).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    advance(&c, &p, 50.0).await?;
    let ev = |kind, x: f64, held: bool| input::DispatchMouseEventParams { button: Some(if kind == M::MouseMoved && !held { input::MouseButton::None } else { input::MouseButton::Left }), buttons: Some(if held && kind == M::MouseMoved { 1 } else { 0 }), click_count: Some(if kind == M::MouseMoved { 0 } else { 1 }), ..input::DispatchMouseEventParams::new(kind, x, 40.0) };
    c.call(Some(&s), &ev(M::MouseMoved, 40.0, false)).await?;
    c.call(Some(&s), &ev(M::MousePressed, 40.0, true)).await?;
    let mut late = 0;
    for i in 1..=moves {
        if stepped {
            // A fresh instant has frames left: the move is delivered at it. Wait for that before the next.
            advance(&c, &p, 1000.0 / 60.0).await?;
        }
        c.send(Some(&s), &ev(M::MouseMoved, 40.0 + (100 * i / moves) as f64, true))?;
        if stepped {
            let want = 2 + i;
            let mut waited = 0;
            while eval(&c, &p, "log.length").await?.as_u64().unwrap_or(0) < want as u64 {
                tokio::time::sleep(Duration::from_millis(2)).await;
                waited += 2;
                if waited > 2000 { late += 1; break; }
            }
        }
    }
    if late > 0 { eprintln!("late {late}"); }
    c.call(Some(&s), &ev(M::MouseReleased, 140.0, true)).await?;
    let log = eval(&c, &p, "JSON.stringify(log.map(e => e.join(':')))").await?;
    Ok(log.as_str().unwrap_or_default().to_string())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let url = format!("http://{addr}/index.html");
    let runs: usize = std::env::var("RUNS").ok().and_then(|r| r.parse().ok()).unwrap_or(12);
    let b = std::sync::Arc::new(Browser::start(&["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string(), "--disable-gpu".to_string()]).await?);
    let mut out = serde_json::Map::new();
    for stepped in [true] {
        let tasks: Vec<_> = (0..runs).map(|_| tokio::spawn(one(b.clone(), url.clone(), stepped, 5))).collect();
        let mut counts = std::collections::BTreeMap::<String, usize>::new();
        for t in tasks {
            *counts.entry(t.await??).or_default() += 1;
        }
        out.insert(if stepped { "stepped" } else { "instant" }.into(), serde_json::to_value(counts)?);
    }
    report("s17-drag-repeat", serde_json::Value::Object(out));
    Ok(())
}
