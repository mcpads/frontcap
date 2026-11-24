mod support;

use frontcap_chromium::generation::{Generation, Prop};
use frontcap_core::action::Action;
use frontcap_core::ids::RequestId;
use frontcap_core::observation::{HitPathNodeKind as Kind, HitPathOmission, HitPathRelation as Via};
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;

#[tokio::test]
async fn hit_ancestry_crosses_slots_closed_roots_and_nested_origin_boundaries_without_running_the_page() {
    if !support::controlled_engine_or_check_baseline().await { return }
    let runs = tempfile::tempdir().unwrap();
    let addr = support::serve();
    let mut options = support::options(runs.path(), "hit-path", format!("http://{addr}/hit-path.html"), RuleSet::default());
    options.profile.viewport.device_scale_milli = 2000;
    options.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port())]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(101)).await.unwrap();
    let target = Locator::Css { selector: "#leaf-target".into() };
    let inspected = g.inspect(&target, &[Prop::Bbox]).await.unwrap();
    let point = inspected.click.point.unwrap();
    let image = g.scene(None, None, 100, true, true).await.unwrap().presentation.image.unwrap();
    let ratio = f64::from(image.pixel_size[0]) / f64::from(image.viewport_size[0]);
    let located = g.locate_image(&image.id, [point[0] * ratio, point[1] * ratio]).await.unwrap();
    assert_eq!(located.target.r#ref, inspected.r#ref);
    let path = located.hit_path.as_ref().unwrap();
    assert_eq!(path.omission, None, "{path:?}");
    // Fixture-authored ancestry, independent of the production reader. Each frame and shadow boundary
    // must remain visible; slot assignment takes precedence over the light-DOM parent host.
    let mut expected = Vec::new();
    for level in (0..=2).rev() {
        expected.push((Kind::Element, if level == 2 { "button" } else { "iframe" },
            if level == 2 { "leaf-target".into() } else { format!("frame-{}", level + 1) },
            if level == 2 { Via::Hit } else { Via::FrameOwner }));
        expected.push((Kind::Element, "slot", format!("slot-{level}"), Via::AssignedSlot));
        expected.push((Kind::Element, "div", format!("wrap-{level}"), Via::Parent));
        expected.push((Kind::ShadowRoot, "#document-fragment", String::new(), Via::Parent));
        expected.push((Kind::Element, "div", format!("host-{level}"), Via::ShadowHost));
        expected.push((Kind::Element, "body", String::new(), Via::Parent));
        expected.push((Kind::Element, "html", String::new(), Via::Parent));
        expected.push((Kind::Document, "#document", String::new(), Via::Parent));
    }
    assert_eq!(path.nodes.len(), expected.len(), "{path:?}");
    for (node, (kind, name, id, via)) in path.nodes.iter().zip(expected) {
        assert_eq!((node.kind, node.name.as_str(), node.via), (kind, name, via));
        if kind == Kind::Element {
            let value = g.query(&Locator::Ref { id: node.r#ref.clone().unwrap() }, &[Prop::Attr("id".into())]).await.unwrap();
            assert_eq!(value.values["attr:id"].as_str().unwrap_or_default(), id);
        } else {
            assert!(node.r#ref.is_none(), "boundary nodes are not element references");
        }
    }
    let repeated = g.locate(&inspected.boundary, point).await.unwrap();
    assert_eq!(repeated.hit_path, located.hit_path);
    assert_eq!(g.observation_boundary().await.unwrap(), image.boundary);
    let props = [Prop::Attr("data-clicks".into()), Prop::Attr("data-getters".into()), Prop::Attr("data-timer".into())];
    let before = g.query(&target, &props).await.unwrap();
    assert_eq!(before.values["attr:data-clicks"], "0");
    assert_eq!(before.values["attr:data-getters"], "0");
    assert_eq!(before.values["attr:data-timer"], "pending");
    let action = Action::ClickAt { target: Locator::Ref { id: located.target.r#ref }, point: located.point, boundary: located.target.boundary };
    g.act_input(RequestId::new("hit-path-click").unwrap(), &action).await.unwrap();
    let delivered = g.query(&target, &[Prop::Attr("data-path".into()), Prop::Attr("data-trusted".into())]).await.unwrap();
    let event_path: Vec<String> = serde_json::from_str(delivered.values["attr:data-path"].as_str().unwrap()).unwrap();
    assert_eq!(event_path, ["leaf-target", "slot-2", "wrap-2", "#document-fragment", "host-2", "BODY", "HTML", "#document", "window"]);
    assert_eq!(delivered.values["attr:data-trusted"], "true");
    assert!(matches!(g.act_input(RequestId::new("stale-hit-path").unwrap(), &action).await, Err(frontcap_chromium::ChromiumError::ObservationChanged(_))));
    let after = g.query(&target, &props).await.unwrap();
    assert_eq!(after.values["attr:data-clicks"], "1");
    assert_eq!(after.values["attr:data-getters"], "0");
    assert_eq!(after.values["attr:data-timer"], "pending");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_deep_hit_path_reports_omitted_ancestors_without_changing_the_boundary() {
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "deep-hit-path",
        format!("http://{}/hit-path.html?deep=260", support::serve()), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let boundary = g.observation_boundary().await.unwrap();
    let located = g.locate(&boundary, [30.0, 30.0]).await.unwrap();
    let path = located.hit_path.unwrap();
    assert_eq!(located.tag, "button");
    assert_eq!(path.omission, Some(HitPathOmission::DepthLimit { limit: 512 }));
    assert_eq!(path.nodes.len(), 512);
    assert_eq!(g.observation_boundary().await.unwrap(), boundary);
    g.stop().await.unwrap();
}
