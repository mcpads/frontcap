//! A request frontier includes worker startup without waiting for external script decisions.
mod support;
use cdp::protocol::{emulation, inspector, network, runtime, target};
use frontcap_chromium::Generation;
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::{Value, json};
use std::time::Duration;

struct Fixture {
    generation: Generation,
    client: cdp::Client,
    page: String,
    before: String,
    _runs: tempfile::TempDir,
}

impl Fixture {
    async fn launch() -> Self {
        let rules = RuleSet::from_json(&json!([
            {"id":"page","match":{"url":"https://enrollment.example.test/"},"action":{"type":"mock","status":200,
             "headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":"<!doctype html><p>worker frontier</p>"}},
            {"id":"scripts","match":{"url":"https://enrollment.example.test/*.js"},"action":{"type":"hold"}}
        ]).to_string()).unwrap();
        let runs = tempfile::tempdir().unwrap();
        let mut options = support::options(runs.path(), "worker-frontier", "https://enrollment.example.test/".into(), rules);
        options.passthrough_origins = Some(vec![]);
        let mut generation = Generation::launch(options).await.unwrap();
        // Prepare the document before constructing any worker under test.
        generation.advance(VirtualDuration::from_ms(0)).await.unwrap();
        let endpoint = generation.endpoint();
        let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
        let page = client.call(None, &target::AttachToTargetParams { flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id)) }).await.unwrap().session_id.0;
        let before = client.call(Some(&page), &emulation::GetVirtualTimeParams::default()).await.unwrap().unix_time_us;
        let fixture = Self { generation, client, page, before, _runs: runs };
        fixture.eval("window.words=new Int32Array(new SharedArrayBuffer(16));window.done=false;window.failed=false;window.frameRan=false;window.timerRan=false;requestAnimationFrame(()=>frameRan=true);setTimeout(()=>timerRan=true,50);true").await;
        fixture
    }

    async fn eval(&self, expression: &str) -> Value {
        let result = self.client.call(Some(&self.page), &runtime::EvaluateParams { return_by_value: Some(true), ..runtime::EvaluateParams::new(expression.into()) }).await.unwrap();
        assert!(result.exception_details.is_none(), "{result:?}");
        result.result.value.unwrap_or(Value::Null)
    }

    async fn start(&self, script: &str, module: bool) {
        self.start_url(&format!("URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}}))", json!(script)), module).await;
    }

    async fn start_url(&self, url_expression: &str, module: bool) {
        let kind = if module { "module" } else { "classic" };
        self.eval(&format!("window.worker=new Worker({url_expression},{{type:'{kind}'}});worker.onmessage=()=>done=true;worker.onerror=()=>failed=true;worker.postMessage(words.buffer);true")).await;
    }

    async fn frontier(&self) {
        let result = tokio::time::timeout(Duration::from_secs(10), self.client.call(Some(&self.page), &emulation::FrontcapDrainCurrentTasksParams::default())).await.expect("frontier must finish without an external script answer").unwrap();
        assert_eq!(result.unix_time_us, self.before);
    }

    async fn workers(&self) -> Vec<emulation::FrontcapWorker> {
        self.client.call(Some(&self.page), &emulation::FrontcapGetWorkersParams::default())
            .await.unwrap().workers
    }

    async fn snapshot(&self) -> Value {
        let clock = self.client.call(Some(&self.page), &emulation::GetVirtualTimeParams::default()).await.unwrap();
        assert_eq!(clock.unix_time_us, self.before);
        let state = self.eval("[Atomics.load(words,0),done,Atomics.load(words,1),failed,frameRan,timerRan]").await;
        assert_eq!(state[4], false, "no animation frame belongs to collection");
        assert_eq!(state[5], false, "no future timer belongs to collection");
        state
    }

    async fn stop(self) {
        self.eval("if(window.worker)worker.terminate();true").await;
        // A cancelled or failed start must not leave a count behind.
        self.frontier().await;
        self.generation.stop().await.unwrap();
    }
}

const LEAF: &str = "onmessage=e=>{Atomics.store(new Int32Array(e.data),0,1);postMessage('done')};";

