//! Image points, native geometry and input agree across nested transforms and origins.
mod support;
use frontcap_chromium::{ChromiumError, Generation};
use frontcap_core::{action::Action, ids::RequestId, observation::Prop, predicate::Locator, rules::RuleSet, time::VirtualDuration};

#[tokio::test]
async fn image_targeting_crosses_zoom_nested_frames_shadow_roots_and_overlays() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap().keep();
    eprintln!("nested visual evidence (retained on failure): {}", runs.display());
    let mut options = support::options(&runs, "nested-visual", format!("http://{addr}/nested-visual.html"), RuleSet::default());
    options.profile.viewport.device_scale_milli = 2000;
    options.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port())]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let scene = g.scene(None, None, 100, true, true).await.unwrap();
    let image = scene.presentation.image.unwrap();
    assert!(image.capture.current && image.capture.exact);
    let scale = f64::from(image.pixel_size[0]) / f64::from(image.viewport_size[0]);
    // The composed frame transforms map leaf (u,v) to top (712-.5u,330-.5v).
    // The overlay covers the button's center; its right-hand visible part remains usable.
    let hit = g.locate_image(&image.id, [694.0 * scale, 300.0 * scale]).await.unwrap();
    assert_eq!(hit.tag, "button");
    // Native quads use float coordinates; tolerate arithmetic roundoff below a layout subpixel.
    for (property, expected) in [("x",652.0),("y",290.0),("width",50.0),("height",20.0)] {
        let actual = hit.target.values["bbox"][property].as_f64().unwrap();
        assert!((actual - expected).abs() < 0.001, "{property}: {actual}, expected {expected}");
    }
    assert!(!hit.target.click.ready, "the ordinary center click is covered");
    assert!(hit.target.click.reason.as_ref().unwrap().starts_with("covered_by"));
    let covered = g.locate(&image.boundary, [678.0, 300.0]).await.unwrap();
    let overlay = Locator::Ref { id: covered.target.r#ref };
    assert_eq!(g.query(&overlay, &[Prop::Attr("id".into())]).await.unwrap().values["attr:id"], "veil");
    let crop = g.crop_image(&image.id, [(649.0 * scale) as u32, (285.0 * scale) as u32, (60.0 * scale) as u32, (30.0 * scale) as u32]).unwrap();
    assert_eq!(g.locate_image(&crop.id, [45.0 * scale, 15.0 * scale]).await.unwrap().target.r#ref, hit.target.r#ref);
    assert_eq!(g.observation_boundary().await.unwrap(), image.boundary);
    let target = Locator::Ref { id: hit.target.r#ref };
    g.act_input(RequestId::new("button-region").unwrap(), &Action::ClickAt { target: target.clone(), point: hit.point, boundary: image.boundary }).await.unwrap();
    let props = [Prop::Attr("data-clicks".into()), Prop::Attr("data-x".into()), Prop::Attr("data-y".into()), Prop::Attr("data-trusted".into())];
    assert_eq!(serde_json::to_value(g.query(&target, &props).await.unwrap().values).unwrap(), serde_json::json!({"attr:data-clicks":"1","attr:data-x":"16","attr:data-y":"20","attr:data-trusted":"true"}));
    assert!(matches!(g.locate_image(&crop.id, [45.0 * scale, 15.0 * scale]).await, Err(ChromiumError::ObservationChanged(_))));
    let boundary = g.observation_boundary().await.unwrap();
    let canvas = g.locate(&boundary, [692.0, 275.0]).await.unwrap();
    assert!(canvas.canvas);
    let target = Locator::Ref { id: canvas.target.r#ref };
    g.act_input(RequestId::new("canvas-region").unwrap(), &Action::ClickAt { target: target.clone(), point: canvas.point, boundary }).await.unwrap();
    assert_eq!(serde_json::to_value(g.query(&target, &props).await.unwrap().values).unwrap(), serde_json::json!({"attr:data-clicks":"1","attr:data-x":"20","attr:data-y":"10","attr:data-trusted":"true"}));
    assert_eq!(g.now().0, 201_000);
    g.stop().await.unwrap();
    std::fs::remove_dir_all(runs).unwrap();
}
