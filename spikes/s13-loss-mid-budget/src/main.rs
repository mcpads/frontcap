//! A connection closed while a virtual-time budget is running: does time stop at the budget's end,
//! stop at once, or run on with the wall clock?
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, network, page, runtime, target};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn attach(c: &cdp::Client, target_id: &str) -> anyhow::Result<Page> {
    let a = c.call(None, &target::AttachToTargetParams { flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(target_id.into())) }).await?;
    Ok(Page { session: a.session_id.0, target_id: target_id.into() })
}

async fn now(c: &cdp::Client, p: &Page) -> anyhow::Result<serde_json::Value> {
    let r = c.call(Some(&p.session), &runtime::EvaluateParams { return_by_value: Some(true), ..runtime::EvaluateParams::new("Date.now() - 946684800000".into()) }).await?;
    Ok(r.result.value.unwrap_or_default())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(&[]).await?;
    let mut out = serde_json::Map::new();
    for (label, budget, policy) in [("advance-60000", 60_000.0, VirtualTimePolicy::Advance), ("pause-if-fetch-60000", 60_000.0, VirtualTimePolicy::PauseIfNetworkFetchesPending)] {
        let created = browser.client.call(None, &target::CreateTargetParams::new("about:blank".into())).await?;
        let target_id = created.target_id.0.clone();
        let a = cdp::Client::connect(&browser.launched.browser_ws_url).await?;
        let pa = attach(&a, &target_id).await?;
        let s = Some(pa.session.as_str());
        a.call(s, &page::EnableParams::default()).await?;
        a.call(s, &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
        navigate(&a, &pa, &format!("http://{addr}/index.html")).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        advance(&a, &pa, 10.0).await?;
        // A long budget, then the connection closes while it runs.
        a.call(s, &emulation::SetVirtualTimePolicyParams { budget: Some(budget), max_virtual_time_task_starvation_count: Some(1000), ..emulation::SetVirtualTimePolicyParams::new(policy) }).await?;
        tokio::time::sleep(Duration::from_millis(5)).await;
        a.close();
        drop(a);
        let b = cdp::Client::connect(&browser.launched.browser_ws_url).await?;
        let pb = attach(&b, &target_id).await?;
        let t0 = now(&b, &pb).await?;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let t1 = now(&b, &pb).await?;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let t2 = now(&b, &pb).await?;
        out.insert(label.into(), serde_json::json!({"at_reconnect": t0, "after_1s": t1, "after_2s": t2}));
    }
    report("s13-loss-mid-budget", serde_json::Value::Object(out));
    Ok(())
}
