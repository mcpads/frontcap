//! Ordinary generation progress with internal shared-time activation; no manual grants.
mod support;
use cdp::protocol::{emulation, runtime, target};
use frontcap_chromium::generation::Generation;
use frontcap_core::{
    ids::RequestId, predicate::Locator, profile::PopupPolicy, rules::RuleSet, time::VirtualDuration,
};
use serde_json::{Value, json};
use std::time::Duration;

async fn attach(client: &cdp::Client, id: target::TargetID) -> String {
    client
        .call(
            None,
            &target::AttachToTargetParams {
                flatten: Some(true),
                ..target::AttachToTargetParams::new(id)
            },
        )
        .await
        .unwrap()
        .session_id
        .0
}
async fn eval(client: &cdp::Client, session: &str, expression: String) -> Value {
    let result = client
        .call(
            Some(session),
            &runtime::EvaluateParams {
                return_by_value: Some(true),
                ..runtime::EvaluateParams::new(expression)
            },
        )
        .await
        .unwrap();
    assert!(
        result.exception_details.is_none(),
        "{:?}",
        result.exception_details
    );
    result.result.value.unwrap_or(Value::Null)
}
#[derive(Clone, Copy, PartialEq)]
enum Worker {
    None,
    AtomicWait,
    DynamicClassic,
    DynamicModule,
    DynamicNested,
    ClassicDuringWait,
    ModuleDuringWait,
    NestedDuringWait,
    ClassicPendingRetirement,
    SyncPendingRetirement,
    ModuleSyncRetirement,
    NestedSyncRetirement,
    AtomicPendingRetirement,
    IndefiniteAtomicRetirement,
    ModuleAtomicRetirement,
    ModuleIndefiniteAtomicRetirement,
    NestedAtomicRetirement,
    NestedIndefiniteAtomicRetirement,
    ModulePendingRetirement,
    NestedPendingRetirement,
    AtomicWithPrefix,
    AtomicNotifiedWithPrefix,
    FetchDuringWait,
    NestedFromResident,
    SyncDuringWait,
    SyncWithPrefix,
}

impl Worker {
    fn retirement_atomic_timeout(self) -> Option<&'static str> {
        match self {
            Self::AtomicPendingRetirement | Self::ModuleAtomicRetirement | Self::NestedAtomicRetirement => Some("1000"),
            Self::IndefiniteAtomicRetirement | Self::ModuleIndefiniteAtomicRetirement | Self::NestedIndefiniteAtomicRetirement => Some("Infinity"),
            _ => None,
        }
    }

    fn starts_during_wait(self) -> bool {
        matches!(self, Self::ClassicDuringWait | Self::ModuleDuringWait | Self::NestedDuringWait
            | Self::ClassicPendingRetirement | Self::ModulePendingRetirement | Self::NestedPendingRetirement | Self::SyncPendingRetirement | Self::ModuleSyncRetirement | Self::NestedSyncRetirement) || self.retirement_atomic_timeout().is_some()
    }

    fn retires_with_pending_response(self) -> bool {
        matches!(self, Self::ClassicPendingRetirement | Self::ModulePendingRetirement | Self::NestedPendingRetirement | Self::SyncPendingRetirement | Self::ModuleSyncRetirement | Self::NestedSyncRetirement) || self.retirement_atomic_timeout().is_some()
    }

    fn dynamic(self) -> bool {
        matches!(
            self,
            Self::DynamicClassic | Self::DynamicModule | Self::DynamicNested
                | Self::ClassicDuringWait | Self::ModuleDuringWait | Self::NestedDuringWait
                | Self::ClassicPendingRetirement | Self::ModulePendingRetirement | Self::NestedPendingRetirement | Self::SyncPendingRetirement | Self::ModuleSyncRetirement | Self::NestedSyncRetirement
        ) || self.retirement_atomic_timeout().is_some()
    }
}

#[derive(Clone, Copy)]
struct Overlap {
    latency_ms: u64,
    requests: u64,
    notification_delay: Duration,
}
impl Overlap {
    fn single(latency_ms: u64) -> Self {
        Self {
            latency_ms,
            requests: 1,
            notification_delay: Duration::ZERO,
        }
    }
}

#[tokio::test]
async fn generation_progress_coordinates_independent_renderer_callbacks() {
    check(Worker::None, false, false, false, None).await;
}
#[tokio::test]
async fn generation_progress_includes_worker_atomic_wait_and_delivery() {
    check(Worker::AtomicWait, false, false, false, None).await;
}
#[tokio::test]
async fn atomic_wait_timeout_preserves_an_earlier_response_during_shared_arrivals() {
    check(
        Worker::AtomicWithPrefix,
        true,
        true,
        false,
        Some(Overlap::single(200)),
    )
    .await;
}
#[tokio::test]
async fn atomic_wait_timeout_preserves_responses_with_a_late_request_notification() {
    check(
        Worker::AtomicWithPrefix,
        true,
        true,
        false,
        Some(Overlap {
            notification_delay: Duration::from_millis(100),
            ..Overlap::single(200)
        }),
    )
    .await;
}
#[tokio::test]
async fn atomic_wait_notification_releases_an_earlier_worker_response() {
    check(Worker::AtomicNotifiedWithPrefix, false, false, false, None).await;
}
#[tokio::test]
async fn shared_progress_admits_and_retires_a_new_classic_worker() {
    check(
        Worker::DynamicClassic,
        true,
        true,
        false,
        Some(Overlap::single(200)),
    )
    .await;
}
#[tokio::test]
async fn shared_progress_admits_and_retires_a_new_module_worker() {
    check(
        Worker::DynamicModule,
        true,
        true,
        false,
        Some(Overlap::single(200)),
    )
    .await;
}
#[tokio::test]
async fn shared_progress_admits_and_retires_new_nested_workers() {
    check(
        Worker::DynamicNested,
        true,
        true,
        false,
        Some(Overlap::single(200)),
    )
    .await;
}
#[tokio::test]
async fn a_classic_worker_starts_and_retires_during_another_renderers_wait() {
    check(Worker::ClassicDuringWait, true, false, false, None).await;
}
#[tokio::test]
async fn a_module_worker_starts_and_retires_during_another_renderers_wait() {
    check(Worker::ModuleDuringWait, true, false, false, None).await;
}
#[tokio::test]
async fn nested_workers_start_and_retire_during_another_renderers_wait() {
    check(Worker::NestedDuringWait, true, false, false, None).await;
}
#[tokio::test]
async fn module_worker_retires_during_its_own_synchronous_request() {
    check(Worker::ModuleSyncRetirement, true, false, false, None).await;
}