#[tokio::test]
async fn new_classic_module_and_nested_workers_finish_before_the_frontier() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let nested = format!("onmessage=e=>{{const child=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));child.onmessage=()=>postMessage('done');child.postMessage(e.data)}};", json!(LEAF));
    for (kind, script, module) in [("classic", LEAF, false), ("module", LEAF, true), ("nested", nested.as_str(), false)] {
        let f = Fixture::launch().await;
        f.start(script, module).await;
        f.frontier().await;
        let state = f.snapshot().await;
        f.stop().await;
        assert_eq!(state, json!([1,true,0,false,false,false]), "{kind}: a newly constructed worker was omitted");
    }
}

#[tokio::test]
async fn new_data_workers_finish_before_the_frontier() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    for module in [false, true] {
        let f = Fixture::launch().await;
        f.start_url(&format!("'data:text/javascript,'+encodeURIComponent({})", json!(LEAF)), module).await;
        f.frontier().await;
        let state = f.snapshot().await;
        f.stop().await;
        assert_eq!(state, json!([1,true,0,false,false,false]), "data worker: module={module}");
    }
}

#[tokio::test]
async fn a_network_worker_host_is_known_without_answering_its_main_script() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let f = Fixture::launch().await;
    f.eval("window.worker=new Worker('/held.js');worker.onmessage=()=>done=true;worker.postMessage(words.buffer);true").await;
    f.frontier().await;
    let hosts = f.client.call(None, &target::GetTargetsParams::default()).await.unwrap().target_infos.into_iter().filter(|info| info.r#type == "worker").count();
    let state = f.snapshot().await;
    f.stop().await;
    assert_eq!(hosts, 1, "the constructor must have reached the browser");
    assert_eq!(state, json!([0,false,0,false,false,false]));
}

#[tokio::test]
async fn waiting_workers_release_the_frontier_and_their_children_rejoin_on_wake() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    for (atomic, nested) in [(false, false), (false, true), (true, false), (true, true)] {
        let f = Fixture::launch().await;
        let wait = if atomic { "Atomics.wait(words,2,0,50)" } else { "importScripts('https://enrollment.example.test/held.js')" };
        let child = if nested { format!("const child=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));child.postMessage(e.data);", json!(LEAF)) } else { String::new() };
        let script = format!("onmessage=e=>{{const words=new Int32Array(e.data);{child}Atomics.store(words,1,1);{wait};postMessage('done')}};");
        f.start(&script, false).await;
        f.frontier().await;
        let waiting = f.snapshot().await;
        let resumed = if atomic && waiting[2] == 1 {
            f.eval("Atomics.store(words,2,1);Atomics.notify(words,2);true").await;
            f.frontier().await;
            Some(f.snapshot().await)
        } else { None };
        f.stop().await;
        assert_eq!(waiting[2], 1, "the parent must actually reach its wait");
        assert_eq!(waiting[1], false, "the external decision remains pending");
        if atomic { assert_eq!(resumed.unwrap(), json!([u8::from(nested),true,1,false,false,false]), "the child must rejoin after the parent wakes"); }
    }
}

#[tokio::test]
async fn a_missing_setup_reply_keeps_the_frontier_open_until_cancellation() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let f = Fixture::launch().await;
    f.generation.omit_worker_network_enable_answer();
    f.start(LEAF, false).await;
    let (was_early, result) = {
        let params = emulation::FrontcapDrainCurrentTasksParams::default();
        let drain = f.client.call(Some(&f.page), &params);
        tokio::pin!(drain);
        let early = tokio::time::timeout(Duration::from_millis(150), &mut drain).await;
        let was_early = early.is_ok();
        let overlapping = f.client.call(Some(&f.page), &params);
        tokio::pin!(overlapping);
        let overlap_early = tokio::time::timeout(Duration::from_millis(150), &mut overlapping).await;
        f.eval("worker.terminate();true").await;
        let result = match early { Ok(result) => result.unwrap(), Err(_) => tokio::time::timeout(Duration::from_secs(10), &mut drain).await.expect("cancellation retires enrollment").unwrap() };
        let overlap_result = match overlap_early {
            Ok(result) => { result.unwrap(); panic!("an overlapping frontier certified missing setup"); },
            Err(_) => tokio::time::timeout(Duration::from_secs(10), &mut overlapping).await.expect("cancellation retires overlapping enrollment").unwrap(),
        };
        assert_eq!(overlap_result.unix_time_us, f.before);
        (was_early, result)
    };
    assert_eq!(result.unix_time_us, f.before);
    let state = f.snapshot().await;
    f.stop().await;
    assert_eq!(state[0], 0, "the unconfigured worker never ran");
    assert!(!was_early, "the frontier certified a worker still awaiting setup");
}

