//! What survives when the CDP connection that set up a page goes away (a recorder dying): virtual-time
//! policy, emulation overrides, bindings, new-document scripts, and isolated-world state.
use cdp::protocol::{emulation::{self, VirtualTimePolicy}, network, page, runtime, target};
use spike_common::*;
use std::time::Duration;

const ORIGIN_MS: f64 = 946_684_800_000.0;

async fn attach(c: &cdp::Client, target_id: &str) -> anyhow::Result<Page> {
    let a = c.call(None, &target::AttachToTargetParams { flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(target_id.into())) }).await?;
    Ok(Page { session: a.session_id.0, target_id: target_id.into() })
}

async fn eval_in(c: &cdp::Client, p: &Page, ctx: Option<runtime::ExecutionContextId>, expr: &str) -> serde_json::Value {
    let r = tokio::time::timeout(Duration::from_secs(3), c.call(Some(&p.session), &runtime::EvaluateParams { context_id: ctx, return_by_value: Some(true), ..runtime::EvaluateParams::new(expr.into()) })).await;
    match r {
        Ok(Ok(r)) => r.result.value.unwrap_or(serde_json::Value::Null),
        Ok(Err(e)) => serde_json::json!(format!("error: {e}")),
        Err(_) => serde_json::json!("timeout"),
    }
}

