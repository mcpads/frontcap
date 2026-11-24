//! Native clock participant contract. This does not qualify coordinated generation progress.
mod support;
use cdp::protocol::{emulation, runtime, target};
use frontcap_chromium::generation::Generation;
use frontcap_core::{ids::RequestId, predicate::Locator, profile::PopupPolicy, rules::RuleSet, time::VirtualDuration};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn rejected_clock_commands_freeze_and_participants_do_not_replace_storage_time() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html"},"body":"<!doctype html><button id=open onclick=\"window.open('https://auth.example.test/')\">open</button>"}},
        {"id":"popup","match":{"url":"https://auth.example.test/"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},
            "body":"<!doctype html><script>globalThis.ticks=0;setInterval(()=>ticks++,10)</script>popup"}}
    ]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "clock-participant", "https://app.example.test/".into(), rules);
    options.profile.popups = PopupPolicy::Controlled;
    options.passthrough_origins = Some(vec![]);
    let epoch = options.profile.initial_date_ms as i128 * 1000;
    let mut generation = Generation::launch(options).await.unwrap();
    generation.advance(VirtualDuration::from_ms(50)).await.unwrap();
    generation.evaluate("document.cookie='owner_clock=live; Max-Age=1; Path=/'").await.unwrap();
    // Independent-renderer generation admission remains open. Exercise the primitive directly
    // after opening the popup; a successful click alone does not qualify its clock.
    let opened = generation.click(RequestId::new("open").unwrap(), &Locator::Css { selector:"#open".into() }).await;
    eprintln!("generation popup admission: {opened:?}");
    let endpoint = generation.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let targets = client.call(None, &target::GetTargetsParams::default()).await.unwrap();
    let popup = targets.target_infos.iter().find(|t| t.r#type == "page" && t.url.starts_with("https://auth.example.test/")).unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten:Some(true), ..target::AttachToTargetParams::new(popup.target_id.clone())
    }).await.unwrap();
    let session = Some(attached.session_id.0.as_str());
    let refused = client.call(session, &emulation::SetVirtualTimePolicyParams {
        initial_virtual_time:Some(cdp::protocol::network::TimeSinceEpoch((epoch + 50_000) as f64 / 1_000_000.0)),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)
    }).await.unwrap_err();
    assert!(refused.to_string().contains("Controlled cookie clock attachment failed"), "{refused}");
    let before = client.call(session, &emulation::FrontcapGetClockParams::default()).await.unwrap();
    assert_eq!(before.unix_time_us, Some((epoch + 50_000).to_string()));
    tokio::time::sleep(Duration::from_millis(150)).await;
    let held = client.call(session, &emulation::FrontcapGetClockParams::default()).await.unwrap();
    assert_eq!(held.unix_time_us, before.unix_time_us, "rejected pause must leave the new clock frozen");
    let refused = client.call(session, &emulation::SetVirtualTimePolicyParams {
        budget:Some(2000.0), ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Advance)
    }).await.unwrap_err();
    assert!(refused.to_string().contains("Controlled cookie clock attachment failed"));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(client.call(session, &emulation::FrontcapGetClockParams::default()).await.unwrap().unix_time_us, before.unix_time_us);

    // A participant controls its scheduler but leaves the partition's cookie/quota clock alone.
    client.call(session, &emulation::SetVirtualTimePolicyParams {
        bind_storage_clock:Some(false), budget:Some(2000.0),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Advance)
    }).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let clock = client.call(session, &emulation::FrontcapGetClockParams::default()).await.unwrap();
        let now = clock.unix_time_us.unwrap().parse::<i128>().unwrap();
        assert!(now <= epoch + 2_050_000, "participant exceeded its budget");
        if now == epoch + 2_050_000 { break; }
        assert!(tokio::time::Instant::now() < deadline, "participant did not exhaust its budget");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Omitting the option preserves the participant's role on the same session.
    client.call(session, &emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)).await.unwrap();
    let home = client.call(None, &target::AttachToTargetParams {
        flatten:Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id.clone()))
    }).await.unwrap();
    let home_session = Some(home.session_id.0.as_str());
    let owner = client.call(home_session, &emulation::FrontcapGetClockParams::default()).await.unwrap();
    assert_ne!(owner.renderer_id, held.renderer_id);
    assert_eq!(owner.unix_time_us, before.unix_time_us);
    let cookies = client.call(home_session, &runtime::EvaluateParams {
        return_by_value:Some(true), ..runtime::EvaluateParams::new("document.cookie".into())
    }).await.unwrap();
    assert_eq!(cookies.result.value, Some(json!("owner_clock=live")), "a participant's later time must not expire the owner's cookie");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(client.call(session, &emulation::FrontcapGetClockParams::default()).await.unwrap().unix_time_us,
        Some((epoch + 2_050_000).to_string()));
    for id in [attached.session_id, home.session_id] {
        client.call(None, &target::DetachFromTargetParams { session_id:Some(id), ..Default::default() }).await.unwrap();
    }
    drop(generation);
}

/// Enrollment of a timer-created popup shares a renderer with an active grant. Preparing the
/// popup must preserve that grant, including its endpoint, instead of pausing its opener.
#[tokio::test]
async fn preparing_shared_popup_preserves_the_openers_active_budget() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html"},"body":"<!doctype html><script>setTimeout(()=>window.open('https://app.example.test/popup'),50)</script>page"}},
        {"id":"popup","match":{"url":"https://app.example.test/popup"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html"},"body":"<!doctype html><script>globalThis.openedAt=Date.now();globalThis.origin=performance.timeOrigin</script>popup"}}
    ]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "clock-prepare", "https://app.example.test/".into(), rules);
    options.profile.popups = PopupPolicy::Controlled;
    options.passthrough_origins = Some(vec![]);
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    generation.advance(VirtualDuration::from_ms(100)).await.unwrap();
    assert_eq!(generation.evaluate("Date.now()").await.unwrap(), json!(epoch + 100));
    generation.enter_context("popup-1").unwrap();
    let popup = generation.evaluate("({now:Date.now(),openedAt,origin})").await.unwrap();
    assert_eq!(popup, json!({"now":epoch + 100,"openedAt":epoch + 50,"origin":epoch + 50}));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(generation.evaluate("Date.now()").await.unwrap(), json!(epoch + 100));
}
