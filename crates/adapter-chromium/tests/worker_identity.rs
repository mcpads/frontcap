//! Native constructor identity must precede script delivery and survive CDP replacement.
mod support;

use cdp::protocol::{emulation, page, target};
use frontcap_chromium::{Generation, bindings::{BindingEntry, BindingJournal, WorkerLineage}};
use frontcap_core::{rules::{MockSpec, RuleSet}, time::VirtualDuration};
use serde_json::json;
use std::collections::BTreeMap;

async fn workers(client: &cdp::Client) -> Vec<target::TargetInfo> {
    client.call(None, &target::GetTargetsParams::default()).await.unwrap()
        .target_infos.into_iter().filter(|info| info.r#type == "worker").collect()
}

async fn documents(client: &cdp::Client, target_id: &str) -> BTreeMap<String, String> {
    let session = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(target_id.into()))
    }).await.unwrap().session_id;
    let before = client.call(Some(&session.0), &emulation::GetVirtualTimeParams::default()).await.unwrap().unix_time_us;
    let tree = client.call(Some(&session.0), &page::GetFrameTreeParams::default()).await.unwrap().frame_tree;
    let after = client.call(Some(&session.0), &emulation::GetVirtualTimeParams::default()).await.unwrap().unix_time_us;
    assert_eq!(before, after, "document inventory must preserve virtual time");
    client.call(None, &target::DetachFromTargetParams { session_id: Some(session), target_id: None }).await.unwrap();
    let mut pending = vec![tree];
    let mut documents = BTreeMap::new();
    while let Some(tree) = pending.pop() {
        let document = tree.frame.frontcap_document_id.expect("native frame document identity");
        assert!(!document.is_empty());
        documents.insert(tree.frame.id.0, document);
        pending.extend(tree.child_frames.unwrap_or_default());
    }
    documents
}

async fn assert_creator_document(client: &cdp::Client, target_id: &str, worker: &target::TargetInfo) {
    let frames = documents(client, target_id).await;
    let frame = worker.parent_frame_id.as_ref().expect("creator frame");
    assert_eq!(frames.get(&frame.0), worker.frontcap_creator_document_id.as_ref());
}

fn sequence(info: &target::TargetInfo) -> u64 {
    info.frontcap_worker_sequence.as_ref().expect("native constructor sequence").parse().unwrap()
}

fn lineages(g: &Generation, suffix: &str) -> Vec<WorkerLineage> {
    let path = g.trace_path().parent().unwrap().join("interceptions.jsonl");
    let mut found: Vec<_> = BindingJournal::read(&path).unwrap().into_iter().filter_map(|entry| match entry {
        BindingEntry::Bound(b) if b.url.ends_with(suffix) => Some(b.worker_lineage.expect("bound worker request must retain constructor lineage")),
        _ => None,
    }).collect();
    found.sort_by(|a, b| a.constructors.cmp(&b.constructors));
    found
}

fn script(body: &str) -> MockSpec {
    serde_json::from_value(json!({"status":200,"headers":{"content-type":"text/javascript"},"body":body})).unwrap()
}