#[tokio::test]
async fn a_frontier_closes_service_admission_before_replying() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let f = Fixture::launch().await;
    f.frontier().await;
    f.eval("window.fileDone=false;window.reader=new FileReader();reader.onload=()=>fileDone=true;reader.readAsText(new Blob(['frontier']));true").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(f.eval("fileDone").await, false, "a completed frontier must leave service callbacks stopped");
    f.frontier().await;
    assert_eq!(f.eval("[fileDone,reader.result]").await, json!([true,"frontier"]));
    f.snapshot().await;
    f.stop().await;
}

#[tokio::test]
async fn immediate_cancellation_and_creator_removal_retire_pending_enrollment() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    for removed in [false, true] {
        let f = Fixture::launch().await;
        let source = json!(LEAF);
        let expression = if removed {
            format!("const frame=document.createElement('iframe');document.body.append(frame);const url=URL.createObjectURL(new Blob([{source}],{{type:'text/javascript'}}));window.worker=frame.contentWindow.eval('new Worker('+JSON.stringify(url)+')');worker.postMessage(words.buffer);frame.remove();true")
        } else {
            format!("window.worker=new Worker(URL.createObjectURL(new Blob([{source}],{{type:'text/javascript'}})));worker.postMessage(words.buffer);worker.terminate();true")
        };
        f.eval(&expression).await;
        f.frontier().await;
        assert!(f.workers().await.is_empty(), "cancelled and removed creators leave no live owners");
        let state = f.snapshot().await;
        f.stop().await;
        assert_eq!(state, json!([0,false,0,false,false,false]));
    }
}

#[tokio::test]
async fn a_failed_local_script_retires_its_start_before_the_frontier() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let f = Fixture::launch().await;
    f.start("onmessage = (", false).await;
    f.frontier().await;
    let state = f.snapshot().await;
    f.stop().await;
    assert_eq!(state, json!([0,false,0,true,false,false]));
}

#[tokio::test]
async fn detaching_a_frontier_closes_its_service_admission() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let mut f=Fixture::launch().await;
    f.generation.omit_worker_network_enable_answer();
    f.start("onmessage=e=>postMessage('done')",false).await;
    let was_pending={
        let params=emulation::FrontcapDrainCurrentTasksParams::default();
        let drain=f.client.call(Some(&f.page),&params);
        tokio::pin!(drain);
        let early=tokio::time::timeout(Duration::from_millis(150),&mut drain).await;
        let pending=early.is_err();
        f.client.call(None,&target::DetachFromTargetParams{session_id:Some(target::SessionID(f.page.clone())),target_id:None}).await.unwrap();
        match early {
            Ok(result)=>{ result.unwrap(); },
            Err(_)=>{
                let detached=tokio::time::timeout(Duration::from_secs(10),&mut drain).await;
                assert!(matches!(detached, Ok(Err(_))), "a detached frontier must end with an explicit error: {detached:?}");
            }
        }
        pending
    };
    f.page=f.client.call(None,&target::AttachToTargetParams{flatten:Some(true),..target::AttachToTargetParams::new(target::TargetID(f.generation.endpoint().target_id))}).await.unwrap().session_id.0;
    f.eval("window.fileDone=false;window.reader=new FileReader();reader.onload=()=>fileDone=true;reader.readAsText(new Blob(['detached']));true").await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    let stopped=f.eval("fileDone").await;
    // Always release the worker before asserting, including when the gate leaked.
    f.eval("worker.terminate();true").await;
    f.frontier().await;
    let after=f.eval("[fileDone,reader.result]").await;
    f.snapshot().await;
    f.stop().await;
    assert!(was_pending,"the detached command must still own an admission");
    assert_eq!(stopped,false,"detachment must close service admission before another progress command");
    assert_eq!(after,json!([true,"detached"]));
}

