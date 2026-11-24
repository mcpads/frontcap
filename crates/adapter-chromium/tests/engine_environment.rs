//! Engine-owned environment settings survive inspectors but remain explicitly changeable.
mod support;
use cdp::protocol::{emulation, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};

#[tokio::test]
async fn explicit_environment_changes_and_clears_remain_effective_while_frozen() {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "environment", format!("http://{addr}/frames_idle.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams { flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id)) }).await.unwrap();
    let native = Session::new(client, attached.session_id.0);
    let viewport = "[innerWidth,innerHeight,devicePixelRatio]";
    // Learn the native viewport instead of prescribing the host's unconfigured size or scale.
    native.call(&emulation::ClearDeviceMetricsOverrideParams::default()).await.unwrap();
    let cleared = native.eval(viewport, None).await.unwrap();
    native.call(&emulation::SetDeviceMetricsOverrideParams::new(960, 640, 1.5, false)).await.unwrap();
    assert_eq!(native.eval(viewport, None).await.unwrap(), serde_json::json!([960, 640, 1.5]));
    native.call(&emulation::ClearDeviceMetricsOverrideParams::default()).await.unwrap();
    assert_eq!(native.eval(viewport, None).await.unwrap(), cleared);
    native.call(&emulation::SetDeviceMetricsOverrideParams::new(960, 640, 1.5, false)).await.unwrap();
    native.call(&emulation::SetLocaleOverrideParams { locale: Some("en-GB".into()) }).await.unwrap();
    native.call(&emulation::SetTimezoneOverrideParams::new("Europe/Paris".into())).await.unwrap();
    let environment = "[innerWidth,innerHeight,devicePixelRatio,Intl.DateTimeFormat().resolvedOptions().locale,Intl.DateTimeFormat().resolvedOptions().timeZone,performance.now()]";
    let expected = serde_json::json!([960, 640, 1.5, "en-GB", "Europe/Paris", 201]);
    assert_eq!(native.eval(environment, None).await.unwrap(), expected);
    native.client.close();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(g.eval_main(environment).await.unwrap(), expected, "closing the setting's inspector must not undo its explicit changes");
    assert_eq!(g.now().0, 201_000);
    g.stop().await.unwrap();
}
