//! Requests independent issuers make at one instant take the declared order (time-model.md, Ordering at
//! the same instant): the page's thread, then dedicated workers by creator document and constructor
//! path, whichever thread the host ran first. Shared memory and messages force each issue order.
mod support;

use frontcap_chromium::Generation;
use frontcap_core::{event::EventKind, log::BehaviorLog, rules::RuleSet, time::{VirtualDuration, VirtualTime}};
use serde_json::json;

const ORIGIN: &str = "https://request-order.example.test";

/// Issues its request when told, after its turn in the shared counter when gated; hands the turn on
/// and tells the page. A message `to` another name goes to this worker's nested worker. One Blob
/// script serves every worker, so neither script URL nor request URL identifies an issuer.
const WORKER: &str = r#"
const take=(words,mine)=>{for(let v;(v=Atomics.load(words,0))!==mine;)Atomics.wait(words,0,v);};
let nested;
onmessage=e=>{
  const d=e.data;
  if(d.nest){nested=new Worker(location.href,{name:d.nest});nested.onmessage=m=>postMessage(m.data);return;}
  if(d.to&&d.to!==name){nested.postMessage(d);return;}
  const words=d.buffer&&new Int32Array(d.buffer);
  if(words)take(words,d.turns[name]);
  fetch(d.origin+'/sequence',{method:'POST',body:name}).then(r=>r.text()).then(body=>postMessage({name,body}));
  if(words){Atomics.add(words,0,1);Atomics.notify(words,0);}
  postMessage({issued:name});
};
postMessage({ready:name});
"#;

/// The page's side: answers land in `got`; `onissued(name)` runs once a worker has issued its request.
const PAGE: &str = r#"
window.got={};window.ready=[];window.onissued=()=>{};
window.listen=w=>{w.addEventListener('message',e=>{
  if(e.data.ready)ready.push(e.data.ready);else if(e.data.issued)onissued(e.data.issued);else got[e.data.name]=e.data.body;});return w;};
window.issue=name=>fetch('/sequence',{method:'POST',body:name}).then(r=>r.text()).then(body=>got[name]=body);
window.go=turns=>({buffer:new SharedArrayBuffer(4),turns,origin:location.origin});
// Each named issuer issues once the previous one's request is out; `a1` is `a`'s nested worker.
window.chain=order=>{let i=0;const next=()=>{const name=order[i++];if(name===undefined)return;
  if(name==='page'){issue('page');next();return;}
  (name==='a1'?workers.a:workers[name]).postMessage({to:name,origin:location.origin});};onissued=next;next();};
"#;

fn rules(steps: usize) -> RuleSet {
    // Cross-origin isolated documents, for the shared memory that gates the workers.
    let isolated = json!({"content-type":"text/html","cross-origin-opener-policy":"same-origin","cross-origin-embedder-policy":"require-corp"});
    let steps: Vec<_> = (1..=steps).map(|n| json!({"status":200,"body":n.to_string()})).collect();
    RuleSet::from_json(&json!([
        {"id":"page","match":{"url":format!("{ORIGIN}/")},"action":{"type":"mock","status":200,"headers":isolated,"body":"<!doctype html><p>request order</p>"}},
        {"id":"frame","match":{"url":format!("{ORIGIN}/frame")},"action":{"type":"mock","status":200,"headers":isolated,"body":"<!doctype html><p>frame</p>"}},
        {"id":"sequence","match":{"url":"https://*request-order.example.test/sequence*"},"action":{"type":"sequence","after_last":"fail","steps":steps}}
    ]).to_string()).unwrap()
}

/// A page with `workers` (names, in constructor order) started and listening.
async fn launch(runs: &std::path::Path, run: &str, steps: usize, workers: &[&str]) -> Generation {
    let mut options = support::options(runs, run, format!("{ORIGIN}/"), rules(steps));
    options.passthrough_origins = Some(vec![]);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    g.eval_main(&format!("{PAGE}window.script=URL.createObjectURL(new Blob([{}],{{type:'text/javascript'}}));\
        window.workers={{}};for(const name of {}){{workers[name]=listen(new Worker(script,{{name}}));}};true", json!(WORKER), json!(workers))).await.unwrap();
    g
}

/// The instants of the trace's sequence requests, in the trace's order.
fn sequence_instants(g: &Generation) -> Vec<VirtualTime> {
    BehaviorLog::read(&g.trace_path()).unwrap().into_iter().filter_map(|e| match e.kind {
        EventKind::Request(r) if r.url.ends_with("/sequence") && r.method != "OPTIONS" => Some(e.t_us),
        _ => None,
    }).collect()
}

/// Runs `script` while the page is stopped, then serves the instant; returns the answers.
async fn issue_at_one_instant(g: &mut Generation, script: &str, expected: usize) -> serde_json::Value {
    let at = g.now();
    g.eval_main(&format!("{script};true")).await.unwrap();
    g.advance(VirtualDuration::from_ms(0)).await.unwrap();
    assert_eq!(g.now(), at);
    assert_eq!(sequence_instants(g), vec![at; expected], "every request is issued at the stopped instant");
    g.eval_main("got").await.unwrap()
}