#[tokio::test]
async fn live_worker_ownership_excludes_cancelled_hosts_and_retires_descendants() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let f = Fixture::launch().await;
    f.eval("window.loading=new Worker('/held.js');true").await;
    f.frontier().await;
    let loading = f.workers().await;
    assert_eq!(loading.len(), 1);
    assert!(!loading[0].thread_started, "a held main script has no worker thread");
    assert!(!loading[0].creator_is_worker);
    assert_eq!(loading[0].sequence, "1");
    let target = loading[0].target_id.clone();
    f.eval("loading.terminate();true").await;
    f.frontier().await;
    assert!(f.workers().await.is_empty(), "explicit cancellation retires the native owner");
    assert!(f.client.call(None, &target::GetTargetsParams::default()).await.unwrap().target_infos
        .iter().any(|info| info.target_id.0 == target), "the retained JS object still owns a browser host");

    f.start("onmessage=e=>{globalThis.child=new Worker('https://enrollment.example.test/held.js');Atomics.store(new Int32Array(e.data),1,1)}", false).await;
    f.frontier().await;
    let workers = f.workers().await;
    assert_eq!(workers.len(), 2);
    let parent = workers.iter().find(|w| !w.creator_is_worker).unwrap();
    let child = workers.iter().find(|w| w.creator_is_worker).unwrap();
    assert_eq!(parent.sequence, "2", "retirement does not recycle constructor order");
    assert!(parent.thread_started);
    assert_eq!(child.creator_id, parent.target_id);
    assert_eq!(child.sequence, "1");
    assert!(!child.thread_started);
    // Lifecycle belongs to the engine, not to this diagnostic connection.
    let endpoint = f.generation.endpoint();
    let replacement = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let session = replacement.call(None, &target::AttachToTargetParams { flatten: Some(true),
        ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id)) }).await.unwrap().session_id;
    let recovered = replacement.call(Some(&session.0), &emulation::FrontcapGetWorkersParams::default()).await.unwrap().workers;
    let ids = |workers: &[emulation::FrontcapWorker]| workers.iter().map(|w| w.target_id.clone()).collect::<std::collections::BTreeSet<_>>();
    assert_eq!(ids(&recovered), ids(&workers));
    replacement.call(None, &target::DetachFromTargetParams { session_id: Some(session), target_id: None }).await.unwrap();
    f.eval("worker.terminate();true").await;
    f.frontier().await;
    assert!(f.workers().await.is_empty(), "parent retirement includes children still loading");
    f.snapshot().await;
    f.stop().await;
}

#[tokio::test]
async fn synchronous_workers_publish_ownership_and_a_notification_boundary_before_waiting() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let f = Fixture::launch().await;
    let script = "onmessage=e=>{globalThis.child=new Worker('https://enrollment.example.test/child.js');fetch('https://enrollment.example.test/before-a');fetch('https://enrollment.example.test/before-b');Atomics.store(new Int32Array(e.data),1,1);importScripts('https://enrollment.example.test/held.js');fetch('https://enrollment.example.test/after');postMessage('done')}";
    f.eval(&format!("window.worker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));worker.onmessage=()=>done=true;true", json!(script))).await;
    f.frontier().await;
    let owner = f.workers().await.pop().unwrap();
    let session = f.client.call(None, &target::AttachToTargetParams { flatten: Some(true),
        ..target::AttachToTargetParams::new(target::TargetID(owner.target_id.clone())) }).await.unwrap().session_id;
    let mut sent = f.client.subscribe::<network::RequestWillBeSentEvent>(Some(&session.0));
    let mut waits = f.client.subscribe::<inspector::FrontcapSynchronousWaitEvent>(Some(&session.0));
    f.client.call(Some(&session.0), &network::EnableParams::default()).await.unwrap();
    f.eval("worker.postMessage(words.buffer);true").await;
    f.frontier().await;
    let workers = f.workers().await;
    let waiting = workers.iter().find(|w| w.target_id == owner.target_id).unwrap();
    let request = waiting.synchronous_wait_id.as_ref().expect("native wait is observable from the page");
    let (boundary, event) = waits.next_timeout_stamped(Duration::from_secs(10)).await.unwrap().expect("worker notifications flushed before blocking");
    assert_eq!(&event.request_id, request);
    let mut before = Vec::new();
    while let Some((arrival, event)) = sent.next_timeout_stamped(Duration::ZERO).await.unwrap() {
        assert!(!event.request.url.ends_with("/after"), "the script answer is still held");
        if event.request.url.contains("/before-") {
            assert!(arrival < boundary, "earlier renderer request notifications precede the wait marker");
            before.push(event.request.url);
        }
    }
    assert_eq!(before, ["https://enrollment.example.test/before-a", "https://enrollment.example.test/before-b"]);
    assert!(workers.iter().any(|w| w.creator_is_worker && w.creator_id == owner.target_id),
        "the blocked creator's child is visible without querying that creator");
    let state = f.snapshot().await;
    assert_eq!(state[2], 1);
    assert_eq!(state[1], false);
    f.eval("worker.terminate();true").await;
    f.frontier().await;
    assert!(f.workers().await.is_empty());
    f.stop().await;
}

