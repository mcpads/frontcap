mod support;

use cdp::protocol::{emulation, target};
use frontcap_chromium::Generation;
use frontcap_core::{event::{BehaviorEvent, EventKind, TaintSource}, rules::RuleSet, time::VirtualDuration};
use serde_json::json;
use std::time::Duration;

async fn page(worker: &str) -> (tempfile::TempDir, Generation) {
    let runs = tempfile::tempdir().unwrap();
    let rules = RuleSet::from_json(&json!([{"id":"page","match":{"url":"https://atomics.example.test/"},
        "action":{"type":"mock","status":200,"headers":{"content-type":"text/html",
            "cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},
            "body":"<!doctype html><p>atomic waits</p>"}}]).to_string()).unwrap();
    let mut options = support::options(runs.path(), "atomic-waits", "https://atomics.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.eval_main(&format!(r#"window.events=[];window.buffer=new SharedArrayBuffer(16);
window.view=new Int32Array(buffer);window.worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));
worker.onmessage=e=>events.push(e.data);true"#, json!(format!("{worker}\npostMessage('ready');")))).await.unwrap();
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!(["ready"]));
    g.eval_main("events=[]").await.unwrap();
    (runs, g)
}

fn no_worker_cutoff(g: &Generation) {
    for line in std::fs::read_to_string(g.trace_path()).unwrap().lines() {
        let event: BehaviorEvent = serde_json::from_str(line).unwrap();
        if let EventKind::Taint(taint) = event.kind {
            assert_ne!(taint.source, TaintSource::WorkersCutOff, "{line}");
        }
    }
}

