use cdp::protocol::{emulation::VirtualTimePolicy, fetch};
use spike_common::*;
use std::time::Duration;

async fn scenario(browser: &Browser, url: &str, mode: &str) -> anyhow::Result<serde_json::Value> {
    let c = &browser.client;
    let p = browser.new_page(false).await?;
    navigate(c, &p, url).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut paused = c.subscribe::<fetch::RequestPausedEvent>(Some(&p.session));
    c.call(Some(&p.session), &fetch::EnableParams {
        patterns: Some(vec![fetch::RequestPattern { url_pattern: Some("*.json".into()), request_stage: Some(fetch::RequestStage::Request), resource_type: None }]),
        ..fetch::EnableParams::default()
    }).await?;
    let policy = if mode == "advance" || mode == "zero-latency" { VirtualTimePolicy::Advance } else { VirtualTimePolicy::PauseIfNetworkFetchesPending };
    set_policy(c, &p, VirtualTimePolicy::Pause, None).await?;
    eval(c, &p, &format!("run('{mode}')")).await?;
    let mut expired = c.subscribe::<cdp::protocol::emulation::VirtualTimeBudgetExpiredEvent>(Some(&p.session));
    set_policy(c, &p, policy, Some(1000.0)).await?;
    let mut handled = Vec::new();
    if mode == "reverse-order" || mode == "latency-segment" {
        let mut evs = Vec::new();
        for _ in 0..2 {
            if let Some(ev) = paused.next_timeout(Duration::from_secs(5)).await? { evs.push(ev); }
        }
        handled = evs.iter().map(|e| e.request.url.clone()).collect();
        if mode == "latency-segment" {
            // Both requests held; grant exactly 200 ms under advance, then fulfil while paused.
            let mut seg = c.subscribe::<cdp::protocol::emulation::VirtualTimeBudgetExpiredEvent>(Some(&p.session));
            set_policy(c, &p, VirtualTimePolicy::Advance, Some(200.0)).await?;
            seg.next_timeout(Duration::from_secs(10)).await?;
        }
        let order: Vec<_> = if mode == "reverse-order" { evs.iter().rev().collect() } else { evs.iter().collect() };
        for ev in order {
            c.call(Some(&p.session), &fetch::FulfillRequestParams {
                body: Some(cdp::Binary("e30=".into())),
                ..fetch::FulfillRequestParams::new(ev.request_id.clone(), 200)
            }).await?;
        }
        if mode == "latency-segment" {
            set_policy(c, &p, VirtualTimePolicy::PauseIfNetworkFetchesPending, Some(800.0)).await?;
        }
    }
    for _ in 0..(if mode == "reverse-order" || mode == "latency-segment" { 0 } else { 2 }) {
        let Some(ev) = paused.next_timeout(Duration::from_secs(5)).await? else { break };
        handled.push(ev.request.url.clone());
        match mode {
            // Fulfil immediately with zero latency.
            "zero-latency" | "zero-latency-pause-if-pending" => {
                c.call(Some(&p.session), &fetch::FulfillRequestParams {
                    body: Some(cdp::Binary("e30=".into())), // "{}"
                    ..fetch::FulfillRequestParams::new(ev.request_id.clone(), 200)
                }).await?;
            }
            // Injected disturbance: hold 300 ms wall, then continue to the real server.
            _ => {
                tokio::time::sleep(Duration::from_millis(300)).await;
                c.call(Some(&p.session), &fetch::ContinueRequestParams::new(ev.request_id.clone())).await?;
            }
        }
    }
    let budget_expired = expired.next_timeout(Duration::from_secs(10)).await?.is_some();
    // Let any remaining completion run so the log is complete before reading.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let log = eval(c, &p, "JSON.stringify(window.__log)").await?;
    Ok(serde_json::json!({ "mode": mode, "intercepted": handled, "budget_expired": budget_expired, "log": log }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(&[]).await?;
    let url = format!("http://{addr}/index.html");
    let mut runs = Vec::new();
    for mode in ["advance", "pause-if-pending", "zero-latency", "zero-latency-pause-if-pending", "reverse-order", "latency-segment"] {
        runs.push(scenario(&browser, &url, mode).await?);
    }
    report("s2-fetch-timing", serde_json::json!({ "runs": runs }));
    Ok(())
}
