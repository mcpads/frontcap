//! Dialogs under virtual time (alert, confirm, prompt): when the opening event arrives, whether virtual
//! time keeps running while a dialog is open, and what the page sees after the platform dismisses it.
//! Also: whether the accessibility tree can be read while virtual time is paused, and how long it takes.
use cdp::protocol::{accessibility, emulation::{self, VirtualTimePolicy}, network, page};
use spike_common::*;
use std::time::{Duration, Instant};

const ORIGIN_MS: f64 = 946_684_800_000.0;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(&[]).await?;
    let c = &browser.client;
    let p = browser.new_page(false).await?;
    let s = Some(p.session.as_str());
    c.call(s, &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(c, &p, &format!("http://{addr}/index.html")).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut dialogs = c.subscribe::<page::JavascriptDialogOpeningEvent>(s);
    let mut expired = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(s);
    let mut seen = Vec::new();
    c.call(s, &emulation::SetVirtualTimePolicyParams { budget: Some(1000.0), max_virtual_time_task_starvation_count: Some(1000), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Advance) }).await?;
    let wall = Instant::now();
    let mut answers = vec![(true, None), (false, None), (true, Some("typed".to_string()))];
    loop {
        tokio::select! {
            d = dialogs.next() => {
                let d = d?;
                // While the dialog is open, can we evaluate? Does the budget expire?
                let probe = tokio::time::timeout(Duration::from_millis(500), eval(c, &p, "Date.now() - 946684800000")).await;
                let (accept, text) = answers.remove(0);
                seen.push(serde_json::json!({"type": format!("{:?}", d.r#type), "message": d.message, "wall_ms": wall.elapsed().as_millis() as u64,
                    "eval_while_open": format!("{probe:?}").chars().take(80).collect::<String>()}));
                c.call(s, &page::HandleJavaScriptDialogParams { prompt_text: text, ..page::HandleJavaScriptDialogParams::new(accept) }).await?;
            }
            e = expired.next() => { e?; break; }
            _ = tokio::time::sleep(Duration::from_secs(10)) => { seen.push(serde_json::json!("budget never expired")); break; }
        }
    }
    let log = eval(c, &p, "JSON.stringify(log)").await?;
    let t0 = Instant::now();
    let tree = c.call(s, &accessibility::GetFullAxTreeParams::default()).await?;
    let ax_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let interesting: Vec<String> = tree.nodes.iter().filter(|n| !n.ignored).filter_map(|n| {
        let role = n.role.as_ref().and_then(|r| r.value.clone()).map(|v| v.to_string()).unwrap_or_default();
        let name = n.name.as_ref().and_then(|r| r.value.clone()).map(|v| v.to_string()).unwrap_or_default();
        (!name.is_empty()).then(|| format!("{role} {name} backend={:?}", n.backend_dom_node_id.as_ref().map(|b| b.0)))
    }).collect();
    report("s12-dialogs-and-ax", serde_json::json!({
        "dialogs": seen, "page_log": log, "ax_wall_ms": ax_ms, "ax_named_nodes": interesting, "ax_total": tree.nodes.len(),
    }));
    Ok(())
}
