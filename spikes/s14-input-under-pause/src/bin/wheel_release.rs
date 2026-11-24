//! S14 follow-up: a wheel dispatched after a click and a move is not answered while frozen; what
//! releases it.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, input, network};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let url = format!("http://{addr}/index.html");
    let b = Browser::start(&["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string()]).await?;
    let mut out = serde_json::Map::new();
    use input::DispatchMouseEventParamsType as M;
    for release in ["raf_click_move_far", "raf_div_click_move", "raf_div_click_move_far", "raf_click_wait_move"] {
        let c = b.client.clone();
        let p = b.new_page(false).await?;
        let s = p.session.clone();
        c.call(Some(&s), &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
        navigate(&c, &p, &url).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        advance(&c, &p, 50.0).await?;
        if release.starts_with("raf") {
            let m = |k, x, y, n| input::DispatchMouseEventParams { button: Some(input::MouseButton::Left), click_count: Some(n), ..input::DispatchMouseEventParams::new(k, x, y) };
            if release.contains("premove") {
                c.call(Some(&s), &input::DispatchMouseEventParams::new(M::MouseMoved, 61.0, 31.0)).await?;
            }
            let (cx, cy) = if release.contains("div") { (61.0, 221.0) } else { (61.0, 31.0) };
            c.call(Some(&s), &m(M::MousePressed, cx, cy, 1)).await?;
            c.call(Some(&s), &m(M::MouseReleased, cx, cy, 1)).await?;
            if release.contains("wait") { advance(&c, &p, 100.0).await?; }
            if release.contains("click_move") {
                let (x, y) = if release.ends_with("far") { (300.0, 300.0) } else { (cx + 1.0, cy + 1.0) };
                c.call(Some(&s), &input::DispatchMouseEventParams::new(M::MouseMoved, x, y)).await?;
            }
            if release.ends_with("twice") {
                c.call(Some(&s), &input::DispatchMouseEventParams::new(M::MouseMoved, 63.0, 33.0)).await?;
            }
            let r = tokio::time::timeout(Duration::from_secs(3), eval(&c, &p, "new Promise(r => requestAnimationFrame(() => r(0)))")).await.is_ok();
            out.insert(release.into(), serde_json::json!({"raf_ran_while_frozen": r}));
            continue;
        }
        let m = |k, x, y, n| input::DispatchMouseEventParams { button: Some(input::MouseButton::Left), click_count: Some(n), ..input::DispatchMouseEventParams::new(k, x, y) };
        c.call(Some(&s), &m(M::MousePressed, 61.0, 31.0, 1)).await?;
        c.call(Some(&s), &m(M::MouseReleased, 61.0, 31.0, 1)).await?;
        c.call(Some(&s), &input::DispatchMouseEventParams::new(M::MouseMoved, 62.0, 32.0)).await?;
        let (wx, wy) = if release == "wheel_at_last_point" { (62.0, 32.0) } else { (300.0, 300.0) };
        if release == "pre_move_then_wheel" {
            c.call(Some(&s), &input::DispatchMouseEventParams::new(M::MouseMoved, 300.0, 300.0)).await?;
        }
        let (c2, s2) = (c.clone(), s.clone());
        let mut wheel = tokio::spawn(async move { c2.call(Some(&s2), &input::DispatchMouseEventParams { delta_x: Some(0.0), delta_y: Some(400.0), ..input::DispatchMouseEventParams::new(M::MouseWheel, wx, wy) }).await.map(|_| ()) });
        let frozen = tokio::time::timeout(Duration::from_millis(300), &mut wheel).await.is_ok();
        let t0 = eval(&c, &p, "Date.now() - 946684800000").await?;
        eprintln!("{release}: frozen={frozen} t0={t0}");
        let step = tokio::time::timeout(Duration::from_secs(5), async {
            match release {
                r if r.starts_with("advance_") => advance(&c, &p, r[8..].parse().unwrap()).await,
                "wall_3s" => { tokio::time::sleep(Duration::from_secs(3)).await; Ok(()) }
                "move_again" => c.call(Some(&s), &input::DispatchMouseEventParams::new(M::MouseMoved, 300.0, 300.0)).await.map(|_| ()).map_err(Into::into),
                _ => eval(&c, &p, "new Promise(r => requestAnimationFrame(() => r(0)))").await.map(|_| ()),
            }
        }).await;
        eprintln!("{release}: step {:?}", step.as_ref().map(|r| r.is_ok()));
        let released = tokio::time::timeout(Duration::from_millis(1000), &mut wheel).await.is_ok();
        let t1 = tokio::time::timeout(Duration::from_secs(2), eval(&c, &p, "Date.now() - 946684800000")).await.ok().and_then(|r| r.ok());
        let y = tokio::time::timeout(Duration::from_secs(2), eval(&c, &p, "JSON.stringify({y: scrollY, log})")).await.ok().and_then(|r| r.ok());
        out.insert(release.into(), serde_json::json!({"answered_frozen": frozen, "step_completed": format!("{:?}", step.map(|r| r.is_ok())), "released": released, "t_before": t0, "t_after": t1, "page": y}));
    }
    report("s14-wheel-release", serde_json::Value::Object(out));
    Ok(())
}
