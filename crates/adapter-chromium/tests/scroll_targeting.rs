//! Viewport geometry, native hit tests and dispatched input agree after scrolling.
mod support;
use frontcap_chromium::{ChromiumError, Generation};
use frontcap_core::{action::Action, ids::RequestId, observation::Prop, predicate::Locator, rules::RuleSet, time::VirtualDuration};

fn css(selector: &str) -> Locator { Locator::Css { selector: selector.into() } }
fn id(s: &str) -> RequestId { RequestId::new(s).unwrap() }

#[tokio::test]
async fn scrolled_targets_agree_across_inspection_images_clicks_and_held_input() {
    let addr = support::serve();
    for (suffix, scale) in [("", 1000), ("#zoom", 2000)] {
        let runs = tempfile::tempdir().unwrap();
        let mut options = support::options(runs.path(), "scroll-target", format!("http://{addr}/scroll-target.html{suffix}"), RuleSet::default());
        options.profile.viewport.device_scale_milli = scale;
        let mut g = Generation::launch(options).await.unwrap();
        g.advance(VirtualDuration::from_ms(51)).await.unwrap();
        let target = css("#toggle");
        assert_eq!(g.inspect(&target, &[]).await.unwrap().click.reason.as_deref(), Some("offscreen"));
        g.act_input(id("scroll"), &Action::Scroll { target: None, delta_x: 600, delta_y: 1000 }).await.unwrap();
        g.advance(VirtualDuration::from_ms(34)).await.unwrap();
        let frozen = g.now();
        let inspected = g.inspect(&target, &[Prop::Bbox]).await.unwrap();
        assert!(inspected.click.ready, "visible checkbox rejected after scroll: {inspected:?}");
        let point = inspected.click.point.unwrap();
        let hit = g.locate(&inspected.boundary, point).await.unwrap();
        assert_eq!(hit.target.r#ref, inspected.r#ref);
        assert_eq!(g.now(), frozen, "inspection and hit tests must not advance time");
        g.click(id("click"), &target).await.unwrap();
        assert_eq!(g.eval_main("document.querySelector('#toggle').checked").await.unwrap(), true);
        assert_eq!(g.eval_main("document.querySelector('#toggle').dataset.trusted").await.unwrap(), "true");
        assert_eq!(g.now(), frozen);

        g.advance(VirtualDuration::from_ms(17)).await.unwrap();
        let scene = g.scene(None, None, 100, true, true).await.unwrap();
        let image = scene.presentation.image.unwrap();
        let ratio = f64::from(image.pixel_size[0]) / f64::from(image.viewport_size[0]);
        let hit = g.locate_image(&image.id, [point[0] * ratio, point[1] * ratio]).await.unwrap();
        assert_eq!(hit.target.r#ref, inspected.r#ref);
        g.act_input(id("begin"), &Action::PointerBegin { target: target.clone(), point: hit.point, button: Default::default(), pointer: Default::default(), boundary: hit.target.boundary }).await.unwrap();
        let held = g.pointer_state().await.unwrap().held.unwrap();
        g.act_input(id("end"), &Action::PointerEnd { handle: held.handle }).await.unwrap();
        assert_eq!(g.eval_main("document.querySelector('#toggle').checked").await.unwrap(), false);
        assert_eq!(g.eval_main("document.querySelector('#toggle').dataset.clicks").await.unwrap(), "2");

        g.eval_main("document.querySelector('#cover').hidden = false").await.unwrap();
        let blocked = g.inspect(&target, &[]).await.unwrap();
        assert!(blocked.click.reason.as_ref().unwrap().contains("covered_by"));
        assert!(matches!(g.click(id("covered"), &target).await, Err(ChromiumError::NotActionable(_))));
        g.stop().await.unwrap();
    }
}
