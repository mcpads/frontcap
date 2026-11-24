mod support;

use frontcap_chromium::generation::Generation;
use frontcap_core::rules::{MockSpec, RuleSet};
use frontcap_core::time::VirtualDuration;
use serde_json::json;

fn response(latency_ms: u64) -> MockSpec {
    serde_json::from_value(json!({
        "status": 200, "headers": {"content-type": "text/javascript", "access-control-allow-origin": "*"},
        "body": "events.push(['script', performance.now()]); document.querySelector('#status').textContent = 'ready';",
        "latency_ms": latency_ms
    })).unwrap()
}

/// A timer remains observable while a script's response is withheld. Releasing it executes the script
/// at the response instant, for parser-blocking, async, module and dynamically inserted scripts.
#[tokio::test]
async fn withheld_scripts_do_not_stop_virtual_time() {
    let addr = support::serve();
    for kind in ["classic", "async", "module", "inserted"] {
        for held in [false, true] {
            let runs = tempfile::tempdir().unwrap();
            let action = if held { json!({"type": "hold"}) } else {
                let mut action = serde_json::to_value(response(300)).unwrap();
                action["type"] = json!("mock");
                action
            };
            let rules = RuleSet::from_json(&json!([{
                "id": "script", "match": {"url": "https://scripts.test/*"}, "action": action
            }]).to_string()).unwrap();
            let mut g = Generation::launch(support::options(runs.path(), &format!("script-{kind}-{held}"),
                format!("http://{addr}/delayed_scripts.html?kind={kind}"), rules)).await.unwrap();
            eprintln!("script {kind}, held={held}");
            g.advance(VirtualDuration::from_ms(100)).await.unwrap();
            assert_eq!(g.eval_main("events").await.unwrap(), json!([["timer", 50]]));
            assert_eq!(g.eval_main("document.querySelector('#status').textContent").await.unwrap(), "waiting");
            let expected = if held {
                let requests = g.held_ordinals();
                assert_eq!(requests.len(), 1);
                g.respond(&requests[0], response(0)).await.unwrap();
                100
            } else { 300 };
            g.advance(VirtualDuration::from_ms(250)).await.unwrap();
            assert_eq!(g.eval_main("events").await.unwrap(), json!([["timer", 50], ["script", expected]]), "{kind}, held={held}");
            assert_eq!(g.eval_main("document.querySelector('#status').textContent").await.unwrap(), "ready");
        }
    }
}

/// The pending script's vote must not stop an unrelated response, nor a module's dependent load.
/// A large module body also exercises the resource/streaming completion boundary.
#[tokio::test]
async fn a_held_script_does_not_block_a_streamed_module_graph() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut entry = response(100);
    entry.body = "import './dependency.js'; events.push(['module', performance.now()]);".into();
    let mut dependency = response(200);
    dependency.body = format!("/*{}*/\nevents.push(['dependency', performance.now()]);", "streaming source ".repeat(16_384));
    let mock = |spec: MockSpec| {
        let mut action = serde_json::to_value(spec).unwrap();
        action["type"] = json!("mock"); action
    };
    let rules = RuleSet::from_json(&json!([
        {"id":"held", "match":{"url":"https://scripts.test/entry.js"}, "action":{"type":"hold"}},
        {"id":"module", "match":{"url":"https://scripts.test/module.js"}, "action":mock(entry)},
        {"id":"dependency", "match":{"url":"https://scripts.test/dependency.js"}, "action":mock(dependency)}
    ]).to_string()).unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "script-graph",
        format!("http://{addr}/delayed_scripts.html?kind=inserted"), rules)).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let held = g.held_ordinals();
    assert_eq!(held.len(), 1);
    g.eval_main("(() => { const s = document.createElement('script'); s.type='module'; s.src='https://scripts.test/module.js'; document.head.append(s); })()").await.unwrap();
    g.advance(VirtualDuration::from_ms(400)).await.unwrap();
    assert_eq!(g.held_ordinals(), held);
    assert_eq!(g.eval_main("events").await.unwrap(), json!([["timer",50],["dependency",400],["module",400]]));
    g.respond(&held[0], response(0)).await.unwrap();
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(g.eval_main("events.at(-1)").await.unwrap(), json!(["script",500]));
}