#[tokio::test]
async fn nested_worker_retires_during_its_own_synchronous_request() {
    check(Worker::NestedSyncRetirement, true, false, false, None).await;
}

#[tokio::test]
async fn worker_retires_during_its_own_finite_atomic_wait() {
    check(Worker::AtomicPendingRetirement, true, false, false, None).await;
}

#[tokio::test]
async fn worker_retires_during_its_own_indefinite_atomic_wait() {
    check(Worker::IndefiniteAtomicRetirement, true, false, false, None).await;
}

#[tokio::test]
async fn module_worker_retires_during_its_own_finite_atomic_wait() {
    check(Worker::ModuleAtomicRetirement, true, false, false, None).await;
}

#[tokio::test]
async fn module_worker_retires_during_its_own_indefinite_atomic_wait() {
    check(Worker::ModuleIndefiniteAtomicRetirement, true, false, false, None).await;
}

#[tokio::test]
async fn nested_worker_retires_during_its_own_finite_atomic_wait() {
    check(Worker::NestedAtomicRetirement, true, false, false, None).await;
}

#[tokio::test]
async fn nested_worker_retires_during_its_own_indefinite_atomic_wait() {
    check(Worker::NestedIndefiniteAtomicRetirement, true, false, false, None).await;
}

#[tokio::test]
async fn worker_retires_during_its_own_synchronous_request() {
    check(Worker::SyncPendingRetirement, true, false, false, None).await;
}

#[tokio::test]
async fn classic_worker_retires_with_a_pending_response_during_another_renderers_wait() {
    check(Worker::ClassicPendingRetirement, true, false, false, None).await;
}
#[tokio::test]
async fn module_worker_retires_with_a_pending_response_during_another_renderers_wait() {
    check(Worker::ModulePendingRetirement, true, false, false, None).await;
}
#[tokio::test]
async fn nested_worker_retires_with_a_pending_response_during_another_renderers_wait() {
    check(Worker::NestedPendingRetirement, true, false, false, None).await;
}
#[tokio::test]
async fn generation_progress_services_synchronous_arrival_with_live_popup() {
    check(Worker::None, true, false, false, None).await;
}
#[tokio::test]
async fn ordinary_popup_response_arrives_before_the_blocked_owners_response() {
    check(Worker::None, true, true, false, None).await;
}
#[tokio::test]
async fn synchronous_arrival_preserves_the_remaining_original_segment() {
    check(Worker::None, true, false, true, None).await;
}
#[tokio::test]
async fn overlapping_synchronous_requests_finish_in_reverse_issue_order() {
    check(Worker::None, true, false, false, Some(Overlap::single(200))).await;
}
#[tokio::test]
async fn overlapping_synchronous_requests_finish_in_issue_order() {
    check(
        Worker::None,
        true,
        false,
        false,
        Some(Overlap::single(1000)),
    )
    .await;
}
#[tokio::test]
async fn overlapping_synchronous_requests_with_equal_deadlines_keep_issue_order() {
    check(Worker::None, true, false, false, Some(Overlap::single(700))).await;
}
#[tokio::test]
async fn a_renderer_can_start_its_next_synchronous_request_while_another_still_waits() {
    check(
        Worker::None,
        true,
        false,
        false,
        Some(Overlap {
            requests: 2,
            ..Overlap::single(200)
        }),
    )
    .await;
}
#[tokio::test]
async fn repeated_synchronous_requests_preserve_the_bound_with_late_notifications() {
    check(
        Worker::None,
        true,
        false,
        false,
        Some(Overlap {
            latency_ms: 200,
            requests: 5,
            notification_delay: Duration::from_millis(100),
        }),
    )
    .await;
}
#[tokio::test]
async fn a_late_synchronous_interception_does_not_block_renderer_pause_service() {
    check(
        Worker::None,
        true,
        false,
        false,
        Some(Overlap {
            notification_delay: Duration::from_millis(100),
            ..Overlap::single(200)
        }),
    )
    .await;
}

#[tokio::test]
async fn ordinary_prefix_responses_arrive_while_both_renderers_wait_synchronously() {
    check(Worker::None, true, true, false, Some(Overlap::single(200))).await;
}

#[tokio::test]
async fn ordinary_prefix_responses_with_a_resident_worker_keep_their_deadlines() {
    check(
        Worker::AtomicWait,
        true,
        true,
        false,
        Some(Overlap::single(200)),
    )
    .await;
}

#[tokio::test]
async fn ordinary_prefix_responses_survive_a_late_blocking_notification() {
    check(
        Worker::None,
        true,
        true,
        false,
        Some(Overlap {
            notification_delay: Duration::from_millis(100),
            ..Overlap::single(200)
        }),
    )
    .await;
}

#[tokio::test]
async fn ordinary_prefix_responses_can_outlive_their_renderers_synchronous_wait() {
    check(Worker::None, true, true, false, Some(Overlap::single(20))).await;
}

#[tokio::test]
async fn a_resident_worker_starts_a_nested_worker_while_its_main_thread_waits() {
    check(Worker::NestedFromResident, true, false, false, None).await;
}

#[tokio::test]
async fn a_worker_fetch_completes_while_its_main_thread_waits_synchronously() {
    check(Worker::FetchDuringWait, true, false, false, None).await;
}

#[tokio::test]
async fn a_worker_fetch_and_popup_requests_keep_same_instant_order() {
    check(Worker::FetchDuringWait, true, true, false, None).await;
}

#[tokio::test]
async fn a_worker_sync_load_completes_with_independent_renderer_callbacks() {
    check(Worker::SyncDuringWait, false, true, false, None).await;
}

#[tokio::test]
async fn a_worker_sync_load_completes_while_its_main_thread_waits() {
    check(Worker::SyncDuringWait, true, true, false, None).await;
}

#[tokio::test]
async fn a_workers_ordinary_prefix_finishes_before_its_synchronous_wait() {
    check(Worker::SyncWithPrefix, true, true, false, None).await;
}

#[tokio::test]
async fn shared_progress_admits_a_popup_after_activation() {
    check_membership(
        Worker::None,
        true,
        true,
        false,
        Some(Overlap::single(200)),
        true,
        ArrivalPopup::None,
    )
    .await;
}

