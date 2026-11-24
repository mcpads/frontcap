//! S14 follow-up: what lets a wheel event through under virtual time.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, input, network};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn page(b: &Browser, url: &str) -> anyhow::Result<Page> {
    let c = &b.client;
    let p = b.new_page(false).await?;
    c.call(Some(&p.session), &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(c, &p, url).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    advance(c, &p, 50.0).await?;
    Ok(p)
}

fn wheel() -> input::DispatchMouseEventParams {
    input::DispatchMouseEventParams { delta_x: Some(0.0), delta_y: Some(400.0), ..input::DispatchMouseEventParams::new(input::DispatchMouseEventParamsType::MouseWheel, 300.0, 300.0) }
}

async fn try_variant(b: &Browser, url: &str, name: &str) -> anyhow::Result<serde_json::Value> {
    let c = b.client.clone();
    let p = page(b, url).await?;
    let s = p.session.clone();
    let (c2, s2) = (c.clone(), s.clone());
    let variant = name.to_string();
    let mut call = tokio::spawn(async move {
        match variant.as_str() {
            "gesture" => c2.call(Some(&s2), &input::SynthesizeScrollGestureParams { y_distance: Some(-400.0), speed: Some(100000), gesture_source_type: Some(input::GestureSourceType::Mouse), ..input::SynthesizeScrollGestureParams::new(300.0, 300.0) }).await.map(|_| ()),
            _ => c2.call(Some(&s2), &wheel()).await.map(|_| ()),
        }
    });
    let mut timeline = vec![];
    match name {
        "advance_200" | "gesture" => {
            advance(&c, &p, 200.0).await?;
        }
        "running_policy" => {
            // virtual time allowed to run (no budget) for a short wall interval, then paused again
            c.call(Some(&s), &emulation::SetVirtualTimePolicyParams { budget: Some(500.0), max_virtual_time_task_starvation_count: Some(1000), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Advance) }).await?;
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        _ => {
            tokio::time::sleep(Duration::from_millis(1000)).await;
        }
    }
    let answered = tokio::time::timeout(Duration::from_millis(500), &mut call).await.is_ok();
    timeline.push(eval(&c, &p, "JSON.stringify({y: window.scrollY, log})").await?);
    Ok(serde_json::json!({"answered": answered, "after": timeline}))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let url = format!("http://{addr}/index.html");
    let flags = vec!["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string()];
    let b = Browser::start(&flags).await?;
    let mut out = serde_json::Map::new();
    for prior in ["click_move_none", "click_move_bare", "click_wait_move_bare", "click_wheel"] {
        let mut lat = vec![];
        for _ in 0..3 {
            let p = page(&b, &url).await?;
            let s = Some(p.session.as_str());
            use input::DispatchMouseEventParamsType as M;
            let m = |k, x, y, n| input::DispatchMouseEventParams { button: Some(input::MouseButton::Left), click_count: Some(n), ..input::DispatchMouseEventParams::new(k, x, y) };
            match prior {
                "move" => { b.client.call(s, &m(M::MouseMoved, 61.0, 31.0, 0)).await?; }
                "click_move_none" => { for (k, n) in [(M::MousePressed, 1), (M::MouseReleased, 1)] { b.client.call(s, &m(k, 61.0, 31.0, n)).await?; } b.client.call(s, &input::DispatchMouseEventParams { button: Some(input::MouseButton::None), ..input::DispatchMouseEventParams::new(M::MouseMoved, 61.0, 31.0) }).await?; }
                "click_move_bare" => { for (k, n) in [(M::MousePressed, 1), (M::MouseReleased, 1)] { b.client.call(s, &m(k, 61.0, 31.0, n)).await?; } b.client.call(s, &input::DispatchMouseEventParams::new(M::MouseMoved, 62.0, 32.0)).await?; }
                "click_wait_move_bare" => { for (k, n) in [(M::MousePressed, 1), (M::MouseReleased, 1)] { b.client.call(s, &m(k, 61.0, 31.0, n)).await?; } eval(&b.client, &p, "0").await?; b.client.call(s, &input::DispatchMouseEventParams::new(M::MouseMoved, 62.0, 32.0)).await?; }
                "click_wheel" => { for (k, n) in [(M::MousePressed, 1), (M::MouseReleased, 1)] { b.client.call(s, &m(k, 61.0, 31.0, n)).await?; } }
                "focus_move" => { eval(&b.client, &p, "document.getElementById('q').focus()").await?; b.client.call(s, &m(M::MouseMoved, 61.0, 31.0, 0)).await?; }
                "insert_move" => { eval(&b.client, &p, "document.getElementById('q').focus()").await?; b.client.call(s, &input::InsertTextParams::new("x".into())).await?; b.client.call(s, &m(M::MouseMoved, 61.0, 31.0, 0)).await?; }
                "dbl_move" => { for (k, n) in [(M::MousePressed, 1), (M::MouseReleased, 1)] { b.client.call(s, &m(k, 61.0, 31.0, n)).await?; } b.client.call(s, &m(M::MouseMoved, 61.0, 31.0, 0)).await?; }
                "move_eval" => { b.client.call(s, &m(M::MouseMoved, 61.0, 31.0, 0)).await?; eval(&b.client, &p, "0").await?; }
                "move_wait" => { b.client.call(s, &m(M::MouseMoved, 61.0, 31.0, 0)).await?; tokio::time::sleep(Duration::from_millis(100)).await; }
                "click" => { b.client.call(s, &m(M::MousePressed, 61.0, 31.0, 1)).await?; b.client.call(s, &m(M::MouseReleased, 61.0, 31.0, 1)).await?; }
                "dblclick" => { for (k, n) in [(M::MousePressed, 1), (M::MouseReleased, 1), (M::MousePressed, 2), (M::MouseReleased, 2)] { b.client.call(s, &m(k, 61.0, 31.0, n)).await?; } }
                "key" => {
                    eval(&b.client, &p, "document.getElementById('q').focus()").await?;
                    b.client.call(s, &input::DispatchKeyEventParams { key: Some("a".into()), text: Some("a".into()), ..input::DispatchKeyEventParams::new(input::DispatchKeyEventParamsType::KeyDown) }).await?;
                    b.client.call(s, &input::DispatchKeyEventParams { key: Some("a".into()), ..input::DispatchKeyEventParams::new(input::DispatchKeyEventParamsType::KeyUp) }).await?;
                }
                "insert" => { eval(&b.client, &p, "document.getElementById('q').focus()").await?; b.client.call(s, &input::InsertTextParams::new("x".into())).await?; }
                "hangul" => { eval(&b.client, &p, "document.getElementById('q').focus()").await?; b.client.call(s, &input::InsertTextParams::new("한글".into())).await?; }
                "hangul_then_blur" => { eval(&b.client, &p, "document.getElementById('q').focus()").await?; b.client.call(s, &input::InsertTextParams::new("한글".into())).await?; eval(&b.client, &p, "document.activeElement.blur()").await?; }
                "eval_after" => { eval(&b.client, &p, "document.getElementById('q').focus()").await?; b.client.call(s, &input::InsertTextParams::new("x".into())).await?; eval(&b.client, &p, "document.getElementById('q').value").await?; }
                "focus" => { eval(&b.client, &p, "document.getElementById('q').focus()").await?; }
                _ => {}
            }
            let t = std::time::Instant::now();
            let r = tokio::time::timeout(Duration::from_secs(5), b.client.call(s, &wheel())).await;
            let y = eval(&b.client, &p, "window.scrollY").await?;
            lat.push(serde_json::json!({"answered_ms": r.is_ok().then(|| t.elapsed().as_millis() as u64), "y": y}));
        }
        out.insert(format!("latency_after_{prior}"), serde_json::Value::Array(lat));
    }
    for v in ["wall_1s", "advance_200", "running_policy", "gesture"] {
        out.insert(v.into(), try_variant(&b, &url, v).await?);
    }
    let b2 = Browser::start(&[]).await?;
    out.insert("default_flags_wall_1s".into(), try_variant(&b2, &url, "wall_1s").await?);
    let mut threaded = flags.clone();
    threaded.push("--disable-threaded-scrolling".into());
    let b3 = Browser::start(&threaded).await?;
    out.insert("no_threaded_scrolling_wall_1s".into(), try_variant(&b3, &url, "wall_1s").await?);
    report("s14-wheel", serde_json::Value::Object(out));
    Ok(())
}
