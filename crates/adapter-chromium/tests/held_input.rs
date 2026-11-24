//! P03: native pointer input can be inspected between its constituent events without time progress.
mod support;

use frontcap_chromium::Generation;
use frontcap_chromium::input::InputStep;
use frontcap_core::action::{Action, Offset};
use frontcap_core::ids::RequestId;
use frontcap_core::observation::Prop;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

#[tokio::test]
async fn held_buttons_preserve_native_masks_capture_and_context_menu() {
    use frontcap_core::pointer::PointerButton;
    let runs = tempfile::tempdir().unwrap();
    let addr = support::serve();
    for (button, native, mask, name) in [(PointerButton::Right, 2, 2, "right"), (PointerButton::Middle, 1, 4, "middle")] {
        let mut g = Generation::launch(support::options(runs.path(), name, format!("http://{addr}/pointer.html"), RuleSet::default())).await.unwrap();
        g.advance(VirtualDuration::from_ms(51)).await.unwrap();
        let boundary = g.observation_boundary().await.unwrap();
        g.act_input(RequestId::new("begin").unwrap(), &Action::PointerBegin { target: Locator::Css { selector: "#pad".into() }, point: [40, 40], button, pointer: Default::default(), boundary }).await.unwrap();
        let held = g.held_pointer().unwrap();
        assert_eq!(held.button, button);
        let boundary = g.observation_boundary().await.unwrap();
        // Outside the target, still delivered to its native pointer capture.
        g.act_input(RequestId::new("move").unwrap(), &Action::PointerMove { handle: held.handle.clone(), point: [300, 40], boundary }).await.unwrap();
        g.act_input(RequestId::new("end").unwrap(), &Action::PointerEnd { handle: held.handle }).await.unwrap();
        let events = g.eval_main("buttons").await.unwrap();
        let events = events.as_array().unwrap();
        for (kind, expected_mask) in [("pointerdown", mask), ("pointerup", 0)] {
            let found: Vec<_> = events.iter().filter(|e| e[0] == kind).collect();
            assert_eq!(found.len(), 1, "{name}: {events:?}");
            assert_eq!(found[0][1], native);
            assert_eq!(found[0][2], expected_mask);
        }
        assert!(events.iter().any(|e| e[0] == "pointermove" && e[2] == mask));
        assert!(events.iter().any(|e| e[0] == "lostpointercapture"));
        assert!(!events.iter().any(|e| e[0] == "click"));
        if button == PointerButton::Right { assert!(events.iter().any(|e| e[0] == "contextmenu")); }
        for event in events { assert_eq!(event[3], true); assert_eq!(event[4], 51); }
        assert!(g.held_pointer().is_none());
        assert_eq!(g.now().0, 51_000);
        g.stop().await.unwrap();
    }
}

