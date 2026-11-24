//! S14 follow-up: which part of the adapter's page setup keeps a wheel event from being handled while
//! frozen.
use cdp::protocol::{dom, emulation::{self, VirtualTimePolicy}, fetch, input, network, page as cpage, runtime};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let url = format!("http://{addr}/index.html");
    let mut flags = vec!["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string()];
    if std::env::var("NO_GPU").is_ok() { flags.push("--disable-gpu".into()); }
    let b = Browser::start(&flags).await?;
    let mut out = serde_json::Map::new();
    for setup in ["input_page_center", "input_page_300", "index_center"] {
        let c = b.client.clone();
        let p = b.new_page(false).await?;
        let s = Some(p.session.as_str());
        match setup {
            "metrics" => { c.call(s, &emulation::SetDeviceMetricsOverrideParams::new(1280, 720, 1.0, false)).await?; }
            "metrics_dpr1" => { c.call(s, &emulation::SetDeviceMetricsOverrideParams::new(1280, 720, 2.0, false)).await?; }
            "fetch" => { c.call(s, &fetch::EnableParams { patterns: Some(vec![fetch::RequestPattern { url_pattern: Some("http://never.invalid/*".into()), ..Default::default() }]), ..Default::default() }).await?; }
            "dom_network" => { c.call(s, &dom::EnableParams::default()).await?; c.call(s, &network::EnableParams::default()).await?; }
            "binding_world" => {
                c.call(s, &runtime::AddBindingParams { execution_context_name: Some("w".into()), ..runtime::AddBindingParams::new("__b".into()) }).await?;
                c.call(s, &cpage::AddScriptToEvaluateOnNewDocumentParams { world_name: Some("w".into()), ..cpage::AddScriptToEvaluateOnNewDocumentParams::new("window.addEventListener('wheel', () => {}, true)".into()) }).await?;
            }
            "autoattach" => { c.call(s, &cdp::protocol::target::SetAutoAttachParams { flatten: Some(true), ..cdp::protocol::target::SetAutoAttachParams::new(true, false) }).await?; }
            _ => {}
        }
        c.call(s, &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
        c.call(s, &emulation::SetDeviceMetricsOverrideParams::new(1280, 720, 1.0, false)).await?;
        let page_url = if setup.starts_with("input") { format!("http://{addr}/input.html") } else { url.clone() };
        navigate(&c, &p, &page_url).await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        match setup {
            "pifnfp" | "advance_starvation" => {
                let mut expired = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(s);
                let policy = if setup == "pifnfp" { VirtualTimePolicy::PauseIfNetworkFetchesPending } else { VirtualTimePolicy::Advance };
                c.call(s, &emulation::SetVirtualTimePolicyParams { budget: Some(50.0), max_virtual_time_task_starvation_count: Some(1000), ..emulation::SetVirtualTimePolicyParams::new(policy) }).await?;
                expired.next_timeout(Duration::from_secs(5)).await?;
            }
            _ => advance(&c, &p, 50.0).await?,
        }
        let r = tokio::time::timeout(Duration::from_secs(2), c.call(s, &input::DispatchMouseEventParams { delta_x: Some(0.0), delta_y: Some(400.0), ..input::DispatchMouseEventParams::new(input::DispatchMouseEventParamsType::MouseWheel, if setup.ends_with("300") { 300.0 } else { 640.0 }, if setup.ends_with("300") { 300.0 } else { 360.0 }) })).await.is_ok();
        out.insert(setup.into(), serde_json::json!(r));
    }
    report("s14-wheel-env", serde_json::Value::Object(out));
    Ok(())
}