#[tokio::test]
async fn identical_workers_have_constructor_identity_before_loading_and_after_reconnection() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let rules = RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"https://identity.example.test/"},"action":{"type":"mock","status":200,"headers":{"content-type":"text/html"},"body":"<!doctype html><p>worker identity</p>"}},
        {"id":"scripts","match":{"url":"https://identity.example.test/*.js"},"action":{"type":"hold"}},
        {"id":"owned","match":{"url":"https://identity.example.test/owned"},"action":{"type":"mock","status":200,"body":"ok"}}
    ]).to_string()).unwrap();
    let runs = tempfile::tempdir().unwrap();
    let mut options = support::options(runs.path(), "worker-identity", "https://identity.example.test/".into(), rules);
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    let endpoint = g.endpoint();
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    g.eval_main("window.ready=[];window.owned=['first','second'].map(name=>{const w=new Worker('/same.js',{name});w.onmessage=e=>ready.push(e.data);return w});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.held().len(), 2);
    assert_eq!(g.eval_main("ready").await.unwrap(), json!([]));
    let before = workers(&client).await;
    assert_eq!(before.len(), 2, "both worker hosts exist while their scripts are held");
    let by_sequence: BTreeMap<_, _> = before.iter().map(|info| (sequence(info), info.target_id.0.clone())).collect();
    assert_eq!(by_sequence.keys().copied().collect::<Vec<_>>(), [1, 2]);
    let document = before[0].frontcap_creator_document_id.clone().expect("creator document identity");
    assert!(!document.is_empty());
    let initial = lineages(&g, "/same.js");
    assert_eq!(initial, vec![
        WorkerLineage { creator_document: document.clone(), constructors: vec![1] },
        WorkerLineage { creator_document: document.clone(), constructors: vec![2] },
    ]);
    for info in &before {
        assert_eq!(info.frontcap_creator_document_id.as_ref(), Some(&document));
        assert_eq!(info.parent_id.as_ref().map(|id| id.0.as_str()), Some(endpoint.target_id.as_str()));
        assert!(info.parent_frame_id.is_some());
    }
    assert_creator_document(&client, &endpoint.target_id, &before[0]).await;
    let current = documents(&client, &endpoint.target_id).await;
    g.eval_main("history.pushState(null,'','#same-document');true").await.unwrap();
    assert_eq!(documents(&client, &endpoint.target_id).await, current,
        "same-document navigation retains the creator identity");
    // Reverse response order must not replace constructor order with readiness order.
    for ordinal in g.held().into_iter().rev() {
        g.respond(&ordinal, script("postMessage(name);fetch('/owned');onmessage=()=>{globalThis.children=['child-first','child-second'].map(name=>new Worker('/child.js',{name}));};")).await.unwrap();
    }
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(lineages(&g, "/owned"), initial, "worker renderer requests use the same lineage as their browser script");
    let loaded = workers(&client).await;
    for info in &loaded {
        assert_eq!(info.target_id.0, by_sequence[&sequence(info)]);
        assert_eq!(sequence(info), match info.title.as_str() { "first" => 1, "second" => 2, other => panic!("unexpected worker title {other}") });
    }
    drop(client);
    let replacement = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let recovered: BTreeMap<_, _> = workers(&replacement).await.into_iter().map(|info| (sequence(&info), info.target_id.0)).collect();
    assert_eq!(recovered, by_sequence);
    assert_eq!(documents(&replacement, &endpoint.target_id).await, current,
        "replacing the diagnostic CDP connection retains document identities");

    // Nested identities use their actual creator worker, with an independent sequence.
    g.eval_main("owned[1].postMessage('children');true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.held().len(), 2);
    let nested: Vec<_> = workers(&replacement).await.into_iter()
        .filter(|info| info.parent_id.as_ref().is_some_and(|id| id.0 == by_sequence[&2])).collect();
    assert_eq!(nested.len(), 2);
    let mut nested_order: Vec<_> = nested.iter().map(sequence).collect();
    nested_order.sort();
    assert_eq!(nested_order, [1, 2]);
    assert!(nested.iter().all(|info| info.frontcap_creator_document_id.is_none()));
    assert_eq!(lineages(&g, "/child.js"), vec![
        WorkerLineage { creator_document: document.clone(), constructors: vec![2, 1] },
        WorkerLineage { creator_document: document.clone(), constructors: vec![2, 2] },
    ]);
    // Termination cannot recycle a sequence within the still-living document.
    g.eval_main("owned.forEach(w=>w.terminate());window.third=new Worker('/same.js',{name:'third'});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(lineages(&g, "/owned"), initial, "retirement does not rewrite already-bound ownership");
    let after = workers(&replacement).await;
    assert!(after.iter().any(|info| sequence(info) == 3 && info.frontcap_creator_document_id.as_ref() == Some(&document)));
    // A new document can restart its counter only under a different document identity.
    g.eval_main("third.terminate();true").await.unwrap();
    g.navigate_action(&frontcap_core::ids::RequestId::new("reload").unwrap(), &frontcap_core::action::Action::Reload {}).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    g.eval_main("window.fresh=new Worker('/same.js',{name:'fresh'});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    let fresh = workers(&replacement).await;
    // A cancelled, still-held script can retain a browser host after its document
    // retires. Correlate this document; target inventory is not an enrollment barrier.
    let fresh: Vec<_> = fresh.into_iter().filter(|info|
        info.parent_frame_id == before[0].parent_frame_id &&
        info.frontcap_creator_document_id.as_ref().is_some_and(|id| id != &document)).collect();
    assert_eq!(fresh.len(), 1);
    assert_eq!(sequence(&fresh[0]), 1);
    assert_ne!(fresh[0].frontcap_creator_document_id.as_ref().unwrap(), &document);
    assert_creator_document(&replacement, &endpoint.target_id, &fresh[0]).await;
    // Same-origin navigation can reuse an initial empty iframe's LocalDOMWindow.
    g.eval_main("window.frame=document.createElement('iframe');document.body.append(frame);frame.contentWindow.early=new frame.contentWindow.Worker('https://identity.example.test/same.js');true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    let empty_frame_worker = workers(&replacement).await.into_iter()
        .find(|info| info.parent_frame_id != fresh[0].parent_frame_id).expect("initial empty iframe worker");
    assert_eq!(sequence(&empty_frame_worker), 1);
    assert_creator_document(&replacement, &endpoint.target_id, &empty_frame_worker).await;
    g.eval_main("frame.src='https://identity.example.test/';true").await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    g.eval_main("frame.contentWindow.later=new frame.contentWindow.Worker('/same.js');true").await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    let next_frame_worker = workers(&replacement).await.into_iter()
        .find(|info| info.parent_frame_id == empty_frame_worker.parent_frame_id && info.frontcap_creator_document_id != empty_frame_worker.frontcap_creator_document_id).expect("navigated iframe worker");
    assert_eq!(sequence(&next_frame_worker), 1);
    assert_ne!(next_frame_worker.frontcap_creator_document_id, empty_frame_worker.frontcap_creator_document_id);
    assert_creator_document(&replacement, &endpoint.target_id, &next_frame_worker).await;
    g.stop().await.unwrap();
}