#[tokio::test]
async fn failed_script_loads_and_self_closed_workers_leave_no_live_owner() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let mut f = Fixture::launch().await;
    f.eval("window.worker=new Worker('/held.js');worker.onerror=()=>failed=true;true").await;
    f.generation.advance(frontcap_core::time::VirtualDuration::from_ms(0)).await.unwrap();
    let held = f.generation.held();
    assert_eq!(held.len(), 1);
    let missing = serde_json::from_value(json!({"status":404,"headers":{"content-type":"text/javascript"},"body":""})).unwrap();
    f.generation.respond(&held[0], missing).await.unwrap();
    f.frontier().await;
    assert_eq!(f.eval("failed").await, true, "the loader failure reached the creator");
    assert!(f.workers().await.is_empty(), "a failed main-script load is terminal");
    f.start("close()", false).await;
    f.frontier().await;
    assert!(f.workers().await.is_empty(), "worker self-close retires its native scope");
    f.stop().await;
}

#[tokio::test]
async fn zero_progress_waits_for_a_released_worker_script_to_enroll() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let mut f = Fixture::launch().await;
    f.generation.omit_worker_network_enable_answer();
    f.start_url("'/held.js'", false).await;
    f.generation.advance(VirtualDuration::from_ms(0)).await.unwrap();
    let held = f.generation.held();
    assert_eq!(held.len(), 1);
    let script = serde_json::from_value(json!({"status":200,"headers":{"content-type":"text/javascript","cross-origin-embedder-policy":"require-corp"},"body":LEAF})).unwrap();
    f.generation.respond(&held[0], script).await.unwrap();
    let client = f.client.clone();
    let page = f.page.clone();
    let (was_early, started) = {
        let progress = f.generation.advance(VirtualDuration::from_ms(0));
        tokio::pin!(progress);
        let early = tokio::time::timeout(Duration::from_millis(150), &mut progress).await;
        let was_early = early.is_ok();
        let started = client.call(Some(&page), &emulation::FrontcapGetWorkersParams::default()).await.unwrap().workers.iter().any(|w| w.thread_started);
        // Always retire the held enrollment, even when the boundary was early.
        client.call(Some(&page), &runtime::EvaluateParams::new("worker.terminate();true".into())).await.unwrap();
        match early {
            Ok(result) => { result.unwrap(); }
            Err(_) => { tokio::time::timeout(Duration::from_secs(10), &mut progress).await.unwrap().unwrap(); }
        }
        (was_early, started)
    };
    let state = f.snapshot().await;
    f.stop().await;
    assert!(started, "the worker must reach thread startup before cancellation");
    assert_eq!(state[3], false, "a failed script is not a held enrollment");
    assert!(!was_early, "zero progress returned before the released worker completed enrollment");
    assert_eq!(state[0], 0, "an unconfigured worker must never run");
}
