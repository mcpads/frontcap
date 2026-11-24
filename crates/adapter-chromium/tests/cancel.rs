//! Cancellation must end native ownership without releasing/activating the target or advancing time.
mod support;
use frontcap_chromium::Generation;
use frontcap_core::{action::Action, ids::RequestId, pointer::PointerButton, predicate::Locator, rules::RuleSet, time::VirtualDuration};

#[tokio::test]
async fn native_input_timestamps_use_controlled_time_and_keep_their_history() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap().keep();
    eprintln!("native input time evidence (retained on failure): {}", runs.display());
    let mut g = Generation::launch(support::options(&runs, "input-time", format!("http://{addr}/cancel.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    for (i, delay) in [0, 25, 80].into_iter().enumerate() {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        g.click(RequestId::new(format!("click-{i}")).unwrap(), &Locator::Css { selector: "#commit".into() }).await.unwrap();
    }
    let events = g.eval_main("events.filter(e => ['pointerdown','pointerup','click'].includes(e[0])).map(e => [e[0],e[4],e[5],e[6]])").await.unwrap();
    let expected: Vec<_> = (0..3).flat_map(|_| ["pointerdown", "pointerup", "click"].map(|kind| serde_json::json!([kind, true, 51, 51]))).collect();
    assert_eq!(events, serde_json::json!(expected), "native event timestamps, not only handler performance.now(), must use controlled time");
    assert_eq!(g.now().0, 51_000);
    g.advance(VirtualDuration::from_ms(17)).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    let target = Locator::Css { selector: "#commit".into() };
    g.act_input(RequestId::new("key").unwrap(), &Action::Press { target: Some(target.clone()), key: "Escape".into() }).await.unwrap();
    let boundary = g.observation_boundary().await.unwrap();
    g.act_input(RequestId::new("wheel").unwrap(), &Action::Wheel { target, point: [40,40], delta: [0,10], boundary }).await.unwrap();
    assert_eq!(g.eval_main("events.filter(e => ['keydown','keyup','wheel'].includes(e[0])).map(e => [e[0],e[4],e[5],e[6]])").await.unwrap(), serde_json::json!([["keydown",true,68,68],["keyup",true,68,68],["wheel",true,68,68]]));
    // Read the retained native DOM objects after progress, not the earlier copied numbers.
    assert_eq!(g.eval_main("nativeEvents.filter(e => ['pointerdown','pointerup','click'].includes(e.type)).map(e => Math.round(e.timeStamp))").await.unwrap(), serde_json::json!([51,51,51,51,51,51,51,51,51]));
    assert_eq!(g.now().0, 68_000);
    g.stop().await.unwrap();
    std::fs::remove_dir_all(runs).unwrap();
}

#[tokio::test]
async fn native_cancel_releases_capture_and_click_state_for_each_button() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    for (button, name) in [(PointerButton::Left, "left"), (PointerButton::Right, "right"), (PointerButton::Middle, "middle")] {
        let mut g = Generation::launch(support::options(runs.path(), name, format!("http://{addr}/cancel.html"), RuleSet::default())).await.unwrap();
        g.advance(VirtualDuration::from_ms(51)).await.unwrap();
        let target = Locator::Css { selector: "#commit".into() };
        let boundary = g.observation_boundary().await.unwrap();
        g.act_input(RequestId::new("begin").unwrap(), &Action::PointerBegin { target: target.clone(), point: [40, 40], button, pointer: Default::default(), boundary }).await.unwrap();
        let held = g.held_pointer().unwrap();
        // Process pending capture with a real held move before checking lostpointercapture.
        let boundary = g.observation_boundary().await.unwrap();
        g.act_input(RequestId::new("move").unwrap(), &Action::PointerMove { handle: held.handle.clone(), point: [80, 40], boundary }).await.unwrap();
        g.act_input(RequestId::new("cancel").unwrap(), &Action::PointerCancel { handle: held.handle.clone() }).await.unwrap();
        assert!(g.held_pointer().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(75)).await;
        let state = g.eval_main("({commits, timer, frame, captured: document.querySelector('#commit').hasPointerCapture(pointer), samePointer: cancelledPointer === pointer, active: document.querySelector('#commit').matches(':active'), terminal: events.filter(e => ['pointerup','pointercancel','click','auxclick','lostpointercapture'].includes(e[0])).map(e => [e[0],e[1],e[3],e[4],e[5]])})").await.unwrap();
        assert_eq!(state, serde_json::json!({"commits":0,"timer":false,"frame":false,"captured":false,"samePointer":true,"active":false,"terminal":[["pointercancel","commit",0,true,51],["lostpointercapture","commit",0,true,51]]}), "{name}: {state}");
        assert_eq!(g.eval_main("events.find(e => e[0] === 'pointercancel')[6]").await.unwrap(), 51, "the cancellation timestamp is virtual");
        assert!(g.input_plan(&Action::PointerEnd { handle: held.handle }).await.is_err());
        // A native hover must leave the old target and report no held button.
        g.act_input(RequestId::new("hover").unwrap(), &Action::Hover { target: Locator::Css { selector: "#other".into() } }).await.unwrap();
        assert_eq!(g.eval_main("events.filter(e => e[0] === 'pointermove').at(-1).slice(1,5)").await.unwrap(), serde_json::json!(["other",-1,0,true]));
        g.act_input(RequestId::new("click").unwrap(), &Action::Click { target }).await.unwrap();
        assert_eq!(g.eval_main("commits").await.unwrap(), 1, "a new click works after cancellation");
        assert_eq!(g.now().0, 51_000);
        g.stop().await.unwrap();
    }
}
