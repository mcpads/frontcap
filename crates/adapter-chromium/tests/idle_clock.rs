mod support;

use frontcap_chromium::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::json;

async fn page() -> (tempfile::TempDir, Generation) {
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(&json!([{"id":"page","match":{"url":"https://idle.example.test/"},
        "action":{"type":"mock","status":200,"headers":{"content-type":"text/html",
            "cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},
            "body":"<!doctype html><p>quiet</p>"}}]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "idle-clock", "https://idle.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    // Linux delivers initial pointer movement. Its preloading predictor samples
    // that movement for 500 ms; this is finite input work, not periodic memory
    // maintenance. Measure the quiet period after it has finished.
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    (runs, g)
}

#[tokio::test]
async fn idle_advances_do_not_render_for_periodic_maintenance() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let (_runs, mut g) = page().await;
    for ms in [60_000, 3_600_000, 30 * 24 * 3_600_000] {
        let before = g.now();
        let frames = g.frame_requests();
        g.advance(VirtualDuration::from_ms(ms)).await.unwrap();
        assert_eq!(g.now().0, before.0 + ms * 1000);
        let frames = g.frame_requests() - frames;
        assert!(frames < 8, "idle {ms} ms requested {frames} native frames");
    }
    assert_eq!(g.eval_main("document.querySelector('p').textContent").await.unwrap(), "quiet");
    g.stop().await.unwrap();
}

#[tokio::test]
async fn page_and_v8_delayed_completions_still_wake_at_their_deadlines() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let (_runs, mut g) = page().await;
    assert_eq!(g.eval_main("crossOriginIsolated && typeof SharedArrayBuffer === 'function'").await.unwrap(), true);
    g.eval_main(r#"window.events=[];window.compiled=false;const started=Date.now();
setTimeout(()=>events.push(['timer',Date.now()-started]),5000);
Atomics.waitAsync(new Int32Array(new SharedArrayBuffer(4)),0,0,7000).value
  .then(value=>events.push([value,Date.now()-started]));
WebAssembly.compile(new Uint8Array([0,97,115,109,1,0,0,0])).then(()=>compiled=true);
true"#).await.unwrap();
    g.advance(VirtualDuration::from_ms(4999)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!([]));
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!([["timer",5000]]));
    g.advance(VirtualDuration::from_ms(1999)).await.unwrap();
    assert_eq!(g.eval_main("events.length").await.unwrap(), 1);
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!([["timer",5000],["timed-out",7000]]));
    assert_eq!(g.eval_main("compiled").await.unwrap(), true);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn worker_gc_does_not_schedule_idle_frames_or_delay_its_next_timer() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let (_runs, mut g) = page().await;
    let worker = r#"for(let n=0;n<5;n++){let garbage=Array.from({length:100000},(_,i)=>({i,text:String(i)}));}
postMessage('ready');onmessage=()=>setTimeout(()=>postMessage(Date.now()),123);"#;
    g.eval_main(&format!("window.received=[];window.worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));worker.onmessage=e=>received.push(e.data);true",json!(worker))).await.unwrap();
    g.advance(VirtualDuration::from_ms(250)).await.unwrap();
    assert_eq!(g.eval_main("received").await.unwrap(),json!(["ready"]));
    let frames=g.frame_requests();
    g.advance(VirtualDuration::from_ms(3_600_000)).await.unwrap();
    assert!(g.frame_requests()-frames<8,"worker maintenance requested {} frames",g.frame_requests()-frames);
    let date=g.eval_main("Date.now()").await.unwrap().as_u64().unwrap();
    g.eval_main("worker.postMessage('timer')").await.unwrap();
    g.advance(VirtualDuration::from_ms(122)).await.unwrap();
    assert_eq!(g.eval_main("received").await.unwrap(),json!(["ready"]));
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.eval_main("received").await.unwrap(),json!(["ready",date+123]));
    g.stop().await.unwrap();
}