async fn check(
    worker: Worker,
    synchronous: bool,
    ordinary: bool,
    short: bool,
    overlap: Option<Overlap>,
) {
    check_membership(
        worker,
        synchronous,
        ordinary,
        short,
        overlap,
        false,
        ArrivalPopup::None,
    )
    .await;
}

#[derive(Clone, Copy, PartialEq)]
enum ArrivalPopup {
    None,
    Fetch,
    Synchronous,
    FetchInBlockedRenderer,
    SynchronousInBlockedRenderer,
    RetargetBeforeBlockedCommit,
    FailedRetargetBeforeBlockedCommit,
    BlankRetargetBeforeBlockedCommit,
}

impl ArrivalPopup {
    fn retargets_before_commit(self) -> bool {
        matches!(self, Self::RetargetBeforeBlockedCommit
            | Self::FailedRetargetBeforeBlockedCommit | Self::BlankRetargetBeforeBlockedCommit)
    }

    fn replacement_without_script(self) -> bool {
        matches!(self, Self::FailedRetargetBeforeBlockedCommit | Self::BlankRetargetBeforeBlockedCommit)
    }

    fn reuses_blocked_renderer(self) -> bool {
        matches!(
            self,
            Self::FetchInBlockedRenderer | Self::SynchronousInBlockedRenderer
        )
    }
}

#[tokio::test]
async fn a_popup_opens_while_another_renderer_waits_synchronously() {
    check_membership(
        Worker::None,
        true,
        true,
        false,
        None,
        false,
        ArrivalPopup::Fetch,
    )
    .await;
}

#[tokio::test]
async fn a_popup_opens_with_a_synchronous_load_while_another_renderer_waits() {
    check_membership(
        Worker::None,
        true,
        true,
        false,
        None,
        false,
        ArrivalPopup::Synchronous,
    )
    .await;
}

#[tokio::test]
async fn a_popup_opens_in_the_renderer_waiting_synchronously() {
    check_membership(
        Worker::None,
        true,
        true,
        false,
        None,
        false,
        ArrivalPopup::FetchInBlockedRenderer,
    )
    .await;
}

#[tokio::test]
async fn a_popup_opens_with_a_synchronous_load_in_the_blocked_renderer() {
    check_membership(
        Worker::None,
        true,
        true,
        false,
        None,
        false,
        ArrivalPopup::SynchronousInBlockedRenderer,
    )
    .await;
}

#[tokio::test]
async fn a_popup_retargets_before_its_blocked_renderer_can_commit() {
    check_membership(
        Worker::None,
        true,
        true,
        false,
        None,
        false,
        ArrivalPopup::RetargetBeforeBlockedCommit,
    )
    .await;
}

#[tokio::test]
async fn a_failed_popup_replacement_preserves_surviving_window_progress() {
    check_membership(Worker::None, true, true, false, None, false,
        ArrivalPopup::FailedRetargetBeforeBlockedCommit).await;
}

#[tokio::test]
async fn a_blank_popup_replacement_preserves_surviving_window_progress() {
    check_membership(Worker::None, true, true, false, None, false,
        ArrivalPopup::BlankRetargetBeforeBlockedCommit).await;
}

