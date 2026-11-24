//! P05 provider oracle, stage 6: an image taken while a platform-scripted stream is open and a child frame
//! runs its own timer. Two children and the source receive the stream's remaining messages at the same
//! virtual instants and continue the frame's timer and hidden state independently.
mod support;

use frontcap_chromium::generation::{Cell, CellUser, Endpoint, Generation, ReconnectOptions, Restoring};
use frontcap_core::event::ControllerChange;
use frontcap_core::ids::{LaunchId, RequestId, RunId};
use frontcap_core::observation::Prop;
use frontcap_core::predicate::{Locator, WatchRegistry};
use frontcap_core::rules::RuleSet;
use frontcap_core::time::VirtualDuration;
use serde_json::Value;
use std::path::Path;

const MOUNT: &str = "/run/frontcap-cell-oracle";

fn css(selector: &str) -> Locator { Locator::Css { selector: selector.into() } }

fn rules() -> RuleSet {
    RuleSet::from_json(&serde_json::json!([
        {"id":"document","match":{"url":"*checkpoint-contexts.html"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html"},"body":include_str!("fixtures/checkpoint-contexts.html")}},
        {"id":"feed","match":{"url":"wss://stream.test/*"},"action":{"type":"stream_mock","messages":[
            {"at_ms":100,"data":"m1"},{"at_ms":400,"data":"m2"},{"at_ms":700,"data":"m3"},{"at_ms":1000,"data":"m4"},{"at_ms":1300,"data":"m5"}]}}
    ]).to_string()).unwrap()
}

fn reconnect(runs: &Path, run: &str, launch_id: LaunchId, endpoint: Endpoint) -> ReconnectOptions {
    reconnect_with(runs, run, launch_id, endpoint, support::options(runs, run, String::new(), RuleSet::default()).profile)
}

/// As the recorder does, with the launch's own profile (its popup policy among it).
fn reconnect_with(runs: &Path, run: &str, launch_id: LaunchId, endpoint: Endpoint, profile: frontcap_core::profile::DeterminismProfile) -> ReconnectOptions {
    ReconnectOptions {
        repo_root: support::repo_root(),
        runs_root: runs.to_path_buf(),
        run: RunId::new(run).unwrap(),
        launch_id,
        profile,
        endpoint,
        watches: WatchRegistry::default(),
        watchdog: std::time::Duration::from_secs(10),
        granted_until: None,
        dialog_rules: Default::default(),
        upstream: None,
    }
}

async fn text(g: &mut Generation, selector: &str) -> Value {
    g.query(&css(selector), &[Prop::Text]).await.unwrap().values["text"].clone()
}

/// `extra` more hidden steps, then 900 ms: the stream's state and the frame's report, and when.
async fn continuation(g: &mut Generation, extra: usize) -> (Value, Value, u64) {
    for n in 0..extra {
        g.click(RequestId::new(format!("extra-{n}")).unwrap(), &css("#step")).await.unwrap();
    }
    g.advance(VirtualDuration::from_ms(900)).await.unwrap();
    (text(g, "#stream").await, text(g, "#frame").await, g.now().0)
}

#[tokio::test]
#[ignore = "creates namespaces and runs CRIU with the patched engine: run as root on Linux with --ignored"]
async fn an_open_stream_and_a_frames_timer_continue_in_each_child() {
    let runs = tempfile::tempdir().unwrap();
    let cells = tempfile::tempdir().unwrap();
    std::fs::set_permissions(cells.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let cell = Cell::create(&cells.path().join("source"), Path::new(MOUNT), CellUser { uid: 65534, gid: 65534 }).unwrap();
    let mut opts = support::options(runs.path(), "source", "https://app.test/checkpoint-contexts.html".into(), rules());
    opts.passthrough_origins = Some(vec![]);
    opts.cell = Some(cell.clone());
    let mut g = Generation::launch(opts).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    g.click(RequestId::new("open").unwrap(), &css("#open")).await.unwrap();
    g.advance(VirtualDuration::from_ms(450)).await.unwrap();
    for n in 0..2 {
        g.click(RequestId::new(format!("step-{n}")).unwrap(), &css("#step")).await.unwrap();
    }
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(text(&mut g, "#stream").await, "2:m2:0", "two of the feed's messages before the image");
    let (launch_id, boundary) = (g.launch_id().clone(), g.now());
    assert!(g.image_blockers().is_empty(), "{:?}", g.image_blockers());
    let endpoint = g.let_go().unwrap();

    let image = cell.image(&cells.path().join("image")).unwrap();
    for n in 1..=2 {
        let copied = std::process::Command::new("cp").arg("-a").arg(runs.path().join("source")).arg(runs.path().join(format!("child-{n}"))).status().unwrap();
        assert!(copied.success());
    }
    let (mut source, _) = Generation::adopt(reconnect(runs.path(), "source", launch_id.clone(), endpoint.clone()), ControllerChange::Imaged).await.unwrap();
    let mut children = Vec::new();
    for n in 1..=2 {
        let restoring = Restoring::prepare(&image, &cells.path().join(format!("child-{n}")), Path::new(MOUNT)).unwrap();
        let child = restoring.restore(&image).unwrap();
        let at = Endpoint { browser_pid: child.init, cell: Some(child), ..endpoint.clone() };
        let (g, _) = Generation::adopt(reconnect(runs.path(), &format!("child-{n}"), launch_id.clone(), at), ControllerChange::Restored).await.unwrap();
        assert_eq!(g.now(), boundary);
        children.push(g);
    }

    // The first child takes one more hidden step; the second and the source take none.
    let first = continuation(&mut children[0], 1).await;
    let second = continuation(&mut children[1], 0).await;
    let original = continuation(&mut source, 0).await;
    assert_eq!(original.0, "5:m5:2", "the stream's remaining messages after the image");
    assert_eq!(second, original, "a child receives the stream and runs the frame as the source does");
    assert_eq!(first.0, "5:m5:3", "and on its own");
    let frame_ticks = |v: &Value| v.as_str().unwrap().split(':').next().unwrap().to_string();
    assert_eq!(frame_ticks(&first.1), frame_ticks(&original.1), "the frame's timer, at the same instants");
    assert!(first.1.as_str().unwrap().ends_with(":3"), "{:?}", first.1);
    assert_eq!(first.2, original.2);
    for g in children.into_iter().chain([source]) {
        g.stop().await.unwrap();
    }
}

fn popup_rules() -> RuleSet {
    RuleSet::from_json(&serde_json::json!([
        {"id":"document","match":{"url":"*checkpoint-popup.html"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html"},"body":include_str!("fixtures/checkpoint-popup.html")}},
        {"id":"window","match":{"url":"*checkpoint-popup-window.html"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html"},"body":include_str!("fixtures/checkpoint-popup-window.html")}}
    ]).to_string()).unwrap()
}

/// `extra` more hidden steps, then 600 ms: what the popup shows, and when.
async fn popup_continuation(g: &mut Generation, extra: usize) -> (Value, u64) {
    for n in 0..extra {
        g.click(RequestId::new(format!("extra-{n}")).unwrap(), &css("#step")).await.unwrap();
    }
    g.advance(VirtualDuration::from_ms(600)).await.unwrap();
    g.enter_context("popup-1").unwrap();
    let shown = text(g, "#state").await;
    g.leave_context();
    (shown, g.now().0)
}

#[tokio::test]
#[ignore = "creates namespaces and runs CRIU with the patched engine: run as root on Linux with --ignored"]
async fn an_open_popup_continues_in_each_child() {
    let runs = tempfile::tempdir().unwrap();
    let cells = tempfile::tempdir().unwrap();
    std::fs::set_permissions(cells.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let cell = Cell::create(&cells.path().join("source"), Path::new(MOUNT), CellUser { uid: 65534, gid: 65534 }).unwrap();
    let mut opts = support::options(runs.path(), "source", "https://app.test/checkpoint-popup.html".into(), popup_rules());
    opts.passthrough_origins = Some(vec![]);
    opts.profile.popups = frontcap_core::profile::PopupPolicy::Controlled;
    opts.cell = Some(cell.clone());
    let profile = opts.profile.clone();
    let mut g = Generation::launch(opts).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    g.click(RequestId::new("open").unwrap(), &css("#open")).await.unwrap();
    g.advance(VirtualDuration::from_ms(450)).await.unwrap();
    for n in 0..2 {
        g.click(RequestId::new(format!("step-{n}")).unwrap(), &css("#step")).await.unwrap();
    }
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    g.enter_context("popup-1").unwrap();
    let before = text(&mut g, "#state").await;
    g.leave_context();
    assert!(before.as_str().unwrap().ends_with(":2"), "the popup counted the steps: {before:?}");
    let (launch_id, boundary) = (g.launch_id().clone(), g.now());
    assert!(g.image_blockers().is_empty(), "{:?}", g.image_blockers());
    let endpoint = g.let_go().unwrap();

    let image = cell.image(&cells.path().join("image")).unwrap();
    for n in 1..=2 {
        let copied = std::process::Command::new("cp").arg("-a").arg(runs.path().join("source")).arg(runs.path().join(format!("child-{n}"))).status().unwrap();
        assert!(copied.success());
    }
    let (mut source, _) = Generation::adopt(reconnect_with(runs.path(), "source", launch_id.clone(), endpoint.clone(), profile.clone()), ControllerChange::Imaged).await.unwrap();
    let mut children = Vec::new();
    for n in 1..=2 {
        let restoring = Restoring::prepare(&image, &cells.path().join(format!("child-{n}")), Path::new(MOUNT)).unwrap();
        let child = restoring.restore(&image).unwrap();
        let at = Endpoint { browser_pid: child.init, cell: Some(child), ..endpoint.clone() };
        let (g, _) = Generation::adopt(reconnect_with(runs.path(), &format!("child-{n}"), launch_id.clone(), at, profile.clone()), ControllerChange::Restored).await.unwrap();
        assert_eq!(g.now(), boundary);
        children.push(g);
    }

    let first = popup_continuation(&mut children[0], 1).await;
    let second = popup_continuation(&mut children[1], 0).await;
    let original = popup_continuation(&mut source, 0).await;
    assert!(original.0.as_str().unwrap().ends_with(":2"), "{:?}", original.0);
    assert_eq!(second, original, "a child's popup continues its timer and closure as the source's does");
    let ticks = |v: &Value| v.as_str().unwrap().split(':').next().unwrap().to_string();
    assert_eq!(ticks(&first.0), ticks(&original.0));
    assert!(first.0.as_str().unwrap().ends_with(":3"), "and on its own: {:?}", first.0);
    for g in children.into_iter().chain([source]) {
        g.stop().await.unwrap();
    }
}
