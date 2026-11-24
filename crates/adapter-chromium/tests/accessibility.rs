//! Accessibility reads must not invalidate a frozen presentation or depend on a controller connection.
mod support;
use cdp::protocol::{emulation, target};
use frontcap_chromium::{Generation, generation::ReconnectOptions, session::Session};
use frontcap_core::{ids::RunId, observation::PresentationRelation, predicate::WatchRegistry, rules::RuleSet, time::VirtualDuration};

#[tokio::test]
async fn first_child_accessibility_reads_preserve_the_presented_frame() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap().keep();
    eprintln!("accessibility evidence (retained on failure): {}", runs.display());
    let mut options = support::options(&runs, "ax-child", format!("http://{addr}/nested-visual.html"), RuleSet::default());
    options.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port())]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let before = g.screenshot().await.unwrap();
    assert!(before.current);
    for _ in 0..2 {
        let scene = g.scene(None, None, 100, true, true).await.unwrap();
        assert_eq!(scene.presentation.relation, PresentationRelation::Current);
        assert!(scene.snapshot.nodes.iter().any(|node| node.name == "Target"));
        let after = g.screenshot().await.unwrap();
        assert!(after.current);
        assert_eq!(after.t_us, before.t_us);
        assert_eq!(std::fs::read(after.path).unwrap(), std::fs::read(&before.path).unwrap());
    }
    assert_eq!(g.now().0, 201_000);
    g.stop().await.unwrap();
    std::fs::remove_dir_all(runs).unwrap();
}

#[tokio::test]
async fn first_child_accessibility_reads_do_not_request_a_frame_across_reconnection() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap().keep();
    eprintln!("accessibility evidence (retained on failure): {}", runs.display());
    let profile_dir = support::DetachedProfile::new();
    let mut options = support::options(&runs, "ax-connection", format!("http://{addr}/nested-visual.html"), RuleSet::default());
    options.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port())]);
    options.profile.locale = "ko-KR".into();
    options.profile.time_zone = "UTC".into();
    options.profile.viewport.device_scale_milli = 2000;
    profile_dir.apply(&mut options);
    let profile = options.profile.clone();
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    assert!(g.screenshot().await.unwrap().current);
    let endpoint = g.endpoint();
    // An independent read-only session observes native frame demand, even while the recorder's
    // session is gone. It never enables accessibility or holds an inspector AX context itself.
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams { flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id.clone())) }).await.unwrap();
    let native = Session::new(client, attached.session_id.0);
    let environment = "[innerWidth,innerHeight,devicePixelRatio,Intl.DateTimeFormat().resolvedOptions().locale,Intl.DateTimeFormat().resolvedOptions().timeZone,performance.now()]";
    let before = native.eval(environment, None).await.unwrap();
    assert_eq!(before, serde_json::json!([1280, 720, 2, "ko-KR", "UTC", 201]));
    g.eval_main("globalThis.controllerResizeEvents=[];addEventListener('resize',()=>controllerResizeEvents.push([innerWidth,innerHeight,performance.now()]));").await.unwrap();
    assert!(!native.call(&emulation::IsMainFrameWantedParams::default()).await.unwrap().wanted);
    for _ in 0..2 {
        let scene = g.scene(None, None, 100, true, true).await.unwrap();
        assert_eq!(scene.presentation.relation, PresentationRelation::Current);
        assert!(scene.snapshot.nodes.iter().any(|node| node.name == "Target"));
    }
    let observations = g.observation_state();
    let launch_id = g.launch_id().clone();
    g.abandon();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(native.eval(environment, None).await.unwrap(), before, "the controlled environment must outlive its controller");
    assert!(!native.call(&emulation::IsMainFrameWantedParams::default()).await.unwrap().wanted, "disconnecting the controller must not invalidate the frozen presentation");
    let (mut g, recovered) = Generation::reconnect(ReconnectOptions {
        repo_root: support::repo_root(), runs_root: runs.clone(), run: RunId::new("ax-connection").unwrap(),
        launch_id, profile, endpoint, watches: WatchRegistry::default(), watchdog: std::time::Duration::from_secs(10),
        granted_until: None, dialog_rules: Default::default(), upstream: None,
    }).await.unwrap();
    assert!(recovered.affected.is_empty());
    g.restore_observations(observations);
    assert_eq!(native.eval(environment, None).await.unwrap(), before);
    let snapshot = g.snapshot(None, None, 100).await.unwrap();
    assert!(snapshot.nodes.iter().any(|node| node.name == "Target"));
    assert!(!native.call(&emulation::IsMainFrameWantedParams::default()).await.unwrap().wanted, "the first reconnected read must not invalidate presentation");
    assert_eq!(g.now().0, 201_000);
    native.client.close();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(g.eval_main(environment).await.unwrap(), before, "disconnecting a read-only inspector must not reset the environment");
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    assert_eq!(g.eval_main("controllerResizeEvents").await.unwrap(), serde_json::json!([]), "reconnection must not leave a resize event for the next progress step");
    g.stop().await.unwrap();
    std::fs::remove_dir_all(runs).unwrap();
}