#[tokio::test]
async fn held_pointer_moves_arrive_while_frozen_and_allow_intermediate_inspection() {
    assert!(browser_pin::engine::patched_binary(&support::repo_root(), browser_pin::current_platform().unwrap()).unwrap().is_some());
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "held-pointer", format!("http://{}/drag.html", support::serve()), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let target = Locator::Css { selector: "#handle".into() };
    g.eval_main("setTimeout(() => document.querySelector('#handle').setAttribute('data-timeout', 'fired'), 5); 'armed'").await.unwrap();
    let action = Action::Drag { target: target.clone(), to: None, offset: Some(Offset { dx: 120, dy: 0 }), steps: Some(3), interval_us: 0 };
    let plan = g.input_plan(&action).await.unwrap();
    let id = RequestId::new("hold-and-inspect").unwrap();
    let at = g.now();
    let mut moved = 0;
    for step in 0..plan.steps.len() {
        g.input_substep(&id, &plan, step).await.unwrap();
        if matches!(plan.steps[step], InputStep::Mouse { kind: frontcap_chromium::input::MouseKind::Drag, .. }) {
            moved += 40;
            for delay in [0, 30] {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                let read = g.inspect(&target, &[Prop::Style("left".into()), Prop::Attr("data-timeout".into())]).await.unwrap();
                assert_eq!(read.values["style:left"], format!("{}px", 20 + moved));
                assert!(read.values["attr:data-timeout"].is_null(), "held input must not run the pending timer");
                assert_eq!(g.now(), at, "a read or held move must not grant progress");
            }
        }
    }
    g.input_finish(&id, &plan).await.unwrap();
    assert_eq!(g.now(), at);
    let log = g.eval_main("window.log").await.unwrap();
    let events = log.as_array().unwrap();
    assert_eq!(events.iter().filter(|e| e[0] == "pointerdown").count(), 1);
    assert_eq!(events.iter().filter(|e| e[0] == "pointerup").count(), 1);
    assert_eq!(events.iter().filter(|e| e[0] == "pointermove" && e[3] == 1).count(), 3);
    for event in events { assert_eq!(event.as_array().unwrap().last().unwrap(), 51); }
    g.advance(VirtualDuration::from_ms(5)).await.unwrap();
    assert_eq!(g.query(&target, &[Prop::Attr("data-timeout".into())]).await.unwrap().values["attr:data-timeout"], "fired");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn paced_drag_runs_only_the_requested_intervals_and_exposes_timer_effects() {
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "paced-pointer", format!("http://{}/drag.html", support::serve()), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let target = Locator::Css { selector: "#handle".into() };
    g.eval_main("setTimeout(() => document.querySelector('#handle').setAttribute('data-timeout', 'fired'), 5); 'armed'").await.unwrap();
    let action = Action::Drag { target: target.clone(), to: None, offset: Some(Offset { dx: 120, dy: 0 }), steps: Some(3), interval_us: 10_000 };
    g.act_input(RequestId::new("paced-drag").unwrap(), &action).await.unwrap();
    assert_eq!(g.now().0, 81_000, "three distinct moves grant exactly three 10 ms intervals");
    let read = g.query(&target, &[Prop::Style("left".into()), Prop::Attr("data-timeout".into())]).await.unwrap();
    assert_eq!(read.values["style:left"], "140px");
    assert_eq!(read.values["attr:data-timeout"], "fired");
    let log = g.eval_main("window.log").await.unwrap();
    let moves: Vec<_> = log.as_array().unwrap().iter().filter(|e| e[0] == "pointermove" && e[3] == 1).map(|e| e[4].clone()).collect();
    assert_eq!(moves, [serde_json::json!(61), serde_json::json!(71), serde_json::json!(81)]);
    let boundary = g.observation_boundary().await.unwrap();
    for interval_us in [1, 60_001_000] {
        let invalid = Action::Drag { target: target.clone(), to: None, offset: Some(Offset { dx: 1, dy: 0 }), steps: None, interval_us };
        assert!(matches!(g.input_plan(&invalid).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
        assert_eq!(g.observation_boundary().await.unwrap(), boundary);
    }
    g.stop().await.unwrap();
}

#[tokio::test]
async fn pointer_sessions_keep_their_owner_and_position_across_stopped_calls() {
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "pointer-session", format!("http://{}/drag.html", support::serve()), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let target = Locator::Css { selector: "#handle".into() };
    let before = g.observation_boundary().await.unwrap();
    g.act_input(RequestId::new("begin").unwrap(), &Action::PointerBegin { button: Default::default(), target: target.clone(), point: [40, 40], pointer: Default::default(), boundary: before.clone() }).await.unwrap();
    let down = g.pointer_state().await.unwrap();
    let held = down.held.unwrap();
    assert_eq!(held.point, [40, 40]);
    assert_eq!(held.handle.request_id.as_str(), "begin");
    assert_eq!(down.boundary.t_us, before.t_us);
    assert!(matches!(g.input_plan(&Action::Click { target: target.clone() }).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
    assert!(matches!(g.input_plan(&Action::PointerMove { handle: held.handle.clone(), point: [80, 40], boundary: before }).await, Err(frontcap_chromium::ChromiumError::ObservationChanged(_))));
    let mut foreign = held.handle.clone();
    foreign.launch_id = frontcap_core::ids::LaunchId::generate();
    assert!(matches!(g.input_plan(&Action::PointerEnd { handle: foreign }).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
    for (n, x) in [80, 120, 160].into_iter().enumerate() {
        let boundary = g.observation_boundary().await.unwrap();
        g.act_input(RequestId::new(format!("move-{n}")).unwrap(), &Action::PointerMove { handle: held.handle.clone(), point: [x, 40], boundary }).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let read = g.query(&target, &[Prop::Style("left".into())]).await.unwrap();
        assert_eq!(read.values["style:left"], format!("{}px", x - 20));
        assert_eq!(g.pointer_state().await.unwrap().held.unwrap().point, [x, 40]);
        assert_eq!(g.now().0, 51_000);
    }
    g.act_input(RequestId::new("end").unwrap(), &Action::PointerEnd { handle: held.handle.clone() }).await.unwrap();
    assert!(g.pointer_state().await.unwrap().held.is_none());
    assert!(matches!(g.input_plan(&Action::PointerEnd { handle: held.handle }).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
    let log = g.eval_main("window.log").await.unwrap();
    let events = log.as_array().unwrap();
    assert_eq!(events.iter().filter(|e| e[0] == "pointerdown").count(), 1);
    assert_eq!(events.iter().filter(|e| e[0] == "pointerup").count(), 1);
    assert_eq!(events.iter().filter(|e| e[0] == "pointermove" && e[3] == 1).count(), 3);
    assert_eq!(g.now().0, 51_000);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn pointer_session_admission_retains_child_identity_and_accepts_html_drag_sources() {
    let runs = tempfile::tempdir().unwrap();
    let addr = support::serve();
    for (name, page, point) in [("pointer-frame", "locate.html", [250, 70]), ("pointer-html-drag", "drag.html", [50, 140])] {
        let mut g = Generation::launch(support::options(runs.path(), name, format!("http://{addr}/{page}"), RuleSet::default())).await.unwrap();
        g.advance(VirtualDuration::from_ms(51)).await.unwrap();
        let boundary = g.observation_boundary().await.unwrap();
        let located = g.locate(&boundary, [point[0].into(), point[1].into()]).await.unwrap();
        let begin = Action::PointerBegin { button: Default::default(), target: Locator::Ref { id: located.target.r#ref }, point, pointer: Default::default(), boundary: boundary.clone() };
        let plan = g.input_plan(&begin).await;
        if name == "pointer-frame" {
            assert!(plan.unwrap().pointer_context.is_some(), "a child press must retain its native lifetime identity");
        } else {
            assert!(plan.is_ok(), "a draggable source can begin an owned pointer session");
        }
        assert_eq!(g.observation_boundary().await.unwrap(), boundary);
        assert!(g.held_pointer().is_none());
        g.stop().await.unwrap();
    }
}
