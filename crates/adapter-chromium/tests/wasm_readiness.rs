//! Ordinary Wasm streaming APIs are distinct from experimental source-phase imports.
mod support;
use frontcap_chromium::generation::Generation;
use frontcap_core::{encode::base64, rules::{MockSpec, RuleSet}, time::VirtualDuration};
use serde_json::json;

fn leb(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let byte=(value & 0x7f) as u8;value >>= 7;
        out.push(byte | if value==0 {0} else {0x80});
        if value==0 {break;}
    }
}
fn section(id: u8, bytes: &[u8], out: &mut Vec<u8>) {
    out.push(id);leb(bytes.len(),out);out.extend_from_slice(bytes);
}
/// One exported constant function and independently compilable neighbors, without external tools.
fn module(functions: usize) -> Vec<u8> {
    let mut bytes=b"\0asm\x01\0\0\0".to_vec();
    section(1,&[1,0x60,0,1,0x7f],&mut bytes);
    let mut types=Vec::new();leb(functions,&mut types);types.resize(types.len()+functions,0);
    section(3,&types,&mut bytes);
    section(7,&[1,6,b'a',b'n',b's',b'w',b'e',b'r',0,0],&mut bytes);
    let mut code=Vec::new();leb(functions,&mut code);
    for _ in 0..functions {code.extend_from_slice(&[4,0,0x41,42,0x0b]);}
    section(10,&code,&mut bytes);bytes
}
fn response(bytes: &[u8], latency_ms: u64) -> MockSpec {
    serde_json::from_value(json!({"status":200,"headers":{"content-type":"application/wasm","access-control-allow-origin":"*"},"body_base64":base64(bytes),"latency_ms":latency_ms})).unwrap()
}

/// Both a small module and a many-function module must settle at the controlled response instant.
/// This does not claim the experimental source-phase module loader's readiness vote was exercised.
#[tokio::test]
async fn held_and_delayed_streaming_compilation_finishes_at_the_response_instant() {
    if !support::controlled_engine_or_check_baseline().await {return;}
    let addr=support::serve();
    for functions in [1,4096] {
        for held in [false,true] {
            let bytes=module(functions);
            let action=if held {json!({"type":"hold"})} else {
                let mut action=serde_json::to_value(response(&bytes,300)).unwrap();action["type"]=json!("mock");action
            };
            let rules=RuleSet::from_json(&json!([{"id":"wasm","match":{"url":"https://wasm.test/module.wasm"},"action":action}]).to_string()).unwrap();
            let runs=tempfile::tempdir().unwrap();
            let mut g=Generation::launch(support::options(runs.path(),&format!("wasm-{functions}-{held}"),format!("http://{addr}/wasm_readiness.html"),rules)).await.unwrap();
            g.advance(VirtualDuration::from_ms(100)).await.unwrap();
            assert_eq!(g.eval_main("events").await.unwrap(),json!([["timer",50]]));
            assert_eq!(g.eval_main("document.querySelector('#status').textContent").await.unwrap(),"waiting");
            let instant=if held {
                let pending=g.held_ordinals();assert_eq!(pending.len(),1);
                g.respond(&pending[0],response(&bytes,0)).await.unwrap();100
            } else {300};
            g.advance(VirtualDuration::from_ms(250)).await.unwrap();
            assert_eq!(g.eval_main("events").await.unwrap(),json!([["timer",50],["wasm",instant,42]]),"functions={functions}, held={held}");
            assert_eq!(g.eval_main("document.querySelector('#status').textContent").await.unwrap(),"ready");
            g.stop().await.unwrap();
        }
    }
}

#[tokio::test]
async fn invalid_streaming_modules_reject_at_release_and_leave_later_timers_serviceable() {
    if !support::controlled_engine_or_check_baseline().await {return;}
    let addr=support::serve();
    for held in [false,true] {
        let action=if held {json!({"type":"hold"})} else {
            let mut action=serde_json::to_value(response(b"not a Wasm module",300)).unwrap();action["type"]=json!("mock");action
        };
        let rules=RuleSet::from_json(&json!([{"id":"wasm","match":{"url":"https://wasm.test/module.wasm"},"action":action}]).to_string()).unwrap();
        let runs=tempfile::tempdir().unwrap();
        let mut g=Generation::launch(support::options(runs.path(),&format!("wasm-error-{held}"),format!("http://{addr}/wasm_readiness.html"),rules)).await.unwrap();
        g.advance(VirtualDuration::from_ms(100)).await.unwrap();
        assert_eq!(g.eval_main("events").await.unwrap(),json!([["timer",50]]));
        let instant=if held {
            let pending=g.held_ordinals();assert_eq!(pending.len(),1);
            g.respond(&pending[0],response(b"not a Wasm module",0)).await.unwrap();100
        } else {300};
        g.advance(VirtualDuration::from_ms(250)).await.unwrap();
        assert_eq!(g.eval_main("events").await.unwrap(),json!([["timer",50],["error",instant,"CompileError"]]));
        g.eval_main("setTimeout(()=>events.push(['after',performance.now()]),25); true").await.unwrap();
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        assert_eq!(g.eval_main("events.at(-1)").await.unwrap(),json!(["after",375]));
        assert_eq!(g.now().0,400_000);g.stop().await.unwrap();
    }
}

/// Removal while the response is pending must not retain the child's readiness work.
/// This does not establish cancellation during an already-running compiler task.
#[tokio::test]
async fn removing_a_streaming_wasm_document_preserves_surviving_document_timers() {
    if !support::controlled_engine_or_check_baseline().await {return;}
    let addr=support::serve();
    for held in [false,true] {
        let action=if held {json!({"type":"hold"})} else {
            let mut action=serde_json::to_value(response(&module(4096),300)).unwrap();
            action["type"]=json!("mock");action
        };
        let rules=RuleSet::from_json(&json!([{"id":"wasm","match":{"url":"https://wasm.test/module.wasm"},"action":action}]).to_string()).unwrap();
        let runs=tempfile::tempdir().unwrap();
        let mut g=Generation::launch(support::options(runs.path(),&format!("wasm-removed-{held}"),format!("http://{addr}/wasm_context.html"),rules)).await.unwrap();
        g.advance(VirtualDuration::from_ms(50)).await.unwrap();
        assert_eq!(g.eval_main("document.querySelector('#child').contentWindow.events").await.unwrap(),json!([["timer",50]]));
        if held {assert_eq!(g.held_ordinals().len(),1);}
        g.advance(VirtualDuration::from_ms(350)).await.unwrap();
        assert_eq!(g.eval_main("events").await.unwrap(),json!([
            ["before",50],["removed",100],["after",150],["later",350]
        ]),"held={held}");
        assert_eq!(g.eval_main("document.querySelector('#child') === null").await.unwrap(),true);
        assert_eq!(g.now().0,400_000);
        g.stop().await.unwrap();
    }
}
