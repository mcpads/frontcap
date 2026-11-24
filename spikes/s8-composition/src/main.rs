//! Composition of virtual-time budgets with Fetch interception and lifecycle freeze (follow-up to S2 and
//! the Plan 3 clock driver). Each scenario starts from a fresh page, frozen or not, at a known virtual time.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, fetch, network, page};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn now(c: &cdp::Client, p: &Page) -> anyhow::Result<f64> {
    Ok(eval(c, p, "Date.now()").await?.as_f64().unwrap() - ORIGIN_MS)
}

async fn lifecycle(c: &cdp::Client, p: &Page, frozen: bool) -> anyhow::Result<()> {
    let state = if frozen { page::SetWebLifecycleStateParamsState::Frozen } else { page::SetWebLifecycleStateParamsState::Active };
    c.call(Some(&p.session), &page::SetWebLifecycleStateParams::new(state)).await?;
    Ok(())
}

async fn budget(c: &cdp::Client, p: &Page, policy: VirtualTimePolicy, ms: f64) -> anyhow::Result<bool> {
    let mut expired = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(Some(&p.session));
    c.call(Some(&p.session), &emulation::SetVirtualTimePolicyParams { budget: Some(ms), max_virtual_time_task_starvation_count: Some(1000), ..emulation::SetVirtualTimePolicyParams::new(policy) }).await?;
    Ok(expired.next_timeout(Duration::from_secs(5)).await?.is_some())
}

async fn scenario(browser: &Browser, url: &str, use_freeze: bool, held_policy: VirtualTimePolicy, fulfil_after_ms: f64) -> anyhow::Result<serde_json::Value> {
    let c = &browser.client;
    let p = browser.new_page(false).await?;
    c.call(Some(&p.session), &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(c, &p, url).await?;
    budget(c, &p, VirtualTimePolicy::Advance, 50.0).await?;
    let mut paused = c.subscribe::<fetch::RequestPausedEvent>(Some(&p.session));
    c.call(Some(&p.session), &fetch::EnableParams { patterns: Some(vec![fetch::RequestPattern { url_pattern: Some("*api.example.test*".into()), request_stage: Some(fetch::RequestStage::Request), ..Default::default() }]), ..Default::default() }).await?;
    let mut steps = Vec::new();
    if use_freeze { lifecycle(c, &p, true).await?; }
    eval(c, &p, "timers(); go()").await?;
    let t0 = now(c, &p).await?;
    if use_freeze { lifecycle(c, &p, false).await?; }
    // Grant time while the request is held.
    let held = paused.next_timeout(Duration::from_secs(5)).await?.expect("intercepted");
    let expired = budget(c, &p, held_policy, fulfil_after_ms).await?;
    if use_freeze { lifecycle(c, &p, true).await?; }
    let t1 = now(c, &p).await?;
    steps.push(serde_json::json!({ "phase": "held", "policy": format!("{held_policy:?}"), "granted": fulfil_after_ms, "expired": expired, "elapsed": t1 - t0 }));
    c.call(Some(&p.session), &fetch::FulfillRequestParams { response_headers: Some(vec![fetch::HeaderEntry { name: "access-control-allow-origin".into(), value: "*".into() }]), body: Some(cdp::Binary("b2s=".into())), ..fetch::FulfillRequestParams::new(held.request_id, 200) }).await?;
    // Grant 1 ms steps under pauseIfNetworkFetchesPending until the response is consumed.
    for _ in 0..5 {
        if use_freeze { lifecycle(c, &p, false).await?; }
        let e = budget(c, &p, VirtualTimePolicy::PauseIfNetworkFetchesPending, 1.0).await?;
        if use_freeze { lifecycle(c, &p, true).await?; }
        let tn = now(c, &p).await?;
        steps.push(serde_json::json!({ "phase": "after-fulfil", "granted": 1.0, "expired": e, "elapsed": tn - t0 }));
    }
    let log = eval(c, &p, "JSON.stringify(window.__log)").await?;
    Ok(serde_json::json!({ "freeze": use_freeze, "t0": t0, "steps": steps, "log": log }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(&[]).await?;
    let url = format!("http://{addr}/index.html");
    let mut runs = Vec::new();
    for use_freeze in [false, true] {
        for policy in [VirtualTimePolicy::Advance, VirtualTimePolicy::PauseIfNetworkFetchesPending] {
            runs.push(scenario(&browser, &url, use_freeze, policy, 500.0).await.unwrap_or_else(|e| serde_json::json!({ "freeze": use_freeze, "policy": format!("{policy:?}"), "error": e.to_string() })));
        }
    }
    report("s8-composition", serde_json::json!({ "runs": runs }));
    Ok(())
}