#[tokio::test]
async fn synchronous_atomic_timeouts_use_virtual_time_while_the_host_waits() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let (_runs, mut g) = page(r#"onmessage=e=>{
const words=new Int32Array(e.data);Atomics.store(words,1,1);
const start=Date.now();const result=Atomics.wait(words,0,0,50);
postMessage([result,Date.now()-start]);};"#).await;
    g.eval_main("worker.postMessage(buffer)").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.eval_main("Atomics.load(view,1)").await.unwrap(), 1);
    assert_eq!(g.eval_main("events").await.unwrap(), json!([]));
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(g.eval_main("events").await.unwrap(), json!([]), "wall time cannot expire the wait");
    g.advance(VirtualDuration::from_ms(49)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!([]));
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!([["timed-out",50]]));
    no_worker_cutoff(&g);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_frame_can_notify_an_infinite_wait_and_the_worker_resumes_at_that_instant() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let (_runs, mut g) = page(r#"onmessage=e=>{const words=new Int32Array(e.data);
postMessage('waiting');const result=Atomics.wait(words,0,0);
postMessage([result,Date.now()]);};"#).await;
    g.eval_main(r#"worker.onmessage=e=>{if(e.data==='waiting'){
requestAnimationFrame(()=>{events.push(['frame',Date.now()]);Atomics.store(view,0,1);Atomics.notify(view,0);});
}else events.push(e.data);};worker.postMessage(buffer);true"#).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let events = g.eval_main("events").await.unwrap();
    assert_eq!(events.as_array().unwrap().len(), 2, "{events}");
    assert_eq!(events[0][0], "frame");
    assert_eq!(events[1][0], "ok");
    assert_eq!(events[0][1], events[1][1], "the wake holds time before the worker is scheduled");
    no_worker_cutoff(&g);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn posted_messages_do_not_wake_an_atomic_wait_and_termination_releases_it() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let (_runs, mut g) = page(r#"onmessage=e=>{const words=new Int32Array(e.data);
Atomics.store(words,1,1);Atomics.wait(words,0,0);postMessage('unexpected return');};"#).await;
    g.eval_main("worker.postMessage(buffer)").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.eval_main("Atomics.load(view,1)").await.unwrap(), 1);
    g.eval_main("worker.postMessage(buffer)").await.unwrap();
    g.advance(VirtualDuration::from_ms(3_600_000)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!([]));
    g.eval_main("worker.terminate();setTimeout(()=>events.push('after termination'),123)").await.unwrap();
    g.advance(VirtualDuration::from_ms(123)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!(["after termination"]));
    no_worker_cutoff(&g);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_computing_worker_holds_the_frame_until_it_finishes_even_after_two_seconds() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let (_runs, mut g) = page(r#"onmessage=e=>{const words=new Int32Array(e.data);
Atomics.store(words,1,1);while(Atomics.load(words,0)===0){}Atomics.store(words,2,42);};"#).await;
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))
    }).await.unwrap();
    g.eval_main("requestAnimationFrame(()=>events.push(Atomics.load(view,2)));worker.postMessage(buffer)").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while g.eval_main("Atomics.load(view,1)").await.unwrap() != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let params = emulation::RunMainFrameParams::default();
    let frame = client.call(Some(&attached.session_id.0), &params);
    tokio::pin!(frame);
    let early = tokio::time::timeout(Duration::from_millis(3500), &mut frame).await;
    // Always release the worker, including when the old engine returns early.
    g.eval_main("Atomics.store(view,0,1)").await.unwrap();
    let was_early = early.is_ok();
    let answer = match early { Ok(answer) => answer.unwrap(), Err(_) => frame.await.unwrap() };
    assert!(!was_early, "the frame crossed a worker still computing");
    assert_ne!(answer.workers_cut_off, Some(true));
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!([42]));
    no_worker_cutoff(&g);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn bigint_and_wasm_waits_share_the_virtual_timeout_and_immediate_return_rules() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let cases = [
        (r#"const words=new BigInt64Array(e.data);const start=Date.now();
const mismatch=Atomics.wait(words,0,1n,1000);const zero=Atomics.wait(words,0,0n,0);
const result=Atomics.wait(words,0,0n,75);postMessage([mismatch,zero,result,Date.now()-start]);"#,
         "worker.postMessage(buffer)", json!(["not-equal","timed-out","timed-out",75])),
        (r#"const name=s=>[s.length,...Array.from(s,c=>c.charCodeAt(0))];
const section=(id,data)=>[id,data.length,...data];
const body=[0,0x20,0,0x20,1,0x20,2,0xfe,1,2,0,0x0b];
const bytes=new Uint8Array([0,97,115,109,1,0,0,0,
...section(1,[1,0x60,3,0x7f,0x7f,0x7e,1,0x7f]),
...section(2,[1,...name('env'),...name('memory'),2,3,1,1]),
...section(3,[1,0]),...section(7,[1,...name('wait'),0,0]),
...section(10,[1,body.length,...body])]);
const instance=new WebAssembly.Instance(new WebAssembly.Module(bytes),{env:{memory:e.data}});
const wait=instance.exports.wait;const start=Date.now();
const mismatch=wait(0,1,1000000000n);const zero=wait(0,0,0n);
const result=wait(0,0,75000000n);postMessage([mismatch,zero,result,Date.now()-start]);"#,
         "worker.postMessage(new WebAssembly.Memory({initial:1,maximum:1,shared:true}))", json!([1,2,2,75])),
    ];
    for (body, start, expected) in cases {
        let (_runs, mut g) = page(&format!("onmessage=e=>{{{body}}};")).await;
        g.eval_main(start).await.unwrap();
        g.advance(VirtualDuration::from_ms(74)).await.unwrap();
        assert_eq!(g.eval_main("events").await.unwrap(), json!([]));
        g.advance(VirtualDuration::from_ms(1)).await.unwrap();
        assert_eq!(g.eval_main("events").await.unwrap(), json!([expected]));
        no_worker_cutoff(&g);
        g.stop().await.unwrap();
    }
}

async fn check_multiple_waiters(notify_count: u32) {
    let script = r#"onmessage=e=>{
const {buffer,id}=e.data;const words=new Int32Array(buffer);
Atomics.store(words,id,1);const start=Date.now();
const result=Atomics.wait(words,0,0,100);
postMessage({id,result,elapsed:Date.now()-start});};"#;
    let (_runs, mut g) = page(script).await;
    g.eval_main(&format!(r#"window.other=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));
other.onmessage=e=>events.push(e.data);true"#, json!(format!("{script}\npostMessage('ready');")))).await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!(["ready"]));
    g.eval_main("events=[];worker.postMessage({buffer,id:1});other.postMessage({buffer,id:2});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.eval_main("[Atomics.load(view,1),Atomics.load(view,2)]").await.unwrap(), json!([1,1]));
    assert_eq!(g.eval_main("events").await.unwrap(), json!([]));
    let start = g.now();
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(g.eval_main("events").await.unwrap(), json!([]), "neither waiter expires in wall time");
    g.eval_main(&format!("window.notified=null;setTimeout(()=>{{notified=Atomics.notify(view,0,{notify_count})}},50);true")).await.unwrap();
    g.advance(VirtualDuration::from_ms(49)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!([]));
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.eval_main("notified").await.unwrap(), json!(notify_count));
    let notified = g.eval_main("events").await.unwrap();
    assert_eq!(notified.as_array().unwrap().len(), notify_count as usize);
    for event in notified.as_array().unwrap() {
        assert_eq!(event["result"], "ok");
        assert_eq!(event["elapsed"], 50);
    }
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    let completed = g.eval_main("events.slice().sort((a,b)=>a.id-b.id)").await.unwrap();
    let completed = completed.as_array().unwrap();
    assert_eq!(completed.len(), 2);
    assert_eq!(completed[0]["id"], 1);
    assert_eq!(completed[1]["id"], 2);
    for event in completed {
        if notified.as_array().unwrap().iter().any(|wake| wake["id"] == event["id"]) {
            assert_eq!(event["result"], "ok");
            assert_eq!(event["elapsed"], 50);
        } else {
            assert_eq!(event["result"], "timed-out");
            assert_eq!(event["elapsed"], 100);
        }
    }
    assert_eq!(g.now().0, start.0 + 100_000);
    assert_eq!(g.eval_main("Atomics.notify(view,0)").await.unwrap(), 0, "no waiter remains after completion");
    no_worker_cutoff(&g);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn notifying_one_waiter_preserves_the_other_waiters_virtual_deadline() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    check_multiple_waiters(1).await;
}

#[tokio::test]
async fn notifying_all_waiters_resumes_each_at_the_same_virtual_instant() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    check_multiple_waiters(2).await;
}

