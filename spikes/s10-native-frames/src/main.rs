//! While virtual time is paused: how long one native rendering frame takes in wall time, with and without
//! the frame rate limit, and whether the document timeline then reads the paused virtual instant.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, network, page, runtime};
use spike_common::*;
use std::time::Instant;

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn run(flags: &[String]) -> anyhow::Result<serde_json::Value> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(flags).await?;
    let c = &browser.client;
    let p = browser.new_page(false).await?;
    c.call(Some(&p.session), &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(c, &p, &format!("http://{addr}/index.html")).await?;
    advance(c, &p, 100.0).await?;
    let tree = c.call(Some(&p.session), &page::GetFrameTreeParams::default()).await?;
    let world = c.call(Some(&p.session), &page::CreateIsolatedWorldParams { world_name: Some("probe".into()), ..page::CreateIsolatedWorldParams::new(tree.frame_tree.frame.id) }).await?.execution_context_id;
    let frame = runtime::EvaluateParams { context_id: Some(world.clone()), await_promise: Some(true), return_by_value: Some(true), ..runtime::EvaluateParams::new("new Promise(r => requestAnimationFrame(() => r(document.timeline.currentTime)))".into()) };
    let mut walls = Vec::new();
    let mut timeline = Vec::new();
    for _ in 0..20 {
        let w = Instant::now();
        match tokio::time::timeout(std::time::Duration::from_secs(3), c.call(Some(&p.session), &frame)).await {
            Ok(r) => timeline.push(r?.result.value),
            Err(_) => { timeline.push(Some(serde_json::json!("timeout"))); break; }
        }
        walls.push(w.elapsed().as_secs_f64() * 1000.0);
    }
    if walls.is_empty() {
        // Does a native frame run once virtual time advances?
        let mut exp = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(Some(&p.session));
        let pending = tokio::spawn({ let c = c.clone(); let s = p.session.clone(); let f = frame.clone(); async move { c.call(Some(&s), &f).await.map(|r| r.result.value) } });
        c.call(Some(&p.session), &emulation::SetVirtualTimePolicyParams { budget: Some(1000.0), max_virtual_time_task_starvation_count: Some(1000), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Advance) }).await?;
        let _ = exp.next_timeout(std::time::Duration::from_secs(5)).await?;
        let got = tokio::time::timeout(std::time::Duration::from_secs(3), pending).await;
        return Ok(serde_json::json!({ "flags": flags, "paused_frame": "timeout", "frame_after_advance": format!("{got:?}") }));
    }
    let started = eval(c, &p, "document.getElementById('box').classList.add('on'); JSON.stringify(document.getAnimations().map(a => [a.pending, a.startTime]))").await?;
    let first_animated_frame = tokio::time::timeout(std::time::Duration::from_secs(3), c.call(Some(&p.session), &frame)).await.map(|r| r.map(|r| r.result.value));
    let after_frame = eval(c, &p, "JSON.stringify(document.getAnimations().map(a => [a.pending, a.startTime, getComputedStyle(document.getElementById('box')).opacity]))").await?;
    advance(c, &p, 50.0).await?;
    let later_frame = tokio::time::timeout(std::time::Duration::from_secs(3), c.call(Some(&p.session), &frame)).await.is_ok();
    let after_advance = eval(c, &p, "JSON.stringify([document.timeline.currentTime, document.getAnimations().map(a => [a.startTime, a.currentTime]), getComputedStyle(document.getElementById('box')).opacity])").await?;
    walls.sort_by(f64::total_cmp);
    Ok(serde_json::json!({
        "flags": flags,
        "frame_wall_ms_median": walls[walls.len() / 2],
        "frame_wall_ms_max": walls[walls.len() - 1],
        "timeline_while_paused_at_100": timeline,
        "on_class_added": started,
        "after_one_frame": after_frame,
        "first_animated_frame": format!("{first_animated_frame:?}"),
        "frame_after_advance_answered": later_frame,
        "after_advance_50_and_frame": after_advance,
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let plain = run(&[]).await?;
    let unlimited = run(&["--disable-frame-rate-limit".into(), "--disable-gpu-vsync".into()]).await?;
    report("s10-native-frames", serde_json::json!({ "default": plain, "unlimited": unlimited }));
    Ok(())
}
