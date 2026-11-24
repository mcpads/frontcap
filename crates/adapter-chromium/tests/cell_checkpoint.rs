//! P05 provider oracle: a generation's browser in a cell is imaged at a stopped boundary and restored into
//! two children that continue its execution (page closure, pending timer, worker heap and a held request)
//! independently, beside the source, which goes on unchanged.
mod support;

use frontcap_chromium::generation::{Cell, CellUser, Endpoint, Generation, ReconnectOptions, Restoring};
use frontcap_core::event::{ControllerChange, EventKind, TaintSource};
use frontcap_core::ids::{LaunchId, RequestId, RunId};
use frontcap_core::observation::Prop;
use frontcap_core::predicate::{Locator, WatchRegistry};
use frontcap_core::rules::{MockSpec, RuleSet};
use frontcap_core::time::VirtualDuration;
use serde_json::Value;
use std::path::Path;

const MOUNT: &str = "/run/frontcap-cell-oracle";

fn css(selector: &str) -> Locator { Locator::Css { selector: selector.into() } }

fn rules() -> RuleSet {
    RuleSet::from_json(&serde_json::json!([
        {"id":"document","match":{"url":"*checkpoint.html"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html"},"body":include_str!("fixtures/checkpoint.html")}},
        {"id":"worker","match":{"url":"*checkpoint-worker.js"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/javascript"},"body":include_str!("fixtures/checkpoint-worker.js")}},
        {"id":"held","match":{"url":"*checkpoint-held"},"action":{"type":"hold"}}
    ]).to_string()).unwrap()
}

fn reconnect(runs: &Path, run: &str, launch_id: LaunchId, endpoint: Endpoint) -> ReconnectOptions {
    ReconnectOptions {
        repo_root: support::repo_root(),
        runs_root: runs.to_path_buf(),
        run: RunId::new(run).unwrap(),
        launch_id,
        profile: support::options(runs, run, String::new(), RuleSet::default()).profile,
        endpoint,
        watches: WatchRegistry::default(),
        watchdog: std::time::Duration::from_secs(10),
        granted_until: None,
        dialog_rules: Default::default(),
        upstream: None,
    }
}

/// `extra` more hidden steps, the held request answered, 400 ms: what the timer, the worker and the
/// response then show, and when.
async fn continuation(g: &mut Generation, extra: usize) -> (Vec<Value>, u64) {
    for n in 0..extra {
        g.click(RequestId::new(format!("extra-{n}")).unwrap(), &css("#step")).await.unwrap();
    }
    let held = g.held();
    assert_eq!(held.len(), 1, "the held request crossed the image");
    g.respond(&held[0], MockSpec { status: 200, headers: Default::default(), body: "answer".into(), body_base64: None, latency_ms: 0 }).await.unwrap();
    g.advance(VirtualDuration::from_ms(400)).await.unwrap();
    let mut shown = Vec::new();
    for selector in ["#timer", "#worker", "#response"] {
        shown.push(g.query(&css(selector), &[Prop::Text]).await.unwrap().values["text"].clone());
    }
    (shown, g.now().0)
}

#[tokio::test]
#[ignore = "creates namespaces and runs CRIU with the patched engine: run as root on Linux with --ignored"]
async fn a_cell_image_restores_into_children_that_continue_its_execution() {
    let runs = tempfile::tempdir().unwrap();
    let cells = tempfile::tempdir().unwrap();
    // A cell's directory is reached only through its mount: its parent is root's alone.
    std::fs::set_permissions(cells.path(), std::os::unix::fs::PermissionsExt::from_mode(0o700)).unwrap();
    let user = CellUser { uid: 65534, gid: 65534 };
    let cell = Cell::create(&cells.path().join("source"), Path::new(MOUNT), user).unwrap();
    let addr = support::serve();
    let mut opts = support::options(runs.path(), "source", format!("http://{addr}/checkpoint.html"), rules());
    opts.cell = Some(cell.clone());
    let mut g = Generation::launch(opts).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    assert_eq!(g.query(&css("#ready"), &[Prop::Text]).await.unwrap().values["text"], "ready");
    for n in 0..2 {
        g.click(RequestId::new(format!("step-{n}")).unwrap(), &css("#step")).await.unwrap();
    }
    g.click(RequestId::new("arm").unwrap(), &css("#arm")).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    for selector in ["#timer", "#worker", "#response"] {
        assert_eq!(g.query(&css(selector), &[Prop::Text]).await.unwrap().values["text"], "pending");
    }
    let (endpoint, launch_id, boundary) = (g.endpoint(), g.launch_id().clone(), g.now());
    g.abandon();

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
        // The child's browser is its own cell's init, never the source's.
        let stale = Endpoint { cell: Some(child.clone()), ..endpoint.clone() };
        let refused = Generation::adopt(reconnect(runs.path(), &format!("child-{n}"), launch_id.clone(), stale), ControllerChange::Restored).await;
        assert!(matches!(refused, Err(frontcap_chromium::ChromiumError::InvalidArgument(_))), "{:?}", refused.err());
        let at =Endpoint { browser_pid: child.init, cell: Some(child), ..endpoint.clone() };
        let (g, _) = Generation::adopt(reconnect(runs.path(), &format!("child-{n}"), launch_id.clone(), at), ControllerChange::Restored).await.unwrap();
        assert_eq!(g.now(), boundary, "a child starts at the source's boundary");
        children.push(g);
    }

    // The first child takes one more hidden step; the second and the source take none.
    let first = continuation(&mut children[0], 1).await;
    let second = continuation(&mut children[1], 0).await;
    let original = continuation(&mut source, 0).await;
    assert_eq!(original.0, [Value::from("2"), Value::from("2"), Value::from("answer:2")]);
    assert_eq!(second, original, "a child continues the saved closure, timer, worker heap and held request");
    assert_eq!(first.0, [Value::from("3"), Value::from("3"), Value::from("answer:3")], "and on its own");
    assert_eq!(first.1, original.1);
    // Letting go for an image, and a child's adoption, are not recorder faults.
    for (g, change) in [(&source, ControllerChange::Imaged), (&children[0], ControllerChange::Restored)] {
        let events = frontcap_core::log::BehaviorLog::read(&g.trace_path()).unwrap();
        assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Controller(c) if c.change == change)));
        assert!(!events.iter().any(|e| matches!(&e.kind, EventKind::Taint(t) if t.source == TaintSource::RecorderFault)));
    }
    for g in children.into_iter().chain([source]) {
        g.stop().await.unwrap();
    }
}
