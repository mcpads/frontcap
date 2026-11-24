//! When does `Input.dispatchMouseEvent` answer a move with the button held while frozen: after the
//! next advance, or never? Variants: `button: left` vs `button: none` with `buttons: 1`.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, input, network};
use spike_common::*;
use std::time::{Duration, Instant};

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn run(b: &Browser, url: &str, button: input::MouseButton, advance_ms: f64) -> anyhow::Result<serde_json::Value> {
    use input::DispatchMouseEventParamsType as M;
    let c = b.client.clone();
    let p = b.new_page(false).await?;
    let s = p.session.clone();
    c.call(Some(&s), &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(&c, &p, url).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    advance(&c, &p, 50.0).await?;
    let ev = |kind, x: f64, button, buttons| input::DispatchMouseEventParams { button: Some(button), buttons: Some(buttons), click_count: Some(if kind == M::MouseMoved { 0 } else { 1 }), ..input::DispatchMouseEventParams::new(kind, x, 40.0) };
    c.call(Some(&s), &ev(M::MouseMoved, 40.0, input::MouseButton::None, 0)).await?;
    c.call(Some(&s), &ev(M::MousePressed, 40.0, input::MouseButton::Left, 1)).await?;
    let start = Instant::now();
    let (c2, s2) = (c.clone(), s.clone());
    let mv = tokio::spawn(async move { c2.call(Some(&s2), &ev(M::MouseMoved, 90.0, button, 1)).await.map(|_| start.elapsed().as_millis()) });
    tokio::time::sleep(Duration::from_millis(500)).await;
    let answered_before = mv.is_finished();
    let log_before = eval(&c, &p, "JSON.stringify(log)").await?;
    let mut advanced_ms = None;
    if advance_ms > 0.0 {
        let t = Instant::now();
        let r = tokio::time::timeout(Duration::from_secs(3), advance(&c, &p, advance_ms)).await;
        advanced_ms = Some(if r.is_ok() { t.elapsed().as_millis() as i64 } else { -1 });
    }
    let answered = tokio::time::timeout(Duration::from_secs(3), mv).await;
    let move_ms = match answered { Ok(Ok(Ok(ms))) => ms as i64, _ => -1 };
    let up = tokio::time::timeout(Duration::from_secs(3), c.call(Some(&s), &ev(M::MouseReleased, 90.0, input::MouseButton::Left, 0))).await.is_ok();
    let log = eval(&c, &p, "JSON.stringify(log)").await?;
    Ok(serde_json::json!({"answered_within_500ms": answered_before, "log_at_500ms": log_before, "advance_wall_ms": advanced_ms, "move_answer_ms": move_ms, "release_answered": up, "log": log}))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let url = format!("http://{addr}/index.html");
    let b = Browser::start(&["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string(), "--disable-gpu".to_string()]).await?;
    let mut out = serde_json::Map::new();
    for (name, button, adv) in [("left_no_advance", input::MouseButton::Left, 0.0), ("left_advance_16", input::MouseButton::Left, 16.0), ("left_advance_1", input::MouseButton::Left, 1.0), ("none_buttons1_no_advance", input::MouseButton::None, 0.0)] {
        out.insert(name.into(), run(&b, &url, button, adv).await?);
    }
    report("s17-move-answers", serde_json::Value::Object(out));
    Ok(())
}