#[tokio::test]
async fn atomic_deadline_excludes_notifications_at_or_after_expiration() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    for delay in [99, 100, 101] {
        let (_runs, mut g) = page(r#"onmessage=e=>{const words=new Int32Array(e.data);
Atomics.store(words,1,1);const start=Date.now();const result=Atomics.wait(words,0,0,100);
postMessage([result,Date.now()-start]);};"#).await;
        g.eval_main("worker.postMessage(buffer)").await.unwrap();
        g.advance(VirtualDuration::from_ms(0)).await.unwrap();
        assert_eq!(g.eval_main("Atomics.load(view,1)").await.unwrap(), 1);
        g.eval_main(&format!("window.notified=null;setTimeout(()=>{{notified=Atomics.notify(view,0)}},{delay});true")).await.unwrap();
        g.advance(VirtualDuration::from_ms(98)).await.unwrap();
        assert_eq!(g.eval_main("events").await.unwrap(), json!([]));
        g.advance(VirtualDuration::from_ms(3)).await.unwrap();
        let observed = g.eval_main("({events,notified})").await.unwrap();
        let expected = if delay < 100 {
            json!({"events":[["ok",99]],"notified":1})
        } else {
            json!({"events":[["timed-out",100]],"notified":0})
        };
        assert_eq!(observed, expected, "notify at {delay} ms");
        no_worker_cutoff(&g);
        g.stop().await.unwrap();
    }
}

#[tokio::test]
async fn atomic_deadline_does_not_consume_a_limited_notification_for_a_live_waiter() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let script = r#"onmessage=e=>{const {buffer,id,timeout}=e.data;
const words=new Int32Array(buffer);Atomics.store(words,id,1);const start=Date.now();
const result=Atomics.wait(words,0,0,timeout);postMessage({id,result,elapsed:Date.now()-start});};"#;
    let (_runs, mut g) = page(script).await;
    g.eval_main(&format!(r#"window.other=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));
other.onmessage=e=>events.push(e.data);true"#, json!(format!("{script}\npostMessage('ready');")))).await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!(["ready"]));
    // Enroll the earlier deadline first so an expired queue head cannot take
    // the one notification intended for a still-live waiter.
    g.eval_main("events=[];worker.postMessage({buffer,id:1,timeout:100});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.eval_main("Atomics.load(view,1)").await.unwrap(), 1);
    g.eval_main("other.postMessage({buffer,id:2,timeout:200});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.eval_main("Atomics.load(view,2)").await.unwrap(), 1);
    g.eval_main("window.notified=null;setTimeout(()=>{notified=Atomics.notify(view,0,1)},100);true").await.unwrap();
    g.advance(VirtualDuration::from_ms(99)).await.unwrap();
    assert_eq!(g.eval_main("events").await.unwrap(), json!([]));
    g.advance(VirtualDuration::from_ms(101)).await.unwrap();
    assert_eq!(g.eval_main("notified").await.unwrap(), 1);
    assert_eq!(g.eval_main("events.slice().sort((a,b)=>a.id-b.id)").await.unwrap(), json!([
        {"id":1,"result":"timed-out","elapsed":100},
        {"id":2,"result":"ok","elapsed":100}
    ]));
    no_worker_cutoff(&g);
    g.stop().await.unwrap();
}
