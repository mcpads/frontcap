//! A missing worker setup acknowledgement must not release unconfigured script execution.
mod support;

use cdp::protocol::{runtime, target};
use frontcap_chromium::{ChromiumError, Generation};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn a_lost_worker_setup_reply_faults_progress_without_running_the_worker() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let rules = RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"https://enrollment.example.test/"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"},"body":"<!doctype html><p>worker enrollment</p>"}}
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "worker-enrollment", "https://enrollment.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    options.watchdog = Duration::from_secs(40);
    let mut generation = Generation::launch(options).await.unwrap();
    let endpoint = generation.endpoint();
    generation.omit_worker_network_enable_answer();
    generation.eval_main("window.words=new Int32Array(new SharedArrayBuffer(4));window.worker=new Worker(URL.createObjectURL(new Blob(['onmessage=e=>Atomics.store(new Int32Array(e.data),0,1)'],{type:'text/javascript'})));worker.postMessage(words.buffer);true").await.unwrap();
    let before = generation.now();
    let error = generation.advance(VirtualDuration::from_ms(100)).await.unwrap_err();
    assert!(matches!(error, ChromiumError::WorkerEnrollment { method: "Network.enable", .. }), "{error:?}");
    assert_eq!(generation.now(), before);
    assert!(matches!(generation.advance(VirtualDuration::from_ms(0)).await, Err(ChromiumError::WorkerEnrollment { .. })), "the failure remains explicit on later operations");
    // The generation correctly refuses further observation. A diagnostic connection
    // reads shared memory without releasing the worker or depending on a page message.
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let page = client.call(None, &target::AttachToTargetParams { flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id)) }).await.unwrap().session_id;
    let read = client.call(Some(&page.0), &runtime::EvaluateParams { return_by_value: Some(true), ..runtime::EvaluateParams::new("Atomics.load(words,0)".into()) }).await.unwrap();
    assert_eq!(read.result.value, Some(json!(0)), "the unconfigured worker must never be resumed");
}
