//! Native modal control prerequisites for a suspended, agent-answered operation.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, network, page};
use spike_common::*;
use std::time::Duration;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(&[
        "--enable-features=FrontcapTimeSeed:seed/1,FrontcapFramesWhileFrozen,FrontcapVirtualNavigation".into(),
    ]).await?;
    let c = &browser.client;
    let p = browser.new_page(false).await?;
    let s = Some(p.session.as_str());
    c.call(s, &emulation::SetVirtualTimePolicyParams {
        initial_virtual_time: Some(network::TimeSinceEpoch(946_684_800.0)),
        ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause)
    }).await?;
    navigate(c, &p, &format!("http://{addr}/index.html")).await?;
    let mut dialogs = c.subscribe::<page::JavascriptDialogOpeningEvent>(s);
    c.call(s, &emulation::SetVirtualTimePolicyParams {
        budget: Some(1000.0),
        ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Advance)
    }).await?;
    let opened = dialogs.next_timeout(Duration::from_secs(10)).await?
        .ok_or_else(|| anyhow::anyhow!("no native dialog"))?;
    let before = tokio::time::timeout(Duration::from_millis(500),
        c.call(s, &emulation::GetVirtualTimeParams::default())).await;
    let pause = tokio::time::timeout(Duration::from_millis(500),
        c.call(s, &emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause))).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = tokio::time::timeout(Duration::from_millis(500),
        c.call(s, &emulation::GetVirtualTimeParams::default())).await;
    let answer = tokio::time::timeout(Duration::from_secs(2),
        c.call(s, &page::HandleJavaScriptDialogParams::new(false))).await;
    let state = tokio::time::timeout(Duration::from_secs(2), eval(c, &p,
        "({now:Date.now()-946684800000,log})")).await;
    report("dialog-serviceability", serde_json::json!({
        "kind":format!("{:?}", opened.r#type), "message":opened.message,
        "clock_before":format!("{before:?}"), "pause":format!("{pause:?}"),
        "clock_after_host_delay":format!("{after:?}"),
        "browser_answer":format!("{answer:?}"), "page_after_answer":format!("{state:?}"),
    }));
    Ok(())
}
