//! E06: a finger on a touch screen can be held, inspected, moved and lifted while virtual time is stopped.
mod support;

use frontcap_chromium::Generation;
use frontcap_core::action::Action;
use frontcap_core::ids::RequestId;
use frontcap_core::pointer::PointerKind;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

#[tokio::test]
async fn a_held_touch_moves_and_lifts_while_frozen() {
    let runs = tempfile::tempdir().unwrap();
    let mut o = support::options(runs.path(), "touch", format!("http://{}/touch.html", support::serve()), RuleSet::default());
    o.profile.touch = true;
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    eprintln!("support {}", g.eval_main("touchSupport").await.unwrap());
    let pad = Locator::Css { selector: "#pad".into() };
    let boundary = g.observation_boundary().await.unwrap();
    g.act_input(RequestId::new("begin").unwrap(), &Action::PointerBegin { target: pad.clone(), point: [40, 40], button: Default::default(), pointer: PointerKind::Touch, boundary }).await.unwrap();
    let held = g.held_pointer().unwrap();
    assert_eq!(held.pointer, PointerKind::Touch);
    eprintln!("after begin {}", g.eval_main("seen").await.unwrap());
    // Wall time passes while the finger is down: nothing the browser times by the wall clock may happen.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    eprintln!("after wall wait {}", g.eval_main("seen").await.unwrap());
    let boundary = g.observation_boundary().await.unwrap();
    g.act_input(RequestId::new("move").unwrap(), &Action::PointerMove { handle: held.handle.clone(), point: [120, 40], boundary }).await.unwrap();
    g.act_input(RequestId::new("end").unwrap(), &Action::PointerEnd { handle: held.handle }).await.unwrap();
    let seen = g.eval_main("seen").await.unwrap();
    eprintln!("after end {seen}");
    let types: Vec<&str> = seen.as_array().unwrap().iter().map(|e| e[0].as_str().unwrap()).collect();
    assert!(!types.contains(&"contextmenu"), "{seen}");
    for t in ["touchstart", "touchmove", "touchend", "pointerdown", "pointerup"] { assert!(types.contains(&t), "{t}: {seen}"); }
    assert!(g.held_pointer().is_none());
    assert_eq!(g.now().0, 51_000);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_tap_clicks_at_the_instant_it_lifts() {
    let runs = tempfile::tempdir().unwrap();
    let mut o = support::options(runs.path(), "tap", format!("http://{}/touch.html", support::serve()), RuleSet::default());
    o.profile.touch = true;
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let pad = Locator::Css { selector: "#pad".into() };
    let boundary = g.observation_boundary().await.unwrap();
    g.act_input(RequestId::new("begin").unwrap(), &Action::PointerBegin { target: pad.clone(), point: [40, 40], button: Default::default(), pointer: PointerKind::Touch, boundary }).await.unwrap();
    let held = g.held_pointer().unwrap();
    g.act_input(RequestId::new("end").unwrap(), &Action::PointerEnd { handle: held.handle }).await.unwrap();
    let seen = g.eval_main("seen").await.unwrap();
    eprintln!("after tap {seen}");
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let later = g.eval_main("seen").await.unwrap();
    eprintln!("after wall wait {later}");
    let types: Vec<&str> = seen.as_array().unwrap().iter().map(|e| e[0].as_str().unwrap()).collect();
    assert_eq!(types, ["pointerdown", "touchstart", "pointerup", "touchend", "mousedown", "mouseup", "click"], "{seen}");
    assert_eq!(seen, later, "nothing arrives later in wall time");
    assert_eq!(g.now().0, 51_000);
    g.stop().await.unwrap();
}

/// The touch screen and a finger on it outlive the session that began the touch: the next session moves
/// and lifts it.
#[tokio::test]
async fn a_held_touch_survives_reconnection() {
    let runs = tempfile::tempdir().unwrap();
    let profile_dir = support::DetachedProfile::new();
    let mut o = support::options(runs.path(), "touch-rc", format!("http://{}/touch.html", support::serve()), RuleSet::default());
    profile_dir.apply(&mut o);
    o.profile.touch = true;
    let profile = o.profile.clone();
    let mut g = Generation::launch(o).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let pad = Locator::Css { selector: "#pad".into() };
    let boundary = g.observation_boundary().await.unwrap();
    g.act_input(RequestId::new("begin").unwrap(), &Action::PointerBegin { target: pad.clone(), point: [40, 40], button: Default::default(), pointer: PointerKind::Touch, boundary }).await.unwrap();
    let (endpoint, launch) = (g.endpoint(), g.launch_id().clone());
    g.abandon();
    let options = frontcap_chromium::generation::ReconnectOptions {
        repo_root: support::repo_root(),
        runs_root: runs.path().to_path_buf(),
        run: frontcap_core::ids::RunId::new("touch-rc").unwrap(),
        launch_id: launch,
        profile,
        endpoint,
        watches: Default::default(),
        watchdog: std::time::Duration::from_secs(10),
        granted_until: None,
        dialog_rules: Default::default(),
        upstream: None,
    };
    let (mut g, _) = Generation::reconnect(options).await.unwrap();
    assert_eq!(g.eval_main("[navigator.maxTouchPoints, 'ontouchstart' in window]").await.unwrap(), serde_json::json!([5, true]));
    let held = g.held_pointer().expect("the touch is still held");
    assert_eq!(held.pointer, PointerKind::Touch);
    let boundary = g.observation_boundary().await.unwrap();
    g.act_input(RequestId::new("move").unwrap(), &Action::PointerMove { handle: held.handle.clone(), point: [120, 40], boundary }).await.unwrap();
    g.act_input(RequestId::new("end").unwrap(), &Action::PointerEnd { handle: held.handle }).await.unwrap();
    let seen = g.eval_main("seen").await.unwrap();
    let types: Vec<&str> = seen.as_array().unwrap().iter().map(|e| e[0].as_str().unwrap()).collect();
    assert_eq!(types, ["pointerdown", "touchstart", "pointermove", "touchmove", "pointerup", "touchend"], "{seen}");
    g.stop().await.unwrap();
}