#[tokio::test]
async fn same_instant_workers_take_their_constructor_order_whichever_issues_first() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    for (run, turns) in [("constructor-first", json!({"one":0,"two":1})), ("constructor-last", json!({"one":1,"two":0}))] {
        let mut g = launch(runs.path(), run, 2, &["one", "two"]).await;
        g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
        assert_eq!(g.eval_main("ready.length").await.unwrap(), 2);
        let got = issue_at_one_instant(&mut g, &format!("const m=go({turns});for(const w of Object.values(workers))w.postMessage(m)"), 2).await;
        assert_eq!(got, json!({"one":"1","two":"2"}), "{run}: issued in the order {turns}");
        g.stop().await.unwrap();
    }
}

#[tokio::test]
async fn the_page_thread_precedes_its_workers_at_one_instant() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    for (run, script) in [
        ("page-issues-first", "issue('page');workers.worker.postMessage(go({worker:0}))"),
        // The page issues only once the worker's request is out.
        ("worker-issues-first", "onissued=()=>issue('page');workers.worker.postMessage(go({worker:0}))"),
    ] {
        let mut g = launch(runs.path(), run, 2, &["worker"]).await;
        g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
        let got = issue_at_one_instant(&mut g, script, 2).await;
        assert_eq!(got, json!({"page":"1","worker":"2"}), "{run}");
        g.stop().await.unwrap();
    }
}

/// A worker's answer that makes the page issue another request at the same instant starts the next
/// round: every request of the first is allocated before any answer is delivered.
#[tokio::test]
async fn a_request_an_answer_causes_at_the_same_instant_follows_the_whole_round() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    for (run, turns) in [("in-order", json!({"one":0,"two":1})), ("reversed", json!({"two":0,"one":1}))] {
        let mut g = launch(runs.path(), run, 4, &["one", "two"]).await;
        g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
        // The first worker's answer reaches the page, which then issues `after` at once.
        g.eval_main("workers.one.addEventListener('message',e=>{if(e.data.body)issue('after');});true").await.unwrap();
        let got = issue_at_one_instant(&mut g, &format!("issue('page');const m=go({turns});for(const w of Object.values(workers))w.postMessage(m)"), 4).await;
        assert_eq!(got, json!({"page":"1","one":"2","two":"3","after":"4"}), "{run}: issued in the order {turns}");
        g.stop().await.unwrap();
    }
}

/// Nested workers follow their creator; a child frame's workers follow the top document's.
#[tokio::test]
async fn workers_of_nested_owners_and_child_documents_take_their_logical_places() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    let mut g = launch(runs.path(), "logical-places", 5, &["a", "b"]).await;
    g.eval_main(&format!("window.frame=document.body.appendChild(document.createElement('iframe'));frame.src={};true", json!(format!("{ORIGIN}/frame")))).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    // `c`'s constructor is the child document's Worker; `a1` is `a`'s own worker.
    g.eval_main("workers.c=listen(new frame.contentWindow.Worker(script,{name:'c'}));workers.a.postMessage({nest:'a1'});true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
    assert_eq!(g.eval_main("ready.slice().sort()").await.unwrap(), json!(["a", "a1", "b", "c"]));
    // Issued in the reverse of the declared order: c, b, a1, a, then the page.
    let got = issue_at_one_instant(&mut g, "chain(['c','b','a1','a','page'])", 5).await;
    assert_eq!(got, json!({"page":"1","a":"2","a1":"3","b":"4","c":"5"}));
    g.stop().await.unwrap();
}


/// A later page task cannot take sequence positions from an already collected worker round.
#[tokio::test]
async fn a_synchronous_request_during_selection_follows_the_existing_round() {
    if !support::controlled_engine_or_check_baseline().await { return; }
    let runs = tempfile::tempdir().unwrap();
    for (run, prefix) in [
        ("sync-during-round", "issue('page')"),
        ("sync-preflight-during-round", "fetch('https://other.request-order.example.test/sequence',{method:'POST',headers:{'X-Test':'1'},body:'page'}).then(r=>r.text()).then(body=>got.page=body)"),
    ] {
        let mut g = launch(runs.path(), run, 4, &["one", "two"]).await;
        g.advance(VirtualDuration::from_ms(1000)).await.unwrap();
        g.evaluate_after_round_selection(format!("{prefix};const x=new XMLHttpRequest();x.open('POST','/sequence',false);x.send('sync');got.sync=x.responseText;true"));
        let got = issue_at_one_instant(&mut g,
            "const m=go({two:0,one:1});for(const w of Object.values(workers))w.postMessage(m)", 4).await;
        assert_eq!(got, json!({"one":"1","two":"2","page":"3","sync":"4"}), "{run}");
        g.stop().await.unwrap();
    }
}
