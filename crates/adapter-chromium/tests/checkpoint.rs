//! P05 continuation oracle. This does not implement or certify an execution checkpoint.
mod support;

use frontcap_chromium::Generation;
use frontcap_core::ids::RequestId;
use frontcap_core::intervention::{InitialStorage, StateField, StorageArea};
use frontcap_core::observation::Prop;
use frontcap_core::predicate::Locator;
use frontcap_core::rules::{MockSpec, RuleSet};
use frontcap_core::time::VirtualDuration;
use serde_json::Value;

fn css(selector: &str) -> Locator { Locator::Css { selector: selector.into() } }

struct Evidence {
    visible: Value,
    pixels: Vec<u8>,
    storage: Value,
    continuation: Vec<Value>,
}

async fn scenario(addr: std::net::SocketAddr, name: &str, steps: usize, delayed_reads: bool) -> Evidence {
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(&serde_json::json!([
        {"id":"document","match":{"url":"*checkpoint.html"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html"},"body":include_str!("fixtures/checkpoint.html")}},
        {"id":"worker","match":{"url":"*checkpoint-worker.js"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/javascript"},"body":include_str!("fixtures/checkpoint-worker.js")}},
        {"id":"held","match":{"url":"*checkpoint-held"},"action":{"type":"hold"}}
    ]).to_string()).unwrap();
    let mut options = support::options(runs.path(), name, format!("http://{addr}/checkpoint.html"), rules);
    options.profile.initial_storage = Some(InitialStorage {
        origin: format!("http://{addr}"), local: [("cart".into(), "large".into())].into(), session: Default::default(),
    });
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(51)).await.unwrap();
    assert_eq!(g.query(&css("#ready"), &[Prop::Text]).await.unwrap().values["text"], "ready");
    for n in 0..steps {
        g.click(RequestId::new(format!("step-{n}")).unwrap(), &css("#step")).await.unwrap();
    }
    g.click(RequestId::new("arm").unwrap(), &css("#arm")).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let at = g.now();
    let scene = g.scene(None, None, 100, true, true).await.unwrap();
    eprintln!("{name}: engine={}, boundary={}us", scene.engine_revision, at.0);
    let visible = g.eval_main("document.body.outerHTML").await.unwrap();
    let pixels = std::fs::read(scene.presentation.capture.unwrap().path).unwrap();
    let scope = g.storage_scope(&css("body")).await.unwrap().scope;
    let field = StateField::Storage { area: StorageArea::Local, scope, key: "cart".into() };
    let storage = g.state_read(&css("body"), &field).await.unwrap().value;
    let held = g.held();
    assert_eq!(held.len(), 1);
    for selector in ["#timer", "#worker", "#response"] {
        assert_eq!(g.query(&css(selector), &[Prop::Text]).await.unwrap().values["text"], "pending");
    }
    if delayed_reads {
        for delay in [10, 70, 25] {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            assert_eq!(g.eval_main("document.body.outerHTML").await.unwrap(), visible);
            assert_eq!(g.now(), at);
            assert_eq!(g.held(), held);
        }
    }
    // The same future input must expose the saved closure, timer and worker state, not merely
    // recreate its pixels. The response continuation also depends on the original pending fetch.
    g.respond(&held[0], MockSpec { status: 200, headers: Default::default(), body: "answer".into(), body_base64: None, latency_ms: 0 }).await.unwrap();
    g.advance(VirtualDuration::from_ms(400)).await.unwrap();
    let mut continuation = Vec::new();
    for selector in ["#timer", "#worker", "#response"] {
        continuation.push(g.query(&css(selector), &[Prop::Text]).await.unwrap().values["text"].clone());
    }
    assert!(g.held().is_empty());
    assert_eq!(g.now().0, 501_000);
    for line in std::fs::read_to_string(g.trace_path()).unwrap().lines() {
        let event: frontcap_core::event::BehaviorEvent = serde_json::from_str(line).unwrap();
        if let frontcap_core::event::EventKind::Taint(taint) = event.kind {
            // The adapter reports worker entropy coverage even for this fixed, non-random fixture.
            // Keep that gap visible; this oracle proves no claim about worker random state.
            assert!(taint.source == frontcap_core::event::TaintSource::UnseededRandomness
                && taint.detail.ends_with("/checkpoint-worker.js"), "unexpected coverage gap: {line}");
            eprintln!("{name}: known entropy coverage gap: {}", taint.detail);
        }
    }
    g.stop().await.unwrap();
    Evidence { visible, pixels, storage, continuation }
}

#[tokio::test]
#[ignore = "P05 mechanism investigation; run explicitly with --ignored --nocapture"]
async fn identical_visible_and_storage_state_do_not_identify_an_execution_checkpoint() {
    assert!(browser_pin::engine::patched_binary(&support::repo_root(), browser_pin::current_platform().unwrap()).unwrap().is_some());
    let addr = support::serve();
    let source = scenario(addr, "checkpoint-source", 2, false).await;
    let repeated = scenario(addr, "checkpoint-repeat", 2, true).await;
    let reconstructed = scenario(addr, "checkpoint-reconstructed", 0, false).await;
    for other in [&repeated, &reconstructed] {
        assert_eq!(source.visible, other.visible);
        assert_eq!(source.pixels, other.pixels);
        assert_eq!(source.storage, other.storage);
    }
    assert_eq!(source.continuation, [Value::from("2"), Value::from("2"), Value::from("answer:2")]);
    assert_eq!(source.continuation, repeated.continuation, "host delay and additional observations must preserve continuation");
    assert_eq!(reconstructed.continuation, [Value::from("0"), Value::from("0"), Value::from("answer:0")]);
    assert_ne!(source.continuation, reconstructed.continuation, "matching DOM, pixels and storage is insufficient restoration evidence");
}