async fn check_membership(
    worker: Worker,
    synchronous: bool,
    ordinary: bool,
    short: bool,
    overlap: Option<Overlap>,
    new_popup: bool,
    arrival_popup: ArrivalPopup,
) {
    if !support::controlled_engine_or_check_baseline().await {
        return;
    }
    // These cases repeatedly coordinate two native renderers within the production
    // watchdog. Bound fixture concurrency instead of relaxing that watchdog.
    static RUN: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(3);
    let _run = RUN.acquire().await.expect("the bound is never closed");
    tokio::time::timeout(
        Duration::from_secs(90),
        run(
            worker,
            synchronous,
            ordinary,
            short,
            overlap,
            new_popup,
            arrival_popup,
        ),
    )
    .await
    .expect("shared generation progress timed out");
}
async fn run(
    worker: Worker,
    synchronous: bool,
    ordinary: bool,
    short: bool,
    overlap: Option<Overlap>,
    new_popup: bool,
    arrival_popup: ArrivalPopup,
) {
    let runs = tempfile::tempdir().unwrap();
    let worker_script = r#"globalThis.workerSamples=[];globalThis.workerReady=false;
      globalThis.worker=new Worker(URL.createObjectURL(new Blob([
        `onmessage=e=>{if(e.data.atomic){setTimeout(()=>{fetch('https://app.example.test/worker-prefix').then(r=>r.text()).then(body=>postMessage({prefix:true,after:Date.now(),body}));const at=Date.now();const result=Atomics.wait(new Int32Array(e.data.atomic),0,0,300);postMessage({at,result,after:Date.now()})},800);return;}if(e.data==='ready'){postMessage({ready:true});return;}if(e.data==='sync'||e.data==='prefix'){setTimeout(()=>{if(e.data==='prefix')fetch('https://app.example.test/worker-prefix').then(r=>r.text()).then(body=>postMessage({prefix:true,after:Date.now(),body}));const at=Date.now();const x=new XMLHttpRequest();x.open('GET','https://app.example.test/worker-fetch',false);x.send();postMessage({at,after:Date.now(),body:x.responseText})},800);return;}if(e.data==='fetch'){setTimeout(async()=>{const at=Date.now();const body=await (await fetch('https://app.example.test/worker-fetch')).text();postMessage({at,after:Date.now(),body})},800);return;}const a=new Int32Array(e.data);const at=Date.now();const result=Atomics.wait(a,0,0,300);postMessage({at,result,after:Date.now()});}`
      ],{type:'application/javascript'})));
      worker.onmessage=e=>{if(e.data.ready)workerReady=true;else workerSamples.push({data:e.data,received:Date.now(),cookie:document.cookie})};worker.postMessage('ready');"#;
    // Schedule the producer before the main thread blocks.
    // The resident worker constructs its child on its own runnable thread.
    let worker_script = if worker == Worker::NestedFromResident {
        r#"globalThis.workerSamples=[];globalThis.workerErrors=[];globalThis.workerReady=false;
          globalThis.worker=new Worker(URL.createObjectURL(new Blob([
            `onmessage=e=>{if(e.data==='ready'){postMessage({ready:true});return;}if(e.data==='nested'){setTimeout(()=>{const child=new Worker(URL.createObjectURL(new Blob(["postMessage({kind:'started',at:Date.now()});fetch('https://app.example.test/worker-prefix').then(r=>r.text()).then(body=>postMessage({kind:'response',at:Date.now(),body}));setTimeout(()=>postMessage({kind:'timer',at:Date.now()}),100);"],{type:'text/javascript'})));postMessage({kind:'created',at:Date.now()});child.onmessage=e=>postMessage({...e.data,relayed:Date.now()});child.onerror=e=>postMessage({kind:'error',at:Date.now(),message:e.message});globalThis.child=child;},800);}}`
          ],{type:'text/javascript'})));
          worker.onmessage=e=>{if(e.data.ready)workerReady=true;else workerSamples.push({data:e.data,received:Date.now(),cookie:document.cookie})};worker.onerror=e=>workerErrors.push({at:Date.now(),message:e.message});worker.postMessage('ready');"#
    } else {
        worker_script
    };
    let body = "<!doctype html><button id=open onclick=\"window.open('https://auth.example.test/')\">open</button><script></script>";
    let retarget = arrival_popup.retargets_before_commit();
    let child_url = if arrival_popup == ArrivalPopup::BlankRetargetBeforeBlockedCommit {
        "about:blank"
    } else if retarget {
        "https://auth.example.test/replacement-child"
    } else if arrival_popup.reuses_blocked_renderer() {
        "https://app.example.test/new-child"
    } else {
        "https://child.example.test/"
    };
    let child_response_url = if retarget {
        "https://auth.example.test/child-response"
    } else if arrival_popup.reuses_blocked_renderer() {
        "https://app.example.test/child-response"
    } else {
        "https://child.example.test/child-response"
    };
    let child_request = if matches!(
        arrival_popup,
        ArrivalPopup::Synchronous | ArrivalPopup::SynchronousInBlockedRenderer
    ) {
        "const x=new XMLHttpRequest();x.open('GET','/child-response',false);x.send();samples.push({at:Date.now(),cookie:document.cookie,body:x.responseText});"
    } else {
        "fetch('/child-response').then(r=>r.text()).then(body=>samples.push({at:Date.now(),cookie:document.cookie,body}));"
    };
    let child_body = format!(
        "<!doctype html><script>globalThis.first=Date.now();globalThis.samples=[];setTimeout(()=>samples.push({{at:Date.now(),cookie:document.cookie,body:'timer'}}),300);{child_request}</script>"
    );
    let rules = RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"https://app.example.test/"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":body}},
        {"id":"popup","match":{"url":"https://auth.example.test/"},"action":{"type":"mock","status":200,
            "headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":if new_popup {"<!doctype html><script>globalThis.firstInstant=Date.now();globalThis.firstSamples=[];setTimeout(()=>firstSamples.push(Date.now()),100)</script>popup"} else {"<!doctype html>popup"}}},
        {"id":"child","match":{"url":child_url},"action":if arrival_popup == ArrivalPopup::FailedRetargetBeforeBlockedCommit {json!({"type":"fail","reason":"connection_refused"})} else {json!({"type":"mock","status":200,"latency_ms":0,"headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":child_body})}},
        {"id":"abandoned-child","match":{"url":if retarget {"https://app.example.test/new-child"} else {"https://unused.example.test/abandoned"}},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":"<!doctype html><script>fetch('/abandoned-start')</script>"}},
        {"id":"abandoned-start","match":{"url":"https://app.example.test/abandoned-start"},"action":{"type":"mock","status":200,"body":"superseded"}},
        {"id":"child-response","match":{"url":child_response_url},"action":{"type":"mock","status":200,"body":"child","latency_ms":100}},
        {"id":"worker-prefix","match":{"url":"https://app.example.test/worker-prefix"},"action":{"type":"mock","status":200,"body":"prefix","headers":{"access-control-allow-origin":"*"},"latency_ms":if worker.retires_with_pending_response() { 500 } else { 50 }}},
        {"id":"worker-fetch","match":{"url":"https://app.example.test/worker-fetch"},"action":{"type":"mock","status":200,"body":"worker","latency_ms":100}},
        {"id":"overlap","match":{"url":"https://auth.example.test/overlap"},"action":{"type":"mock","status":200,"body":"overlap","latency_ms":overlap.map_or(0, |o| o.latency_ms)}},
        {"id":"async2","match":{"url":"https://auth.example.test/async2"},"action":{"type":"mock","status":200,"body":"async2","latency_ms":50}},
        {"id":"async","match":{"url":"https://auth.example.test/async"},"action":{"type":"mock","status":200,"body":"async","latency_ms":100}},
        {"id":"sync","match":{"url":"https://app.example.test/sync"},"action":{"type":"mock","status":200,"body":"done","latency_ms":if short {1} else {1000}}}
    ]).to_string()).unwrap();
    let mut options = support::options(
        runs.path(),
        "shared-time",
        "https://app.example.test/".into(),
        rules,
    );
    options.profile.popups = PopupPolicy::Controlled;
    options.passthrough_origins = Some(vec![]);
    let epoch = options.profile.initial_date_ms;
    let mut generation = Generation::launch(options).await.unwrap();
    if worker != Worker::None && !worker.dynamic() {
        generation
            .advance(VirtualDuration::from_ms(0))
            .await
            .unwrap();
        generation
            .eval_main(&format!("{worker_script};true"))
            .await
            .unwrap();
    }
    generation
        .advance(VirtualDuration::from_ms(50))
        .await
        .unwrap();
    generation
        .evaluate("document.cookie='shared=live; Domain=example.test; Path=/; Max-Age=1'")
        .await
        .unwrap();
    if new_popup {
        generation.test_enable_shared_time().await.unwrap();
    }
    generation
        .click(
            RequestId::new("open").unwrap(),
            &Locator::Css {
                selector: "#open".into(),
            },
        )
        .await
        .unwrap();
    if new_popup {
        generation
            .advance(VirtualDuration::from_ms(0))
            .await
            .unwrap();
    }
    let endpoint = generation.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url)
        .await
        .unwrap();
    let root = attach(&client, target::TargetID(endpoint.target_id.clone())).await;
    let targets = client
        .call(None, &target::GetTargetsParams::default())
        .await
        .unwrap();
    let popup = attach(
        &client,
        targets
            .target_infos
            .iter()
            .find(|t| t.r#type == "page" && t.url.starts_with("https://auth.example.test/"))
            .unwrap()
            .target_id
            .clone(),
    )
    .await;
    assert_ne!(
        client
            .call(Some(&root), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap()
            .renderer_id,
        client
            .call(Some(&popup), &emulation::FrontcapGetClockParams::default())
            .await
            .unwrap()
            .renderer_id
    );
    if new_popup {
        assert_eq!(
            eval(&client, &popup, "firstInstant".into()).await,
            json!(epoch + 50)
        );
    }
    if !new_popup {
        generation.test_enable_shared_time().await.unwrap();
    }
    for session in [&root, &popup] {
        let renderer = client
            .call(
                Some(session),
                &cdp::protocol::page::GetFrameTreeParams::default(),
            )
            .await
            .unwrap();
        let browser = client
            .call(
                Some(session),
                &cdp::protocol::page::FrontcapGetDocumentPlacesParams::default(),
            )
            .await
            .unwrap();
        assert_eq!(browser.documents.len(), 1);
        assert_eq!(
            Some(&browser.documents[0].document_id),
            renderer.frame_tree.frame.frontcap_document_id.as_ref()
        );
        assert!(browser.documents[0].frame_path.is_empty());
    }
    for session in [&root, &popup] {
        eval(&client, session, format!("globalThis.samples=[];globalThis.sample=()=>samples.push({{at:Date.now()-{epoch},cookie:document.cookie}});sample();[400,1500,2000].forEach(t=>setTimeout(sample,t));null")).await;
    }
    let resident_identity = if worker == Worker::NestedFromResident {
        let clock = client.call(Some(&root), &emulation::FrontcapGetClockParams::default()).await.unwrap();
        let inventory = client.call(Some(&root), &emulation::FrontcapGetProcessWorkersParams::default()).await.unwrap();
        let document = client.call(Some(&root), &cdp::protocol::page::GetFrameTreeParams::default()).await.unwrap()
            .frame_tree.frame.frontcap_document_id.expect("root document identity");
        assert_eq!(inventory.renderer_id, clock.renderer_id);
        assert_eq!(inventory.workers.len(), 1, "only the resident exists before the wait");
        let resident = &inventory.workers[0];
        assert!(!resident.creator_is_worker);
        assert_eq!(resident.creator_id, document);
        assert_eq!(resident.sequence, "1");
        assert!(resident.thread_started);
        Some((clock.renderer_id, document, resident.target_id.clone()))
    } else {
        None
    };
    if worker != Worker::None && !worker.dynamic() {
        assert_eq!(
            eval(&client, &root, "workerReady && crossOriginIsolated".into()).await,
            json!(true)
        );
        eval(
            &client,
            &root,
            if matches!(worker, Worker::AtomicWithPrefix | Worker::AtomicNotifiedWithPrefix) {
                "globalThis.atomicBuffer=new SharedArrayBuffer(4);worker.postMessage({atomic:atomicBuffer});null"
            } else if worker == Worker::NestedFromResident {
                "worker.postMessage('nested');null"
            } else if worker == Worker::FetchDuringWait {
                "worker.postMessage('fetch');null"
            } else if worker == Worker::SyncDuringWait {
                "worker.postMessage('sync');null"
            } else if worker == Worker::SyncWithPrefix {
                "worker.postMessage('prefix');null"
            } else {
                "worker.postMessage(new SharedArrayBuffer(4));null"
            }
            .into(),
        )
        .await;
    }
    if worker.dynamic() {
        let leaf = "postMessage({kind:'started',at:Date.now()});fetch('https://app.example.test/worker-prefix').then(r=>r.text()).then(body=>postMessage({kind:'response',at:Date.now(),body}));setTimeout(()=>postMessage({kind:'late',at:Date.now()}),1000);";
        let leaf = if matches!(worker, Worker::SyncPendingRetirement | Worker::ModuleSyncRetirement | Worker::NestedSyncRetirement) {
            "postMessage({kind:'started',at:Date.now()});const request=new XMLHttpRequest();request.open('GET','https://app.example.test/worker-prefix',false);request.send();postMessage({kind:'response',at:Date.now(),body:request.responseText});setTimeout(()=>postMessage({kind:'late',at:Date.now()}),1000);"
        } else { leaf };
        let atomic_leaf;
        let leaf = if let Some(timeout) = worker.retirement_atomic_timeout() {
            atomic_leaf = format!("const words=new Int32Array(new SharedArrayBuffer(4));postMessage({{kind:'started',at:Date.now()}});fetch('https://app.example.test/worker-prefix').then(r=>r.text()).then(body=>postMessage({{kind:'response',at:Date.now(),body}}));const result=Atomics.wait(words,0,0,{timeout});postMessage({{kind:'wait-returned',at:Date.now(),result}});");
            atomic_leaf.as_str()
        } else { leaf };
        let script = if matches!(worker, Worker::DynamicNested | Worker::NestedDuringWait | Worker::NestedPendingRetirement | Worker::NestedSyncRetirement | Worker::NestedAtomicRetirement | Worker::NestedIndefiniteAtomicRetirement) {
            format!(
                "const child=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})));child.onmessage=e=>postMessage(e.data);",
                json!(leaf)
            )
        } else {
            leaf.into()
        };
        let kind = if matches!(worker, Worker::DynamicModule | Worker::ModuleDuringWait | Worker::ModulePendingRetirement | Worker::ModuleSyncRetirement | Worker::ModuleAtomicRetirement | Worker::ModuleIndefiniteAtomicRetirement) {
            "module"
        } else {
            "classic"
        };
        let owner = if worker.starts_during_wait() { &popup } else { &root };
        let start_delay = if worker.starts_during_wait() { 800 } else { 100 };
        let retire_delay = start_delay + 200;
        eval(&client, owner, format!("globalThis.dynamicSamples=[];globalThis.dynamicErrors=[];globalThis.retiredAt=null;setTimeout(()=>{{globalThis.dynamicWorker=new Worker(URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}})),{{type:'{kind}'}});dynamicWorker.onerror=e=>dynamicErrors.push({{at:Date.now(),message:e.message}});dynamicWorker.onmessage=e=>dynamicSamples.push({{data:e.data,received:Date.now()}})}},{start_delay});setTimeout(()=>{{dynamicWorker.terminate();retiredAt=Date.now()}},{retire_delay});null", json!(script))).await;
    }
    if worker == Worker::AtomicNotifiedWithPrefix {
        eval(&client, &root, "globalThis.notified=null;setTimeout(()=>{notified={at:Date.now(),count:Atomics.notify(new Int32Array(atomicBuffer),0,1)}},875);null".into()).await;
    }
    if synchronous {
        let script = "globalThis.syncSample=null;setTimeout(()=>{const x=new XMLHttpRequest();x.open('GET','/sync',false);x.send();syncSample={at:Date.now(),body:x.responseText,cookie:document.cookie}},DELAY);null";
        eval(
            &client,
            &root,
            script.replace("DELAY", if short { "510" } else { "500" }),
        )
        .await;
    }
    if ordinary && overlap.is_none() {
        eval(&client, &popup, "globalThis.asyncSamples=[];setTimeout(()=>{for(const path of ['/async','/async2'])fetch(path).then(r=>r.text()).then(body=>asyncSamples.push({at:Date.now(),body}))},800);null".into()).await;
    }
    if let Some(overlap) = overlap {
        if !overlap.notification_delay.is_zero() {
            generation.delay_interceptions("/overlap", overlap.notification_delay);
        }
        let prefix = if ordinary {
            "for(const path of ['/async','/async2'])fetch(path).then(r=>r.text()).then(body=>asyncSamples.push({at:Date.now(),body}));"
        } else {
            ""
        };
        eval(&client, &popup, format!("globalThis.asyncSamples=[];globalThis.overlapSamples=[];setTimeout(()=>{{{prefix}for(let i=0;i<{};i++){{const x=new XMLHttpRequest();x.open('GET','/overlap',false);x.send();overlapSamples.push({{at:Date.now(),body:x.responseText}})}}}},800);null", overlap.requests)).await;
    }
    if arrival_popup != ArrivalPopup::None {
        eval(
            &client,
            &popup,
            if retarget {
                format!("setTimeout(()=>{{globalThis.arrivalChild=window.open('https://app.example.test/new-child')}},800);setTimeout(()=>{{arrivalChild.location={}}},850);null", json!(child_url))
            } else {
                format!("setTimeout(()=>window.open({}),800);null", json!(child_url))
            },
        )
        .await;
    }
    let result = generation.advance(VirtualDuration::from_ms(2000)).await;
    if let Err(error) = &result {
        eprintln!(
            "advance error: {error:?}; shared: {:?}",
            client
                .call(
                    Some(&root),
                    &cdp::protocol::page::FrontcapGetSharedClockParams::default()
                )
                .await
        );
        if let Ok(trace) = std::fs::read_to_string(generation.trace_path()) {
            eprintln!("trace: {trace}");
        }
    }
    result.unwrap();
    assert_eq!(generation.now().0, 2_050_000);
    let child = if arrival_popup != ArrivalPopup::None && !arrival_popup.replacement_without_script() {
        let targets = client
            .call(None, &target::GetTargetsParams::default())
            .await
            .unwrap();
        let child = attach(
            &client,
            targets
                .target_infos
                .iter()
                .find(|t| t.url == child_url)
                .unwrap()
                .target_id
                .clone(),
        )
        .await;
        let mut renderers = Vec::new();
        for session in [&root, &popup, &child] {
            renderers.push(
                client
                    .call(Some(session), &emulation::FrontcapGetClockParams::default())
                    .await
                    .unwrap()
                    .renderer_id,
            );
        }
        let first = if retarget {
            assert_eq!(renderers[1], renderers[2], "replacement must join the opener's renderer");
            assert_ne!(renderers[0], renderers[2], "replacement must leave the blocked root");
            // Chromium queues replacement behind the pending root commit. Its
            // zero-latency document can start when that commit becomes runnable
            // at the root response, without holding the shared clock at 900 ms.
            // This does not assert that the superseded commit can never execute.
            1550
        } else if arrival_popup.reuses_blocked_renderer() {
            assert_eq!(renderers[0], renderers[2], "child must reuse the blocked root");
            assert_ne!(renderers[0], renderers[1], "opener must remain independent");
            // Its first script cannot run on the blocked root thread until the
            // root's synchronous response arrives, despite opening at 850 ms.
            1550
        } else {
            assert_eq!(
                renderers.iter().collect::<std::collections::BTreeSet<_>>().len(),
                3,
                "admission must exercise a new independent renderer"
            );
            850
        };
        assert_eq!(
            eval(&client, &child, "first".into()).await,
            json!(epoch + first)
        );
        assert_eq!(
            eval(&client, &child, "samples".into()).await,
            json!([
                {"at":epoch+first+100,"cookie":if first + 100 < 1050 {"shared=live"} else {""},"body":"child"},
                {"at":epoch+first+300,"cookie":"","body":"timer"}
            ])
        );
        Some(child)
    } else {
        None
    };
    if new_popup {
        assert_eq!(
            eval(&client, &popup, "firstSamples".into()).await,
            json!([epoch + 150])
        );
    }
    if arrival_popup.replacement_without_script() {
        let targets = client.call(None, &target::GetTargetsParams::default()).await.unwrap();
        let pages: Vec<_> = targets.target_infos.iter().filter(|target| target.r#type == "page").collect();
        assert_eq!(pages.len(), 3, "replacement must preserve all windows");
        let replacement = pages.iter().find(|target| target.url == child_url)
            .expect("replacement target URL");
        let events = frontcap_core::log::BehaviorLog::read(&generation.trace_path()).unwrap();
        let contexts: Vec<_> = events.iter().filter_map(|event| match &event.kind {
            frontcap_core::event::EventKind::Context(context)
                if context.event == frontcap_core::event::ContextEvent::Opened
                    && context.target.as_deref() == Some(replacement.target_id.0.as_str()) =>
                Some(context.context.as_str()),
            _ => None,
        }).collect();
        assert_eq!(contexts.len(), 1, "replacement target has one generation context");
        let navigation: Vec<_> = events.iter().filter_map(|event| match &event.kind {
            frontcap_core::event::EventKind::Navigation(navigation)
                if navigation.context.as_deref() == Some(contexts[0]) && navigation.rseq == 0 =>
                Some((navigation.url.as_str(), navigation.document, event.t_us.0)),
            _ => None,
        }).collect();
        let final_url = if arrival_popup == ArrivalPopup::FailedRetargetBeforeBlockedCommit {
            "chrome-error://chromewebdata/"
        } else {
            "about:blank"
        };
        // The HTTP document commits when the blocked renderer resumes. A second
        // document in that same context proves replacement, rather than an
        // unchanged initial blank target or a failed request with no commit.
        assert_eq!(navigation, vec![
            ("https://app.example.test/new-child", 1, 1_550_000),
            (final_url, 2, 1_550_000),
        ]);
        if arrival_popup == ArrivalPopup::BlankRetargetBeforeBlockedCommit {
            let blank = attach(&client, replacement.target_id.clone()).await;
            let frame = client.call(Some(&blank), &cdp::protocol::page::GetFrameTreeParams::default()).await.unwrap().frame_tree.frame;
            assert_eq!(frame.url, "about:blank");
            assert!(!frame.loader_id.0.is_empty());
            assert!(frame.frontcap_document_id.is_some_and(|id| !id.is_empty()));
            assert_eq!(eval(&client, &blank, "({url:document.URL,readyState:document.readyState})".into()).await,
                json!({"url":"about:blank","readyState":"complete"}));
        }
    }
    for session in [&root, &popup] {
        let callback = if session == &popup {
            overlap.map_or(1550, |o| (850 + o.latency_ms * o.requests).max(1550))
        } else {
            1550
        };
        assert_eq!(
            eval(&client, session, "samples".into()).await,
            json!([
                {"at":50,"cookie":"shared=live"},{"at":450,"cookie":"shared=live"},
                {"at":callback,"cookie":""},{"at":2050,"cookie":""}
            ])
        );
    }
    if worker != Worker::None && !worker.dynamic() {
        let mut expected = if matches!(
            worker,
            Worker::FetchDuringWait | Worker::SyncDuringWait | Worker::SyncWithPrefix
        ) {
            json!([{"data":{"at":epoch+850,"after":epoch+950,"body":"worker"},"received":epoch+if synchronous {1550} else {950},"cookie":if synchronous {""} else {"shared=live"}}])
        } else {
            json!([{"data":{"at":epoch+50,"after":epoch+350,"result":"timed-out"},"received":epoch+350,"cookie":"shared=live"}])
        };
        if matches!(
            worker,
            Worker::AtomicWithPrefix | Worker::AtomicNotifiedWithPrefix
        ) {
            let end = if worker == Worker::AtomicWithPrefix {
                1150
            } else {
                925
            };
            let received = if synchronous { 1550 } else { end };
            expected = json!([
                {"data":{"at":epoch+850,"after":epoch+end,"result":if worker == Worker::AtomicWithPrefix {"timed-out"} else {"ok"}},"received":epoch+received,"cookie":if synchronous {""} else {"shared=live"}},
                {"data":{"prefix":true,"after":epoch+end,"body":"prefix"},"received":epoch+received,"cookie":if synchronous {""} else {"shared=live"}}
            ]);
        }
        if worker == Worker::AtomicNotifiedWithPrefix {
            assert_eq!(
                eval(&client, &root, "notified".into()).await,
                json!({"at":epoch+925,"count":1})
            );
        }
        if worker == Worker::NestedFromResident {
            expected = json!([
                {"data":{"kind":"created","at":epoch+850},"received":epoch+1550,"cookie":""},
                {"data":{"kind":"started","at":epoch+850,"relayed":epoch+850},"received":epoch+1550,"cookie":""},
                {"data":{"kind":"response","at":epoch+900,"relayed":epoch+900,"body":"prefix"},"received":epoch+1550,"cookie":""},
                {"data":{"kind":"timer","at":epoch+950,"relayed":epoch+950},"received":epoch+1550,"cookie":""}
            ]);
            let errors = eval(&client, &root, "workerErrors".into()).await;
            assert_eq!(errors, json!([]));
            let (renderer, document, resident_id) = resident_identity.as_ref().unwrap();
            let inventory = client.call(Some(&root), &emulation::FrontcapGetProcessWorkersParams::default()).await.unwrap();
            assert_eq!(&inventory.renderer_id, renderer);
            assert_eq!(inventory.workers.len(), 2, "resident and child stay live for startup observation");
            let resident = inventory.workers.iter().find(|entry| &entry.target_id == resident_id).unwrap();
            assert!(!resident.creator_is_worker);
            assert_eq!(&resident.creator_id, document);
            let child = inventory.workers.iter().find(|entry| entry.creator_is_worker).unwrap();
            assert_eq!(&child.creator_id, resident_id);
            assert_eq!(child.sequence, "1");
            assert!(resident.thread_started && child.thread_started);
            let bindings = frontcap_chromium::bindings::BindingJournal::read(
                &generation.trace_path().parent().unwrap().join("interceptions.jsonl")
            ).unwrap();
            let lineages: Vec<_> = bindings.into_iter().filter_map(|entry| match entry {
                frontcap_chromium::bindings::BindingEntry::Bound(binding)
                    if binding.url == "https://app.example.test/worker-prefix" =>
                    Some(binding.worker_lineage.expect("child fetch must retain constructor lineage")),
                _ => None,
            }).collect();
            assert_eq!(lineages, vec![frontcap_chromium::bindings::WorkerLineage {
                creator_document: document.clone(), constructors: vec![1, 1],
            }]);
        }
        if worker == Worker::SyncWithPrefix {
            expected.as_array_mut().unwrap().push(json!({
                "data":{"prefix":true,"after":epoch+950,"body":"prefix"},
                "received":epoch+1550,"cookie":""
            }));
        }
        assert_eq!(eval(&client, &root, "workerSamples".into()).await, expected);
    }
    if worker.dynamic() {
        let owner = if worker.starts_during_wait() { &popup } else { &root };
        let started = if worker.starts_during_wait() { 850 } else { 150 };
        let mut expected = vec![json!({"data":{"kind":"started","at":epoch+started},"received":epoch+started})];
        if !worker.retires_with_pending_response() {
            expected.push(json!({"data":{"kind":"response","at":epoch+started+50,"body":"prefix"},"received":epoch+started+50}));
        }
        let errors = eval(&client, owner, "dynamicErrors".into()).await;
        if matches!(worker, Worker::SyncPendingRetirement | Worker::ModuleSyncRetirement | Worker::NestedSyncRetirement) {
            // Terminating a synchronous XHR may surface its cancellation at the
            // owner. It must not be a failure before termination or a late event.
            for error in errors.as_array().unwrap() {
                assert_eq!(error["at"], json!(epoch + started + 200));
                assert!(error["message"].as_str().unwrap().starts_with("Uncaught NetworkError:"), "{error}");
            }
        } else {
            assert_eq!(errors, json!([]));
        }
        assert_eq!(eval(&client, owner, "dynamicSamples".into()).await, json!(expected));
        assert_eq!(
            eval(&client, owner, "retiredAt".into()).await,
            json!(epoch + started + 200)
        );
        assert!(
            client
                .call(Some(owner), &emulation::FrontcapGetWorkersParams::default())
                .await
                .unwrap()
                .workers
                .is_empty(),
            "terminated workers must leave native membership"
        );
    }
    if let Some(overlap) = overlap {
        let samples: Vec<_> = (1..=overlap.requests)
            .map(|n| json!({"at":epoch+850+overlap.latency_ms*n,"body":"overlap"}))
            .collect();
        assert_eq!(
            eval(&client, &popup, "overlapSamples".into()).await,
            json!(samples)
        );
    }
    if ordinary
        || worker.dynamic()
        || overlap.is_some()
        || matches!(
            worker,
            Worker::FetchDuringWait
                | Worker::NestedFromResident
                | Worker::SyncDuringWait
                | Worker::SyncWithPrefix
                | Worker::AtomicWithPrefix
                | Worker::AtomicNotifiedWithPrefix
        )
    {
        let events = frontcap_core::log::BehaviorLog::read(&generation.trace_path()).unwrap();
        let requests: std::collections::BTreeMap<_, _> = events
            .iter()
            .filter_map(|event| {
                if let frontcap_core::event::EventKind::Request(request) = &event.kind {
                    Some((request.id.clone(), (request.url.clone(), event.t_us.0)))
                } else {
                    None
                }
            })
            .collect();
        if arrival_popup == ArrivalPopup::FailedRetargetBeforeBlockedCommit {
            let selected: Vec<_> = requests.iter().filter(|(_, (url, _))| url == child_url).collect();
            assert_eq!(selected.len(), 1, "one failed replacement request");
            assert_eq!(selected[0].1.1, 900_000);
            let failed: Vec<_> = events.iter().filter(|event| matches!(&event.kind,
                frontcap_core::event::EventKind::Response(response)
                if &response.id == selected[0].0 && response.source == frontcap_core::event::ResponseSource::Fail)).collect();
            assert_eq!(failed.len(), 1, "one retained failed response");
            assert_eq!(failed[0].t_us.0, 900_000);
        }
        if worker.retires_with_pending_response() {
            let pending: Vec<_> = requests.values().filter(|(url, _)| url.ends_with("/worker-prefix")).collect();
            assert_eq!(pending.len(), 1, "the terminated worker must have issued its pending load");
            assert_eq!(pending[0].1, 850_000, "the 500 ms response is still pending at retirement at 1050 ms");
        }
        let responses: Vec<_> = events
            .iter()
            .filter_map(|event| {
                if let frontcap_core::event::EventKind::Response(response) = &event.kind {
                    let (url, issued) = requests.get(&response.id)?;
                    if worker.retires_with_pending_response() && url.ends_with("/worker-prefix") {
                        assert_eq!(response.source, frontcap_core::event::ResponseSource::Fail,
                            "a terminated worker must not receive a successful response");
                        return None;
                    }
                    if url.ends_with("/async")
                        || url.ends_with("/async2")
                        || url.ends_with("/sync")
                        || url.ends_with("/overlap")
                        || url.ends_with("/worker-fetch")
                        || url.ends_with("/worker-prefix")
                    {
                        return Some((
                            url.rsplit('/').next().unwrap().to_owned(),
                            *issued,
                            event.t_us.0,
                        ));
                    }
                }
                None
            })
            .collect();
        let mut expected = if synchronous {
            vec![("sync".to_owned(), 550_000, 1_550_000)]
        } else {
            vec![]
        };
        if let Some(overlap) = overlap {
            expected.extend((0..overlap.requests).map(|n| {
                (
                    "overlap".to_owned(),
                    (850 + overlap.latency_ms * n) * 1000,
                    (850 + overlap.latency_ms * (n + 1)) * 1000,
                )
            }));
        }
        if ordinary {
            expected.extend([
                ("async2".to_owned(), 850_000, 900_000),
                ("async".to_owned(), 850_000, 950_000),
            ]);
        }
        if matches!(
            worker,
            Worker::FetchDuringWait | Worker::SyncDuringWait | Worker::SyncWithPrefix
        ) {
            expected.push(("worker-fetch".to_owned(), 850_000, 950_000));
        }
        if matches!(
            worker,
            Worker::SyncWithPrefix | Worker::AtomicWithPrefix | Worker::AtomicNotifiedWithPrefix
        ) {
            expected.push(("worker-prefix".to_owned(), 850_000, 900_000));
        }
        if worker == Worker::NestedFromResident {
            expected.push(("worker-prefix".to_owned(), 850_000, 900_000));
        }
        if worker.dynamic() && !worker.retires_with_pending_response() {
            let started = if worker.starts_during_wait() { 850_000 } else { 150_000 };
            expected.push(("worker-prefix".to_owned(), started, started + 50_000));
        }
        expected.sort_by_key(|response| response.2);
        assert_eq!(responses, expected);
        if ordinary {
            let ready = overlap.map_or(0, |o| 850 + o.latency_ms * o.requests);
            assert_eq!(
                eval(&client, &popup, "asyncSamples".into()).await,
                json!([{"at":epoch+900.max(ready),"body":"async2"},{"at":epoch+950.max(ready),"body":"async"}])
            );
        }
    }
    if synchronous {
        assert_eq!(
            eval(&client, &root, "syncSample".into()).await,
            if short {
                json!({"at":epoch+561,"body":"done","cookie":"shared=live"})
            } else {
                json!({"at":epoch+1550,"body":"done","cookie":""})
            }
        );
    }
    generation
        .advance(VirtualDuration::from_micros(125))
        .await
        .unwrap();
    assert_eq!(generation.now().0, 2_050_125);
    tokio::time::sleep(Duration::from_millis(100)).await;
    for session in [&root, &popup].into_iter().chain(child.as_ref()) {
        assert_eq!(
            client
                .call(Some(session), &emulation::GetVirtualTimeParams::default())
                .await
                .unwrap()
                .unix_time_us,
            (i128::from(epoch) * 1000 + 2_050_125).to_string()
        );
    }
}
