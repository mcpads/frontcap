//! A request frontier finishes runnable work without consuming pending responses or a frame.
mod support;

use cdp::protocol::{emulation, runtime, target};
use frontcap_chromium::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::json;
use std::time::Duration;

fn has_bound_request(trace: &std::path::Path, suffix: &str) -> bool {
    use frontcap_chromium::bindings::{BindingEntry, BindingJournal};
    BindingJournal::read(&trace.parent().unwrap().join("interceptions.jsonl")).unwrap().iter().any(|entry|
        matches!(entry, BindingEntry::Bound(binding) if binding.url.ends_with(suffix)))
}

#[tokio::test]
async fn ordinary_selection_collects_later_fetch_pauses_including_preflight_dependents() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    for preflight in [false, true] {
        let rules = RuleSet::from_json(&json!([
            {"id":"page","match":{"url":"https://fetch-frontier.example.test/"},"action":{"type":"mock","status":200,
             "headers":{"content-type":"text/html"},"body":"<!doctype html><p>Fetch frontier</p>"}},
            {"id":"requests","match":{"url":"https://*.example.test/*"},"action":{"type":"hold"}}
        ]).to_string()).unwrap();
        let runs = tempfile::tempdir().unwrap();
        let mut options = support::options(runs.path(), "fetch-frontier", "https://fetch-frontier.example.test/".into(), rules);
        options.passthrough_origins = Some(vec![]);
        let mut g = Generation::launch(options).await.unwrap();
        g.advance(VirtualDuration::from_ms(0)).await.unwrap();
        g.delay_interceptions("/slow", Duration::from_millis(1000));
        if preflight {
            // The browser's preflight notification can precede its renderer
            // request's notification. Its issue time must still come from that request.
            g.delay_request_announcements("\"method\":\"POST\"", Duration::from_millis(500));
        }
        let slow = if preflight {
            "fetch('https://cross-origin.example.test/slow',{method:'POST',headers:{'content-type':'application/json'},body:'{}'})"
        } else { "fetch('/slow')" };
        g.eval_main(&format!("setTimeout(()=>{{fetch('/fast');{slow};}},10);true")).await.unwrap();
        let trace = g.trace_path();
        let premature;
        {
            let progress = g.advance(VirtualDuration::from_ms(20));
            tokio::pin!(progress);
            let early = tokio::time::timeout(Duration::from_millis(150), &mut progress).await;
            premature = has_bound_request(&trace, "/fast");
            match early {
                Ok(result) => { result.unwrap(); }
                Err(_) => { tokio::time::timeout(Duration::from_secs(10), &mut progress).await.unwrap().unwrap(); }
            }
        }
        let held = g.held().len();
        assert_eq!(g.now(), frontcap_core::time::VirtualTime(20_000));
        let bindings = frontcap_chromium::bindings::BindingJournal::read(&trace.parent().unwrap().join("interceptions.jsonl")).unwrap();
        for binding in bindings.iter().filter_map(|entry| match entry {
            frontcap_chromium::bindings::BindingEntry::Bound(binding) if binding.url.ends_with("/fast") || binding.url.ends_with("/slow") => Some(binding),
            _ => None,
        }) {
            assert_eq!(binding.issued_us, 10_000, "{} {} must keep the timer issue instant", binding.method, binding.url);
        }
        g.stop().await.unwrap();
        assert!(!premature, "fast request selected before the slow Fetch pause; preflight={preflight}");
        assert_eq!(held, 2, "preflights release their dependent request without taking an application hold");
    }
}

