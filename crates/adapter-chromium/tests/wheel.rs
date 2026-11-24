//! Native wheel input respects browser recipients and cancellation without granting time.
mod support;
use frontcap_chromium::{Generation, ChromiumError};
use frontcap_core::{action::Action, ids::RequestId, predicate::Locator, rules::RuleSet, time::VirtualDuration};

#[tokio::test]
async fn wheel_preserves_native_cancellation_and_nested_scroll_at_explicit_frames() {
    let runs = tempfile::tempdir().unwrap();
    let addr = support::serve();
    let mut results = Vec::new();
    for delay in [0, 150] {
        let mut g = Generation::launch(support::options(runs.path(), &format!("wheel-{delay}"), format!("http://{addr}/wheel.html"), RuleSet::default())).await.unwrap();
        g.advance(VirtualDuration::from_ms(51)).await.unwrap();
        let initial = g.observation_boundary().await.unwrap();
        let target = Locator::Css { selector: "#blocked-content".into() };
        let invalid = Action::Wheel { target: target.clone(), point: [340, 40], delta: [0, 0], boundary: initial.clone() };
        assert!(matches!(g.input_plan(&invalid).await, Err(ChromiumError::InvalidArgument(_))));
        let covered = Action::Wheel { target: target.clone(), point: [40, 40], delta: [0, 80], boundary: initial.clone() };
        assert!(matches!(g.input_plan(&covered).await, Err(ChromiumError::NotActionable(_))));
        assert_eq!(g.observation_boundary().await.unwrap(), initial);
        g.act_input(RequestId::new("blocked-wheel").unwrap(), &Action::Wheel { target, point: [340, 40], delta: [0, 80], boundary: initial.clone() }).await.unwrap();
        let stale = Action::Wheel { target: Locator::Css { selector: "#content".into() }, point: [40, 40], delta: [0, 80], boundary: initial };
        assert!(matches!(g.input_plan(&stale).await, Err(ChromiumError::ObservationChanged(_))));
        let boundary = g.observation_boundary().await.unwrap();
        g.act_input(RequestId::new("nested-wheel").unwrap(), &Action::Wheel { target: Locator::Css { selector: "#content".into() }, point: [40, 40], delta: [0, 80], boundary }).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        assert_eq!(g.now().0, 51_000);
        let frozen = g.eval_main("({events: wheels, timer: document.body.dataset.timer || null, inner: document.querySelector('#inner').scrollTop, outer: document.querySelector('#outer').scrollTop, blocked: document.querySelector('#blocked').scrollTop})").await.unwrap();
        assert!(frozen["timer"].is_null());
        assert_eq!(frozen["events"], serde_json::json!([["blocked-content", 0, 80, true, true, 51], ["content", 0, 80, true, false, 51]]));
        g.advance(VirtualDuration::from_ms(34)).await.unwrap();
        let scrolled = g.eval_main("[document.querySelector('#inner').scrollTop, document.querySelector('#outer').scrollTop, document.querySelector('#blocked').scrollTop]").await.unwrap();
        assert_eq!(scrolled, serde_json::json!([80, 0, 0]), "native default action scrolls only the inner recipient");
        // No frame publishes this new listener to the compositor before the next wheel.
        g.eval_main("document.querySelector('#inner').addEventListener('wheel', e => { e.preventDefault(); document.body.dataset.lateBlocked = 'yes'; }, {passive: false}); 'installed'").await.unwrap();
        let boundary = g.observation_boundary().await.unwrap();
        g.act_input(RequestId::new("late-listener-wheel").unwrap(), &Action::Wheel { target: Locator::Css { selector: "#content".into() }, point: [40, 40], delta: [0, 50], boundary }).await.unwrap();
        assert_eq!(g.now().0, 85_000);
        assert_eq!(g.eval_main("document.body.dataset.lateBlocked").await.unwrap(), "yes");
        g.advance(VirtualDuration::from_ms(34)).await.unwrap();
        assert_eq!(g.eval_main("document.querySelector('#inner').scrollTop").await.unwrap(), 80);
        let boundary = g.observation_boundary().await.unwrap();
        let hit = g.locate(&boundary, [620.0, 40.0]).await.unwrap();
        g.act_input(RequestId::new("child-wheel").unwrap(), &Action::Wheel { target: Locator::Ref { id: hit.target.r#ref }, point: [620, 40], delta: [0, 60], boundary }).await.unwrap();
        assert_eq!(g.now().0, 119_000);
        g.advance(VirtualDuration::from_ms(34)).await.unwrap();
        assert_eq!(g.eval_main("({events: document.querySelector('#child').contentWindow.wheels, scroll: document.querySelector('#child').contentWindow.scrollY})").await.unwrap(), serde_json::json!({"events":[[60, true]],"scroll":60}));
        results.push((frozen, scrolled));
        g.stop().await.unwrap();
    }
    assert_eq!(results[0], results[1], "host delay must not change a stopped wheel result");
}

/// `scroll` wheels where its scroller is reached first, as a user places the pointer: a carousel by its
/// axis, a list by a row out of view, the document away from the list in its middle. The page decides
/// the rest: an editor takes the wheel for its own lines, a locked page does not move, and a list under
/// a modal cannot be reached.
#[tokio::test]
async fn scroll_wheels_where_its_scroller_is_reached_first() {
    let runs = tempfile::tempdir().unwrap();
    let addr = support::serve();
    let mut g = Generation::launch(support::options(runs.path(), "scroll-wheel", format!("http://{addr}/scroll_wheel.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let css = |s: &str| Some(Locator::Css { selector: s.into() });
    let state = "[scrollY, list.scrollTop, carousel.scrollLeft, line.textContent]";
    let scroll = async |g: &mut Generation, id: &str, target: Option<Locator>, delta_x: i32, delta_y: i32| {
        let done = g.act_input(RequestId::new(id).unwrap(), &Action::Scroll { target, delta_x, delta_y }).await;
        g.advance(VirtualDuration::from_ms(34)).await.unwrap();
        done
    };
    scroll(&mut g, "carousel-x", css("#slide"), 150, 0).await.unwrap();
    assert_eq!(g.eval_main(state).await.unwrap(), serde_json::json!([0, 0, 150, "0"]));
    // Vertically the carousel has nothing to scroll: the list around it does.
    scroll(&mut g, "carousel-y", css("#slide"), 0, 200).await.unwrap();
    assert_eq!(g.eval_main(state).await.unwrap(), serde_json::json!([0, 200, 150, "0"]));
    scroll(&mut g, "offscreen-row", css("#row-30"), 0, 500).await.unwrap();
    assert_eq!(g.eval_main(state).await.unwrap(), serde_json::json!([0, 700, 150, "0"]));
    scroll(&mut g, "editor", css("#editor"), 0, 300).await.unwrap();
    assert_eq!(g.eval_main(state).await.unwrap(), serde_json::json!([0, 700, 150, "1"]));
    // The viewport's middle is over the list: the document is wheeled beside it.
    scroll(&mut g, "document", None, 0, 300).await.unwrap();
    assert_eq!(g.eval_main(state).await.unwrap(), serde_json::json!([300, 700, 150, "1"]));
    // The compositor acts on what the last frame published: the lock reaches the wheel after a frame.
    g.eval_main("document.documentElement.classList.add('locked'); 'locked'").await.unwrap();
    g.advance(VirtualDuration::from_ms(34)).await.unwrap();
    scroll(&mut g, "locked", None, 0, 300).await.unwrap();
    assert_eq!(g.eval_main(state).await.unwrap(), serde_json::json!([300, 700, 150, "1"]));
    let covered = scroll(&mut g, "covered", css("#row-20"), 0, 100).await.unwrap_err();
    assert!(matches!(&covered, ChromiumError::NotActionable(m) if m.contains("list")), "{covered:?}");
    g.stop().await.unwrap();
}
