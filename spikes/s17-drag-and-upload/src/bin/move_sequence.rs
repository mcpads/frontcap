//! A held drag as a sequence over virtual time: does each move's answer come, and when is each move
//! delivered? `move_then_advance`: move, then advance one frame interval; `advance_then_move`: the other
//! order. Answers are awaited only after the release.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, input, network};
use spike_common::*;
use std::time::{Duration, Instant};

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn run(b: &Browser, url: &str, move_first: bool, frame_ms: f64) -> anyhow::Result<serde_json::Value> {
    use input::DispatchMouseEventParamsType as M;
    let c = b.client.clone();
    let p = b.new_page(false).await?;
    let s = p.session.clone();
    c.call(Some(&s), &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(&c, &p, url).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    advance(&c, &p, 50.0).await?;
    let ev = |kind, x: f64, buttons| input::DispatchMouseEventParams { button: Some(if kind == M::MouseMoved && buttons == 0 { input::MouseButton::None } else { input::MouseButton::Left }), buttons: Some(buttons), click_count: Some(if kind == M::MouseMoved { 0 } else { 1 }), ..input::DispatchMouseEventParams::new(kind, x, 40.0) };
    c.call(Some(&s), &ev(M::MouseMoved, 40.0, 0)).await?;
    c.call(Some(&s), &ev(M::MousePressed, 40.0, 1)).await?;
    let start = Instant::now();
    let mut pending = vec![];
    let mut logs = vec![];
    for i in 1..=4 {
        if !move_first {
            advance(&c, &p, frame_ms).await?;
        }
        let (c2, s2) = (c.clone(), s.clone());
        pending.push(tokio::spawn(async move { c2.call(Some(&s2), &ev(M::MouseMoved, 40.0 + 25.0 * i as f64, 1)).await.map(|_| start.elapsed().as_millis()) }));
        tokio::time::sleep(Duration::from_millis(50)).await;
        if move_first {
            advance(&c, &p, frame_ms).await?;
        }
        logs.push(eval(&c, &p, "JSON.stringify(log.slice(2).map(e => e[0] + '@' + e.at(-1)))").await?);
    }
    let release = tokio::time::timeout(Duration::from_secs(3), c.call(Some(&s), &ev(M::MouseReleased, 140.0, 0))).await.is_ok();
    let mut answers = vec![];
    for h in pending {
        answers.push(match tokio::time::timeout(Duration::from_secs(2), h).await { Ok(Ok(Ok(ms))) => ms as i64, _ => -1 });
    }
    let log = eval(&c, &p, "JSON.stringify(log.slice(2).map(e => e[0] + '@' + e.at(-1)))").await?;
    Ok(serde_json::json!({"logs_after_each_step": logs, "release_answered": release, "move_answers_ms": answers, "log": log}))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let url = format!("http://{addr}/index.html");
    let b = Browser::start(&["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string(), "--disable-gpu".to_string()]).await?;
    let out = serde_json::json!({
        "move_then_advance_16": run(&b, &url, true, 16.0).await?,
        "advance_then_move_16": run(&b, &url, false, 16.0).await?,
        "move_then_advance_1": run(&b, &url, true, 1.0).await?,
    });
    report("s17-move-sequence", out);
    Ok(())
}
