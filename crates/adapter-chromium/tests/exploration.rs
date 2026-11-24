//! Mechanism evidence for frozen observations (exploration P01).
mod support;

use frontcap_chromium::Generation;
use frontcap_core::ids::RequestId;
use frontcap_core::observation::Prop;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;
use std::time::Duration;

fn css(selector: &str) -> Locator {
    Locator::Css { selector: selector.into() }
}

async fn explore(addr: std::net::SocketAddr, run: &str, extra_reads: bool) -> String {
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), run, format!("http://{addr}/explore.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    g.click(RequestId::new("change").unwrap(), &css("#change")).await.unwrap();
    let at = g.now();
    let props = [Prop::Text, Prop::Bbox, Prop::Style("opacity".into()), Prop::Attr("data-timer".into()),
        Prop::Attr("data-frame".into()), Prop::Attr("data-microtask".into())];
    let initial = g.query(&css("#panel"), &props).await.unwrap();
    assert_eq!(initial.values["text"], "Changed");
    assert_eq!(initial.values["attr:data-microtask"], "1");
    assert_eq!(initial.values["attr:data-timer"], "0");
    assert_eq!(initial.values["attr:data-frame"], "0");
    let shot = g.screenshot().await.unwrap();
    assert!(!shot.current, "logical input has changed the page before its next presentation");
    let pixels = std::fs::read(&shot.path).unwrap();
    if extra_reads {
        for delay_ms in [0, 15, 75, 5] {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let snapshot = g.snapshot(None, None, 100).await.unwrap();
            assert!(snapshot.nodes.iter().any(|n| n.name == "Changed"));
            let queried = g.query(&css("#panel"), &props).await.unwrap();
            assert_eq!(queried.values, initial.values, "host delay and repeated reads must not advance the app");
            let repeated = g.screenshot().await.unwrap();
            assert!(!repeated.current);
            assert_eq!(repeated.t_us, shot.t_us);
            assert_eq!(std::fs::read(repeated.path).unwrap(), pixels, "reading must not render pending logical state");
            assert_eq!(g.now(), at);
        }
    }
    g.advance(VirtualDuration::from_ms(200)).await.unwrap();
    let final_state = g.query(&css("#panel"), &props).await.unwrap();
    assert_eq!(final_state.values["attr:data-timer"], "1");
    assert_eq!(final_state.values["attr:data-frame"], "1");
    assert_eq!(final_state.values["style:opacity"], "0.4");
    let presented = g.screenshot().await.unwrap();
    assert!(presented.current, "after explicit progress the final state must be presented");
    assert_ne!(std::fs::read(presented.path).unwrap(), pixels);
    let path = g.trace_path();
    g.stop().await.unwrap();
    std::fs::read_to_string(path).unwrap()
}

#[tokio::test]
async fn frozen_reads_preserve_pending_presentation_and_subsequent_behavior() {
    // This establishes the patched engine's mechanism, not a baseline approximation.
    assert!(browser_pin::engine::patched_binary(&support::repo_root(), browser_pin::current_platform().unwrap()).unwrap().is_some());
    let addr = support::serve();
    let plain = explore(addr, "explore-plain", false).await;
    let observed = explore(addr, "explore-observed", true).await;
    assert_eq!(frontcap_core::projection::compare_behavior(&plain, &observed), Ok(()));
}