async fn world_of(c: &cdp::Client, p: &Page, name: &str) -> anyhow::Result<Option<runtime::ExecutionContextId>> {
    let mut created = c.subscribe::<runtime::ExecutionContextCreatedEvent>(Some(&p.session));
    c.call(Some(&p.session), &runtime::EnableParams::default()).await?;
    let mut found = None;
    while let Some(ev) = created.next_timeout(Duration::from_millis(300)).await? {
        if ev.context.name == name {
            found = Some(ev.context.id);
        }
    }
    Ok(found)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
    let addr = serve_dir(&fixtures)?;
    let browser = Browser::start(&[]).await?;
    let created = browser.client.call(None, &target::CreateTargetParams::new("about:blank".into())).await?;
    let target_id = created.target_id.0.clone();

    // The "recorder": its own connection, dropped below.
    let a = cdp::Client::connect(&browser.launched.browser_ws_url).await?;
    let pa = attach(&a, &target_id).await?;
    let s = Some(pa.session.as_str());
    a.call(s, &page::EnableParams::default()).await?;
    a.call(s, &runtime::EnableParams::default()).await?;
    a.call(s, &emulation::SetTimezoneOverrideParams::new("Pacific/Auckland".into())).await?;
    a.call(s, &emulation::SetDeviceMetricsOverrideParams::new(777, 555, 1.0, false)).await?;
    a.call(s, &runtime::AddBindingParams { execution_context_name: Some("w".into()), ..runtime::AddBindingParams::new("__rep".into()) }).await?;
    a.call(s, &page::AddScriptToEvaluateOnNewDocumentParams { world_name: Some("w".into()), ..page::AddScriptToEvaluateOnNewDocumentParams::new("globalThis.mark = 'doc-script'; globalThis.x = 42;".into()) }).await?;
    a.call(s, &emulation::SetVirtualTimePolicyParams { initial_virtual_time: Some(network::TimeSinceEpoch(ORIGIN_MS / 1000.0)), ..emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause) }).await?;
    navigate(&a, &pa, &format!("http://{addr}/index.html")).await?;
    advance(&a, &pa, 50.0).await?;
    let before = eval(&a, &pa, "JSON.stringify([Date.now() - 946684800000, window.count, document.title, Intl.DateTimeFormat().resolvedOptions().timeZone, innerWidth])").await?;
    navigate(&a, &pa, &format!("http://{addr}/index.html?control")).await?;
    for _ in 0..5 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        advance(&a, &pa, 50.0).await?;
    }
    let control = eval(&a, &pa, "JSON.stringify([Date.now() - 946684800000, window.count, document.readyState, location.search])").await?;
    drop(pa);
    drop(a);
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let b = cdp::Client::connect(&browser.launched.browser_ws_url).await?;
    let pb = attach(&b, &target_id).await?;
    let world = world_of(&b, &pb, "w").await?;
    let after = eval_in(&b, &pb, None, "JSON.stringify([Date.now() - 946684800000, window.count, document.title, Intl.DateTimeFormat().resolvedOptions().timeZone, innerWidth])").await;
    let world_state = match world.clone() {
        Some(w) => eval_in(&b, &pb, Some(w), "JSON.stringify([globalThis.mark, globalThis.x, typeof globalThis.__rep])").await,
        None => serde_json::json!("no world"),
    };
    let mut bound = b.subscribe::<runtime::BindingCalledEvent>(Some(&pb.session));
    if let Some(w) = world.clone() {
        eval_in(&b, &pb, Some(w), "typeof __rep === 'function' && __rep('hello')").await;
    }
    let binding_delivered = bound.next_timeout(Duration::from_millis(500)).await?.map(|e| e.payload);
    b.call(Some(&pb.session), &runtime::AddBindingParams { execution_context_name: Some("w".into()), ..runtime::AddBindingParams::new("__rep".into()) }).await?;
    if let Some(w) = world.clone() {
        eval_in(&b, &pb, Some(w), "__rep('again')").await;
    }
    let binding_after_readd = bound.next_timeout(Duration::from_millis(500)).await?.map(|e| e.payload);
    // A new document: does the old connection's new-document script still run?
    b.call(Some(&pb.session), &page::EnableParams::default()).await?;
    b.call(Some(&pb.session), &network::EnableParams::default()).await?;
    let mut sent = b.subscribe::<network::RequestWillBeSentEvent>(Some(&pb.session));
    let mut resp = b.subscribe::<network::ResponseReceivedEvent>(Some(&pb.session));
    let mut fin = b.subscribe::<network::LoadingFinishedEvent>(Some(&pb.session));
    let mut failed = b.subscribe::<network::LoadingFailedEvent>(Some(&pb.session));
    // S11: a new session must own virtual time before anything loads.
    if std::env::var("S11_NO_REOWN").is_err() {
        b.call(Some(&pb.session), &emulation::SetVirtualTimePolicyParams::new(VirtualTimePolicy::Pause)).await?;
    }
    navigate(&b, &pb, &format!("http://{addr}/index.html?after")).await?;
    for _ in 0..5 {
        // The document response arrives on the wall clock; queued network work runs when time advances.
        tokio::time::sleep(Duration::from_millis(100)).await;
        advance(&b, &pb, 50.0).await?;
    }
    let mut net = Vec::new();
    while let Some(e) = sent.next_timeout(Duration::from_millis(50)).await? { net.push(format!("sent {}", e.request.url)); }
    while let Some(e) = resp.next_timeout(Duration::from_millis(50)).await? { net.push(format!("resp {} {}", e.response.url, e.response.status)); }
    while let Some(e) = fin.next_timeout(Duration::from_millis(50)).await? { net.push(format!("finished {}", e.request_id.0)); }
    while let Some(e) = failed.next_timeout(Duration::from_millis(50)).await? { net.push(format!("failed {}", e.error_text)); }
    let world2 = world_of(&b, &pb, "w").await?;
    let after_navigation = eval_in(&b, &pb, None, "JSON.stringify([Date.now() - 946684800000, Intl.DateTimeFormat().resolvedOptions().timeZone, innerWidth, location.href, window.count, document.readyState, document.title])").await;
    let world2_state = match world2.clone() { Some(w) => eval_in(&b, &pb, Some(w), "JSON.stringify([globalThis.mark, typeof globalThis.__rep])").await, None => serde_json::json!("no world") };
    report("s11-session-loss", serde_json::json!({
        "before_drop": before,
        "control_navigation_same_session": control,
        "after_1s_wall": after,
        "world_found": world.is_some(),
        "world_state": world_state,
        "binding_delivered_to_new_session": binding_delivered,
        "world_after_navigation": world2.is_some(),
        "after_navigation": after_navigation,
        "network_after_navigation": net,
        "binding_after_readd": binding_after_readd,
        "world_after_navigation_state": world2_state,
    }));
    Ok(())
}
