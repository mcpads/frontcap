//! Follow-up to the review of S1: sequential freeze → hit-test → press → release → unfreeze against an
//! adversary that swaps the target on `resume` and in the first frame after it.
use cdp::protocol::{dom, emulation::VirtualTimePolicy, input, page};
use spike_common::*;
use std::time::Duration;

fn mouse(kind: input::DispatchMouseEventParamsType) -> input::DispatchMouseEventParams {
    input::DispatchMouseEventParams {
        button: Some(input::MouseButton::Left),
        click_count: Some(1),
        ..input::DispatchMouseEventParams::new(kind, 70.0, 30.0)
    }
}

pub async fn run(browser: &Browser, addr: std::net::SocketAddr) -> anyhow::Result<serde_json::Value> {
    let c = &browser.client;
    let p = browser.new_page(false).await?;
    navigate(c, &p, &format!("http://{addr}/adversary.html")).await?;
    // Settle wait: let the fixture load before the measurement starts (not a disturbance).
    tokio::time::sleep(Duration::from_millis(500)).await;
    set_policy(c, &p, VirtualTimePolicy::Pause, None).await?;
    let frozen = page::SetWebLifecycleStateParams::new(page::SetWebLifecycleStateParamsState::Frozen);
    let active = page::SetWebLifecycleStateParams::new(page::SetWebLifecycleStateParamsState::Active);
    let mut correct = 0;
    let mut wrong = Vec::new();
    let mut missed = 0;
    let mut blocked = None;
    for i in 0..100 {
        c.call(Some(&p.session), &frozen).await?;
        let hit = c.call(Some(&p.session), &dom::GetNodeForLocationParams::new(70, 30)).await?;
        let _ = hit;
        let g_hit = eval(c, &p, "window.__gen()").await?;
        let before = eval(c, &p, "window.__clicks.length").await?;
        let sent = tokio::time::timeout(Duration::from_secs(2), async {
            c.call(Some(&p.session), &mouse(input::DispatchMouseEventParamsType::MousePressed)).await?;
            c.call(Some(&p.session), &mouse(input::DispatchMouseEventParamsType::MouseReleased)).await
        })
        .await;
        if sent.is_err() {
            blocked = Some(format!("input dispatch blocked under freeze at iteration {i}"));
            c.call(Some(&p.session), &active).await?;
            break;
        }
        sent.unwrap()?;
        c.call(Some(&p.session), &active).await?;
        // Let the resume handler and the queued input run; virtual time stays paused.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let clicks: Vec<i64> = serde_json::from_value(eval(c, &p, "window.__clicks").await?)?;
        let n_before = before.as_u64().unwrap_or(0) as usize;
        match clicks.get(n_before) {
            Some(g) if serde_json::json!(g) == g_hit => correct += 1,
            Some(g) => wrong.push(serde_json::json!({ "iteration": i, "hit": g_hit, "clicked": g })),
            None => missed += 1,
        }
    }
    let lifecycle = eval(c, &p, "JSON.stringify(window.__lifecycle.slice(0, 6))").await?;

    // Does the freeze survive a navigation?
    let q = browser.new_page(false).await?;
    navigate(c, &q, &format!("http://{addr}/mutator.html?raf")).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    set_policy(c, &q, VirtualTimePolicy::Pause, None).await?;
    c.call(Some(&q.session), &frozen).await?;
    navigate(c, &q, &format!("http://{addr}/mutator.html?raf")).await?;
    // Injected disturbance: wall time after navigating a frozen, paused page.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after_nav = tokio::time::timeout(Duration::from_secs(3), eval(c, &q, "JSON.stringify({ready: document.readyState, gen: typeof window.__gen === 'function' ? window.__gen() : null})")).await;
    Ok(serde_json::json!({
        "sequential_freeze": { "correct": correct, "wrong": wrong, "missed": missed, "blocked": blocked },
        "lifecycle_events_seen_by_page": lifecycle,
        "after_navigation_while_frozen": after_nav.ok().and_then(|r| r.ok()),
    }))
}