#[tokio::test]
async fn a_cancelled_request_does_not_hold_fetch_collection_open() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let rules = RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"https://cancel-frontier.example.test/"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><p>cancel frontier</p>"}},
        {"id":"requests","match":{"url":"https://cancel-frontier.example.test/*"},"action":{"type":"hold"}}
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "cancel-frontier", "https://cancel-frontier.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    g.delay_interceptions("/cancelled", Duration::from_millis(1000));
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let page = client.call(None, &target::AttachToTargetParams { flatten:Some(true),
        ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id)) }).await.unwrap().session_id;
    let mut sent = client.subscribe::<cdp::protocol::network::RequestWillBeSentEvent>(Some(&page.0));
    client.call(Some(&page.0), &cdp::protocol::network::EnableParams::default()).await.unwrap();
    g.eval_main("window.abort=new AbortController();fetch('/cancelled',{signal:abort.signal}).catch(()=>{});true").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = sent.next_timeout(Duration::from_secs(10)).await.unwrap().unwrap();
            if event.request.url.ends_with("/cancelled") { break; }
        }
    }).await.unwrap();
    g.eval_main("abort.abort();fetch('/kept');true").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), g.advance(VirtualDuration::from_ms(0))).await.unwrap().unwrap();
    assert_eq!(g.held().len(), 1);
    assert_eq!(g.now(), frontcap_core::time::VirtualTime::ZERO);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn ordinary_selection_waits_for_live_worker_script_announcements() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let rules = RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"https://script-frontier.example.test/"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><p>script frontier</p>"}},
        {"id":"requests","match":{"url":"https://script-frontier.example.test/*"},"action":{"type":"hold"}}
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "script-frontier", "https://script-frontier.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let page = client.call(None, &target::AttachToTargetParams { flatten:Some(true),
        ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id)) }).await.unwrap().session_id;
    // The ordinary request is selectable while both channels for the live
    // worker's main script are still delayed. Neither may be omitted merely
    // because the renderer task boundary has already completed.
    g.delay_interceptions("/child.js", Duration::from_millis(1000));
    g.delay_request_announcements("/child.js", Duration::from_millis(1500));
    client.call(Some(&page.0), &runtime::EvaluateParams::new("fetch('/ordinary');window.worker=new Worker('/child.js');true".into())).await.unwrap();
    client.call(Some(&page.0), &emulation::FrontcapDrainCurrentTasksParams::default()).await.unwrap();
    let workers = client.call(Some(&page.0), &emulation::FrontcapGetWorkersParams::default()).await.unwrap().workers;
    assert_eq!(workers.len(), 1);
    assert!(!workers[0].thread_started);
    let trace = g.trace_path();
    {
        let progress = g.advance(VirtualDuration::from_ms(0));
        tokio::pin!(progress);
        let early = tokio::time::timeout(Duration::from_millis(150), &mut progress).await;
        let was_pending = early.is_err();
        let prematurely_selected = has_bound_request(&trace, "/ordinary");
        // Finish the same operation even when the assertion will fail, so a
        // regression does not abandon an in-flight progress call.
        match early {
            Ok(result) => { result.unwrap(); }
            Err(_) => { tokio::time::timeout(Duration::from_secs(10), &mut progress).await.unwrap().unwrap(); }
        }
        assert!(was_pending, "collection completed before the live worker's delayed announcement");
        assert!(!prematurely_selected, "ordinary selection preceded a live worker's main-script announcement");
    }
    assert_eq!(g.held().len(), 2);
    assert_eq!(g.now(), frontcap_core::time::VirtualTime::ZERO);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn a_request_frontier_drains_workers_and_messages_without_answers_frames_or_future_time() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let rules = RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"https://frontier.example.test/"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":"<!doctype html><p>request frontier</p>"}},
        {"id":"answer","match":{"url":"https://frontier.example.test/request"},"action":{"type":"mock","status":200,"body":"ok"}}
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "request-frontier", "https://frontier.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    let worker = r#"onmessage=e=>{const words=new Int32Array(e.data);Atomics.store(words,1,1);
while(Atomics.load(words,0)===0){};
fetch('https://frontier.example.test/request').then(r=>r.text()).then(()=>Atomics.store(words,3,1));
Atomics.store(words,2,42);postMessage('done');};postMessage('ready');"#;
    g.eval_main(&format!("window.buffer=new SharedArrayBuffer(16);window.words=new Int32Array(buffer);window.ready=false;window.worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));worker.onmessage=()=>ready=true;true",json!(worker))).await.unwrap();
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    assert_eq!(g.eval_main("ready").await.unwrap(),true);
    let before=g.now();
    let endpoint=g.endpoint();
    let client=cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let page=client.call(None,&target::AttachToTargetParams {flatten:Some(true),..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id))}).await.unwrap().session_id;
    let mut console=client.subscribe::<runtime::ConsoleApiCalledEvent>(Some(&page.0));
    client.call(Some(&page.0),&runtime::EnableParams::default()).await.unwrap();
    let frozen=client.call(Some(&page.0),&emulation::GetVirtualTimeParams::default()).await.unwrap();
    g.eval_main("window.frameRan=false;window.timerRan=false;requestAnimationFrame(()=>frameRan=true);setTimeout(()=>timerRan=true,50);worker.onmessage=e=>console.log('frontier-'+e.data);worker.postMessage(buffer);true").await.unwrap();
    tokio::time::timeout(Duration::from_secs(10),async {
        while g.eval_main("Atomics.load(words,1)").await.unwrap()!=1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    let params=emulation::FrontcapDrainCurrentTasksParams::default();
    let drain=client.call(Some(&page.0),&params);
    tokio::pin!(drain);
    let early=tokio::time::timeout(Duration::from_millis(150),&mut drain).await;
    // Release even on a regression so a failure leaves no computing worker behind.
    g.eval_main("Atomics.store(words,0,1)").await.unwrap();
    let was_early=early.is_ok();
    let answer=match early {Ok(answer)=>answer.unwrap(),Err(_)=>tokio::time::timeout(Duration::from_secs(10),&mut drain).await.unwrap().unwrap()};
    assert!(!was_early,"a runnable worker was omitted from the frontier");
    let mut delivered=false;
    while let Some(event)=console.try_next().unwrap() {
        delivered |= event.args.iter().any(|arg| arg.value.as_ref()==Some(&json!("frontier-done")));
    }
    assert!(delivered,"the worker's message must reach the page before the frontier reply");
    let clock=client.call(Some(&page.0),&emulation::GetVirtualTimeParams::default()).await.unwrap();
    assert_eq!(answer.unix_time_us,frozen.unix_time_us);
    assert_eq!(answer.unix_time_us,clock.unix_time_us);
    assert_eq!(g.now(),before);
    assert_eq!(g.eval_main("[Atomics.load(words,2),Atomics.load(words,3),frameRan,timerRan]").await.unwrap(),json!([42,0,false,false]));
    // The command finished despite the unanswered load. Ordinary progress can now answer it.
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.eval_main("Atomics.load(words,3)").await.unwrap(),1);
    g.advance(VirtualDuration::from_ms(50)).await.unwrap();
    assert_eq!(g.eval_main("timerRan").await.unwrap(),true);
    g.stop().await.unwrap();
}
