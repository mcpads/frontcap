//! Which event timestamps follow virtual time, and whether an isolated world can be evaluated while the
//! page is paused in the debugger.
use cdp::protocol::{debugger, emulation::{self, VirtualTimePolicy}, network, page, runtime};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(&[]).await?;
    let c = &browser.client;
    let p = browser.new_page(false).await?;
    c.call(Some(&p.session), &network::EnableParams::default()).await?;
    c.call(Some(&p.session), &debugger::EnableParams::default()).await?;
    c.call(Some(&p.session), &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(c, &p, &format!("http://{addr}/index.html")).await?;
    advance(c, &p, 10.0).await?;
    let tree = c.call(Some(&p.session), &page::GetFrameTreeParams::default()).await?;
    let world = c.call(Some(&p.session), &page::CreateIsolatedWorldParams { world_name: Some("probe".into()), ..page::CreateIsolatedWorldParams::new(tree.frame_tree.frame.id) }).await?.execution_context_id;
    let mut console = c.subscribe::<runtime::ConsoleApiCalledEvent>(Some(&p.session));
    let mut sent = c.subscribe::<network::RequestWillBeSentEvent>(Some(&p.session));
    let mut paused = c.subscribe::<debugger::PausedEvent>(Some(&p.session));
    eval(c, &p, "go()").await?;
    let mut exp = c.subscribe::<emulation::VirtualTimeBudgetExpiredEvent>(Some(&p.session));
    c.call(Some(&p.session), &emulation::SetVirtualTimePolicyParams { budget: Some(500.0), max_virtual_time_task_starvation_count: Some(1000), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Advance) }).await?;
    let pause = paused.next_timeout(Duration::from_secs(5)).await?;
    let world_eval_while_paused = tokio::time::timeout(Duration::from_secs(3), c.call(Some(&p.session), &runtime::EvaluateParams { context_id: Some(world.clone()), return_by_value: Some(true), ..runtime::EvaluateParams::new("Date.now()".into()) })).await;
    let world_time = match &world_eval_while_paused { Ok(Ok(r)) => r.result.value.clone(), Ok(Err(e)) => Some(serde_json::json!(e.to_string())), Err(_) => Some(serde_json::json!("timeout")) };
    c.call(Some(&p.session), &debugger::ResumeParams::default()).await?;
    let _ = exp.next_timeout(Duration::from_secs(5)).await?;
    let cons = console.next_timeout(Duration::from_secs(1)).await?;
    let req = loop {
        match sent.next_timeout(Duration::from_secs(1)).await? { Some(r) if r.request.url.ends_with("x.json") => break Some(r), Some(_) => continue, None => break None }
    };
    report("s9-event-clocks", serde_json::json!({
        "console_args": cons.as_ref().map(|e| e.args.iter().map(|a| a.value.clone()).collect::<Vec<_>>()),
        "console_timestamp": cons.as_ref().map(|e| e.timestamp.0),
        "request_timestamp": req.as_ref().map(|r| r.timestamp.0),
        "request_wall_time": req.as_ref().map(|r| r.wall_time.0),
        "paused": pause.is_some(),
        "world_date_now_while_paused": world_time,
    }));
    Ok(())
}