#[tokio::test]
async fn scene_revisions_change_with_input_but_not_observation() {
    let runs = tempfile::tempdir().unwrap();
    let addr = support::serve();
    let mut g = Generation::launch(support::options(runs.path(), "scene", format!("http://{addr}/explore.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let before = g.scene(None, None, 100, true, true).await.unwrap();
    let repeated = g.scene(None, None, 100, false, true).await.unwrap();
    assert_eq!(before.boundary, repeated.boundary);
    assert_eq!(before.presentation.relation, frontcap_core::observation::PresentationRelation::Current);
    assert!(!before.coverage_gaps.is_empty(), "a document read must not certify every execution participant");
    g.click(RequestId::new("change").unwrap(), &css("#change")).await.unwrap();
    let after = g.scene(None, None, 100, true, true).await.unwrap();
    assert_eq!(before.boundary.t_us, after.boundary.t_us);
    assert!(after.boundary.state_revision > before.boundary.state_revision);
    assert_eq!(after.presentation.relation, frontcap_core::observation::PresentationRelation::Pending);
    assert!(after.snapshot.nodes.iter().any(|n| n.name == "Changed"));
    assert_eq!(g.scene(None, None, 100, true, true).await.unwrap().boundary, after.boundary);
    assert!(matches!(g.scene(None, None, 0, false, true).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
    assert_eq!(g.observation_boundary().await.unwrap(), after.boundary);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn inspection_explains_click_rejection_without_dispatching_input() {
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "inspect-cover", format!("http://{}/cover.html", support::serve()), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let before = g.observation_boundary().await.unwrap();
    for (selector, reason) in [("#ok", None), ("#hidden-btn", Some("covered_by")), ("#dis", Some("disabled")), ("#off", Some("offscreen"))] {
        let read = g.inspect(&css(selector), &[Prop::Interaction, Prop::Bbox]).await.unwrap();
        assert_eq!(read.boundary, before);
        assert_eq!(read.border_quad.as_ref().unwrap().len(), 8);
        assert_eq!(read.click.ready, reason.is_none());
        if let Some(reason) = reason {
            assert!(read.click.reason.as_ref().unwrap().starts_with(reason), "{read:?}");
            let error = g.click(RequestId::new(format!("reject-{}", &selector[1..])).unwrap(), &css(selector)).await.unwrap_err();
            match error {
                frontcap_chromium::ChromiumError::NotActionable(actual) => assert_eq!(Some(&actual), read.click.reason.as_ref()),
                frontcap_chromium::ChromiumError::Offscreen { .. } => assert_eq!(read.click.reason.as_deref(), Some("offscreen")),
                other => panic!("{other:?}"),
            }
        }
    }
    assert_eq!(g.eval_main("JSON.stringify(window.__clicks)").await.unwrap(), "[]");
    assert_eq!(g.observation_boundary().await.unwrap(), before);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn inspection_reads_focus_selection_and_scroll_without_moving_them() {
    use frontcap_core::action::Action;
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "inspect-input", format!("http://{}/input.html", support::serve()), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    g.act_input(RequestId::new("fill").unwrap(), &Action::Fill { target: css("#name"), value: "Ada".into() }).await.unwrap();
    let before = g.observation_boundary().await.unwrap();
    for _ in 0..3 {
        let read = g.inspect(&css("#name"), &[Prop::Value, Prop::Interaction, Prop::Selection, Prop::Scroll]).await.unwrap();
        assert_eq!(read.boundary, before);
        assert_eq!(read.values["value"], "Ada");
        assert_eq!(read.values["interaction"]["focused"], true);
        assert_eq!(read.values["interaction"]["editable"], true);
        assert_eq!(read.values["selection"]["start"], 3);
        assert_eq!(read.values["selection"]["end"], 3);
        assert_eq!(read.values["scroll"]["y"], 0);
    }
    let readonly = g.inspect(&css("#readonly"), &[Prop::Interaction]).await.unwrap();
    assert_eq!(readonly.values["interaction"]["editable"], false);
    assert_eq!(g.observation_boundary().await.unwrap(), before);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn locate_uses_native_hit_testing_and_rejects_stale_boundaries() {
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "locate", format!("http://{}/cover.html", support::serve()), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let boundary = g.observation_boundary().await.unwrap();
    let covered = g.locate(&boundary, [30.9, 240.2]).await.unwrap();
    assert_eq!(covered.point, [30, 240]);
    let hit = g.query(&Locator::Ref { id: covered.target.r#ref }, &[Prop::Attr("id".into())]).await.unwrap();
    assert_eq!(hit.values["attr:id"], "veil", "hit testing must identify the overlay, not the obscured button");
    assert_eq!(g.observation_boundary().await.unwrap(), boundary);
    assert!(matches!(g.locate(&boundary, [-1.0, 1.0]).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
    g.click(RequestId::new("click").unwrap(), &css("#ok")).await.unwrap();
    assert!(matches!(g.locate(&boundary, [30.0, 30.0]).await, Err(frontcap_chromium::ChromiumError::ObservationChanged(_))));
    assert_eq!(g.eval_main("JSON.stringify(window.__clicks)").await.unwrap(), "[\"ok\"]");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn locate_resolves_transformed_frames_and_canvas_at_device_scale_two() {
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "locate-surfaces", format!("http://{}/locate.html", support::serve()), RuleSet::default());
    options.profile.viewport.device_scale_milli = 2000;
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let boundary = g.observation_boundary().await.unwrap();
    let hit = g.locate(&boundary, [250.0, 70.0]).await.unwrap();
    assert_eq!(hit.tag, "button");
    let bbox = &hit.target.values["bbox"];
    assert_eq!([bbox["x"].as_f64().unwrap(), bbox["y"].as_f64().unwrap(), bbox["width"].as_f64().unwrap(), bbox["height"].as_f64().unwrap()], [230.0, 20.0, 40.0, 100.0]);
    let canvas = g.locate(&boundary, [430.0, 50.0]).await.unwrap();
    assert!(canvas.canvas);
    assert_eq!(canvas.tag, "canvas");
    assert_eq!(canvas.target.boundary, boundary);
    let target = Locator::Ref { id: hit.target.r#ref };
    g.click(RequestId::new("native-click").unwrap(), &target).await.unwrap();
    assert_eq!(g.query(&target, &[Prop::Text]).await.unwrap().values["text"], "Clicked");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn retained_image_crops_preserve_pixels_and_mapping_but_stale_images_cannot_locate() {
    fn decode(path: &std::path::Path) -> (png::OutputInfo, Vec<u8>) {
        let mut reader = png::Decoder::new(std::io::Cursor::new(std::fs::read(path).unwrap())).read_info().unwrap();
        let mut bytes = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut bytes).unwrap();
        (info, bytes)
    }
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "image-map", format!("http://{}/locate.html", support::serve()), RuleSet::default());
    options.profile.viewport.device_scale_milli = 2000;
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let before_read = g.screenshot().await.unwrap();
    assert!(before_read.current);
    let before_pixels = std::fs::read(before_read.path).unwrap();
    let scene = g.scene(None, None, 100, true, true).await.unwrap();
    let image = scene.presentation.image.unwrap();
    assert!(image.capture.current);
    assert_eq!(std::fs::read(&image.capture.path).unwrap(), before_pixels, "first accessibility read must not render or invalidate the existing surface");
    let sx = f64::from(image.pixel_size[0]) / f64::from(image.viewport_size[0]);
    let sy = f64::from(image.pixel_size[1]) / f64::from(image.viewport_size[1]);
    let point = [250.0 * sx, 70.0 * sy];
    let hit = g.locate_image(&image.id, point).await.unwrap();
    assert_eq!(hit.tag, "button");
    let rect = [(220.0 * sx) as u32, (10.0 * sy) as u32, (70.0 * sx) as u32, (130.0 * sy) as u32];
    let crop = g.crop_image(&image.id, rect).unwrap();
    let local = [point[0] - f64::from(rect[0]), point[1] - f64::from(rect[1])];
    assert_eq!(g.locate_image(&crop.id, local).await.unwrap().target.r#ref, hit.target.r#ref);
    let nested = g.crop_image(&crop.id, [1, 2, rect[2] - 2, rect[3] - 4]).unwrap();
    assert_eq!(g.locate_image(&nested.id, [local[0] - 1.0, local[1] - 2.0]).await.unwrap().target.r#ref, hit.target.r#ref);
    let (original_info, original_pixels) = decode(&image.capture.path);
    let (cropped_info, cropped_pixels) = decode(&crop.capture.path);
    assert_eq!([cropped_info.width, cropped_info.height], [rect[2], rect[3]]);
    let channels = original_info.color_type.samples();
    for row in 0..rect[3] as usize {
        let start = (rect[1] as usize + row) * original_info.line_size + rect[0] as usize * channels;
        assert_eq!(&original_pixels[start..start + cropped_info.line_size], &cropped_pixels[row * cropped_info.line_size..(row + 1) * cropped_info.line_size]);
    }
    assert_eq!(g.observation_boundary().await.unwrap(), image.boundary);
    assert!(matches!(g.crop_image(&image.id, [u32::MAX, 0, 2, 2]), Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
    g.click(RequestId::new("change-image").unwrap(), &Locator::Ref { id: hit.target.r#ref }).await.unwrap();
    assert!(matches!(g.locate_image(&image.id, point).await, Err(frontcap_chromium::ChromiumError::ObservationChanged(_))));
    let pending = g.scene(None, None, 100, true, true).await.unwrap().presentation.image.unwrap();
    assert!(!pending.capture.current);
    assert!(matches!(g.locate_image(&pending.id, point).await, Err(frontcap_chromium::ChromiumError::ObservationChanged(_))));
    let historical = g.crop_image(&image.id, rect).unwrap();
    assert_eq!(std::fs::read(historical.capture.path).unwrap(), std::fs::read(crop.capture.path).unwrap());
    std::fs::write(&image.capture.path, b"changed").unwrap();
    assert!(matches!(g.crop_image(&image.id, rect), Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn guarded_coordinate_click_uses_the_chosen_canvas_region_and_rejects_stale_input() {
    use frontcap_core::action::Action;
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "click-at", format!("http://{}/locate.html", support::serve()), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    let scene = g.scene(None, None, 100, true, true).await.unwrap();
    let image = scene.presentation.image.unwrap();
    let hit = g.locate_image(&image.id, [430.0, 50.0]).await.unwrap();
    let target = Locator::Ref { id: hit.target.r#ref };
    let action = Action::ClickAt { target: target.clone(), point: hit.point, boundary: hit.target.boundary };
    g.act_input(RequestId::new("region").unwrap(), &action).await.unwrap();
    let props = [Prop::Attr("data-clicks".into()), Prop::Attr("data-x".into()), Prop::Attr("data-trusted".into())];
    let state = g.query(&target, &props).await.unwrap();
    assert_eq!(state.values["attr:data-clicks"], "1");
    assert_eq!(state.values["attr:data-x"], "30", "click must use the chosen point, not canvas center");
    assert_eq!(state.values["attr:data-trusted"], "true");
    assert!(matches!(g.act_input(RequestId::new("stale-region").unwrap(), &action).await, Err(frontcap_chromium::ChromiumError::ObservationChanged(_))));
    let wrong_target = Action::ClickAt { target: css("#target"), point: [430, 50], boundary: g.observation_boundary().await.unwrap() };
    assert!(matches!(g.act_input(RequestId::new("wrong-target").unwrap(), &wrong_target).await, Err(frontcap_chromium::ChromiumError::NotActionable(_))));
    assert_eq!(g.query(&target, &props).await.unwrap().values, state.values);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn direct_state_batches_preflight_and_report_partial_effects_without_user_events() {
    use frontcap_core::intervention::{StateChange, StateField as F, ExpectedValue};
    use serde_json::json;
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "state", format!("http://{}/state.html", support::serve()), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let initial = g.state_read(&css("#name"), &F::Value).await.unwrap();
    assert_eq!(initial.value, "before");
    let change = |selector, field, value| StateChange { target: css(selector), field, value, expected: None };
    let invalid = vec![change("#name", F::Value, json!("must not apply")), change("#check", F::Checked, json!("wrong type"))];
    assert!(matches!(g.state_apply(RequestId::new("invalid").unwrap(), &initial.boundary, &invalid).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
    assert_eq!(g.state_read(&css("#name"), &F::Value).await.unwrap(), initial);
    let invalid_name = vec![change("#name", F::Value, json!("must not apply")), change("#text", F::Attribute { name: "bad name".into() }, json!("x"))];
    assert!(matches!(g.state_apply(RequestId::new("invalid-name").unwrap(), &initial.boundary, &invalid_name).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
    assert_eq!(g.state_read(&css("#name"), &F::Value).await.unwrap(), initial);
    let applied = g.state_apply(RequestId::new("set").unwrap(), &initial.boundary, &[
        change("#name", F::Value, json!("Ada")), change("#check", F::Checked, json!(true)),
        change("#text", F::Text, json!("Changed")), change("#text", F::Style { name: "color".into() }, json!("red")),
    ]).await.unwrap();
    assert!(applied.completed, "{applied:?}");
    assert_eq!(applied.boundary.t_us, initial.boundary.t_us);
    assert!(applied.boundary.state_revision > initial.boundary.state_revision);
    assert!(applied.receipts.iter().all(|r| r.attempted && r.verified));
    assert_eq!(g.state_read(&css("#check"), &F::Checked).await.unwrap().value, true);
    assert_eq!(g.eval_main("JSON.stringify(events)").await.unwrap(), "[]");
    let mut guarded = change("#name", F::Value, json!("final"));
    guarded.expected = Some(ExpectedValue { value: json!("Ada") });
    let partial = g.state_apply(RequestId::new("partial").unwrap(), &applied.boundary, &[
        change("#widget", F::Attribute { name: "data-change".into() }, json!("yes")), guarded,
        change("#text", F::Text, json!("must not apply")),
    ]).await.unwrap();
    assert!(!partial.completed);
    assert_eq!(partial.receipts.len(), 2);
    assert!(partial.receipts[0].verified);
    assert!(!partial.receipts[1].attempted);
    assert_eq!(g.state_read(&css("#name"), &F::Value).await.unwrap().value, "reaction");
    assert_eq!(g.state_read(&css("#text"), &F::Text).await.unwrap().value, "Changed");
    let trace = std::fs::read_to_string(g.trace_path()).unwrap();
    assert!(trace.contains("\"kind\":\"intervention\""));
    assert!(!trace.contains("\"kind\":\"action\""));
    assert!(!trace.contains("arbitrary_evaluate"));
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_state_plan_cannot_resume_into_a_replacement_document() {
    use frontcap_core::{action::Action, intervention::{StateChange, StateField}};
    let runs = tempfile::tempdir().unwrap();
    let url = format!("http://{}/state.html", support::serve());
    let mut g = Generation::launch(support::options(runs.path(), "state-replaced", url.clone(), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let boundary = g.observation_boundary().await.unwrap();
    let changes = vec![StateChange { target: css("#name"), field: StateField::Value, value: "first".into(), expected: None }];
    let request = RequestId::new("write").unwrap();
    let plan = g.state_plan(&boundary, &changes).await.unwrap();
    g.state_begin(&request).await.unwrap();
    let interrupted = g.state_run(&request, &plan, |stage, _, _| {
        if stage == "state_after_write" { Err(frontcap_chromium::ChromiumError::InterventionUncertain("test interruption".into())) } else { Ok(()) }
    }).await;
    assert!(interrupted.is_err());
    g.navigate_action(&RequestId::new("replace").unwrap(), &Action::Navigate { url }).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    assert!(matches!(g.state_run(&request, &plan, |_, _, _| Ok(())).await, Err(frontcap_chromium::ChromiumError::InterventionUncertain(_))));
    assert_eq!(g.state_read(&css("#name"), &StateField::Value).await.unwrap().value, "before");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn storage_changes_check_partition_and_context_and_do_not_reload_or_fabricate_events() {
    use frontcap_core::intervention::{StateChange, StateField, StorageArea};
    use serde_json::json;
    let runs = tempfile::tempdir().unwrap();
    let url = format!("http://{}/storage.html", support::serve());
    let mut g = Generation::launch(support::options(runs.path(), "storage", url.clone(), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(101)).await.unwrap();
    let scoped = g.storage_scope(&css("#anchor")).await.unwrap();
    assert_eq!(scoped.scope.origin, url.trim_end_matches("/storage.html"));
    assert!(!scoped.scope.storage_key.is_empty());
    let local = StateField::Storage { area: StorageArea::Local, scope: scoped.scope.clone(), key: "cart".into() };
    let session = StateField::Storage { area: StorageArea::Session, scope: scoped.scope.clone(), key: "cart".into() };
    assert_eq!(g.state_read(&css("#anchor"), &local).await.unwrap().value, json!(null));
    let applied = g.state_apply(RequestId::new("storage-set").unwrap(), &scoped.boundary, &[
        StateChange { target: css("#anchor"), field: local.clone(), value: "local-cart".into(), expected: None },
        StateChange { target: css("#anchor"), field: session.clone(), value: "session-cart".into(), expected: None },
    ]).await.unwrap();
    assert!(applied.completed, "{applied:?}");
    assert_eq!(applied.boundary.t_us, scoped.boundary.t_us);
    let child = Locator::Frame { frame: Box::new(css("#child")), target: Box::new(css("#inner")) };
    assert_eq!(g.storage_scope(&child).await.unwrap().scope, scoped.scope);
    assert_eq!(g.state_read(&child, &local).await.unwrap().value, "local-cart");
    assert_eq!(g.state_read(&child, &session).await.unwrap().value, "session-cart");
    assert_eq!(g.eval_main("JSON.stringify(storageEvents)").await.unwrap(), "[]");
    assert_eq!(g.query(&css("#boot"), &[Prop::Text]).await.unwrap().values["text"], "empty");
    let mut wrong = scoped.scope.clone();
    wrong.storage_key.push_str("wrong-partition");
    let bad = vec![
        StateChange { target: css("#boot"), field: StateField::Text, value: "must not apply".into(), expected: None },
        StateChange { target: css("#anchor"), field: StateField::Storage { area: StorageArea::Local, scope: wrong, key: "cart".into() }, value: "wrong".into(), expected: None },
    ];
    assert!(matches!(g.state_apply(RequestId::new("wrong-partition").unwrap(), &applied.boundary, &bad).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
    assert_eq!(g.query(&css("#boot"), &[Prop::Text]).await.unwrap().values["text"], "empty");
    let removed = g.state_apply(RequestId::new("remove").unwrap(), &applied.boundary, &[StateChange { target: css("#anchor"), field: local.clone(), value: json!(null), expected: None }]).await.unwrap();
    assert!(removed.completed);
    assert_eq!(g.state_read(&child, &local).await.unwrap().value, json!(null));
    assert_eq!(g.state_read(&child, &session).await.unwrap().value, "session-cart");
    let mut independent = Generation::launch(support::options(runs.path(), "storage-other", url, RuleSet::default())).await.unwrap();
    independent.advance(VirtualDuration::from_ms(51)).await.unwrap();
    let scope = independent.storage_scope(&css("#anchor")).await.unwrap();
    let other = StateField::Storage { area: StorageArea::Session, scope: scope.scope, key: "cart".into() };
    assert_eq!(independent.state_read(&css("#anchor"), &other).await.unwrap().value, json!(null));
    independent.stop().await.unwrap();
    g.stop().await.unwrap();
}

#[tokio::test]
async fn storage_access_in_another_origin_requires_its_own_scope() {
    use frontcap_core::intervention::{StateChange, StateField, StorageArea};
    let runs = tempfile::tempdir().unwrap();
    let addr = support::serve();
    let child_url = format!("http://localhost:{}/storage.html", addr.port());
    let mut options = support::options(runs.path(), "storage-origin", format!("http://{addr}/child_frames.html?child={child_url}"), RuleSet::default());
    options.passthrough_origins = Some(vec![format!("http://{addr}"), format!("http://localhost:{}", addr.port())]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(101)).await.unwrap();
    let top = g.storage_scope(&css("#status")).await.unwrap();
    let child = Locator::Frame { frame: Box::new(css("#widget")), target: Box::new(css("#anchor")) };
    let scoped = g.storage_scope(&child).await.unwrap();
    assert_eq!(scoped.scope.origin, format!("http://localhost:{}", addr.port()));
    assert_ne!(scoped.scope.storage_key, top.scope.storage_key);
    let wrong = StateField::Storage { area: StorageArea::Local, scope: top.scope.clone(), key: "cart".into() };
    assert!(matches!(g.state_read(&child, &wrong).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))));
    let field = StateField::Storage { area: StorageArea::Local, scope: scoped.scope, key: "cart".into() };
    let changed = g.state_apply(RequestId::new("child-storage").unwrap(), &scoped.boundary, &[StateChange { target: child.clone(), field: field.clone(), value: "child-only".into(), expected: None }]).await.unwrap();
    assert!(changed.completed, "{changed:?}");
    assert_eq!(g.state_read(&child, &field).await.unwrap().value, "child-only");
    assert!(g.state_read(&css("#status"), &wrong).await.unwrap().value.is_null());
    g.stop().await.unwrap();
}

#[tokio::test]
async fn initial_storage_is_installed_before_first_script_and_not_reapplied_on_reload() {
    use frontcap_core::{action::Action, intervention::InitialStorage};
    use serde_json::json;
    let runs = tempfile::tempdir().unwrap();
    let addr = support::serve();
    let mut options = support::options(runs.path(), "initial-state", format!("http://{addr}/initial-state.html"), RuleSet::default());
    options.profile.initial_storage = Some(InitialStorage {
        origin: format!("http://{addr}"),
        local: [("cart".into(), "large-cart".into())].into(),
        session: [("login".into(), "signed-in".into())].into(),
    });
    let mut g = Generation::launch(options).await.unwrap();
    assert_eq!(g.now().0, 0);
    assert_eq!(g.eval_main("typeof globalThis.boot").await.unwrap(), "undefined", "initialization must precede the first app script");
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    assert_eq!(g.eval_main("boot").await.unwrap(), json!({"local":"large-cart","session":"signed-in"}));
    g.navigate_action(&RequestId::new("reload").unwrap(), &Action::Reload {}).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    assert_eq!(g.eval_main("boot").await.unwrap(), json!({"local":"changed-by-app","session":"changed-by-app"}), "reload must preserve app changes rather than reapply the recipe");
    let trace = std::fs::read_to_string(g.trace_path()).unwrap();
    assert_eq!(trace.lines().filter(|line| line.contains("\"kind\":\"initialization\"")).count(), 1);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn initial_storage_for_another_origin_refuses_to_publish_a_generation() {
    use frontcap_core::intervention::InitialStorage;
    let runs = tempfile::tempdir().unwrap();
    let addr = support::serve();
    let mut options = support::options(runs.path(), "initial-wrong", format!("http://{addr}/initial-state.html"), RuleSet::default());
    options.profile.initial_storage = Some(InitialStorage { origin: "https://other.example".into(), local: [("cart".into(), "wrong".into())].into(), ..Default::default() });
    assert!(matches!(Generation::launch(options).await, Err(frontcap_chromium::ChromiumError::InvalidArgument(message)) if message.contains("does not match committed")));
}
