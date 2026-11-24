//! Browser-owned stream reports survive their document and reporting inspector session.
mod support;

use cdp::protocol::{page,runtime,target};
use frontcap_chromium::{Generation,session::Session};
use frontcap_core::{rules::RuleSet,time::VirtualDuration};
use serde::Serialize;
use serde_json::{Value,json};

// Raw frame helpers keep the fixture setup independent of optional frame metadata.
macro_rules! command {
    ($name:ident,$method:literal) => {
        #[derive(Serialize)]
        #[serde(transparent)]
        struct $name(Value);
        impl cdp::Command for $name {
            const METHOD:&'static str=$method;
            type Returns=Value;
        }
    };
}
command!(Frames,"Page.getFrameTree");
command!(World,"Page.createIsolatedWorld");

async fn reports(session:&Session)->Value {
    serde_json::to_value(session.call(&page::FrontcapGetStreamReportsParams::default()).await.unwrap()).unwrap()
}

async fn attach(endpoint:&frontcap_chromium::generation::Endpoint)->Session {
    let client=cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached=client.call(None,&target::AttachToTargetParams {
        flatten:Some(true),..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id.clone()))
    }).await.unwrap();
    Session::new(client,attached.session_id.0)
}

#[tokio::test]
async fn stream_receipts_survive_source_removal_and_session_replacement_until_acknowledged() {
    assert!(support::controlled_engine_or_check_baseline().await);
    let runs=tempfile::tempdir().unwrap();
    let rules=RuleSet::from_json(&json!([
        {"id":"parent","match":{"url":"http://top.example.test/"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><iframe src='http://child.example.test/'></iframe>"}},
        {"id":"child","match":{"url":"http://child.example.test/"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><p>Child</p>"}}
    ]).to_string()).unwrap();
    let mut options=support::options(runs.path(),"receipts","http://top.example.test/".into(),rules);
    let epoch_us=options.profile.initial_date_ms*1000;
    options.passthrough_origins=Some(vec![]);
    let mut g=Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration(17_333)).await.unwrap();
    let before=g.now();
    assert_eq!(g.eval_main("typeof __frontcapRetainStreamReport").await.unwrap(),"undefined");
    let endpoint=g.endpoint();
    let native=attach(&endpoint).await;
    let tree=native.call(&Frames(json!({}))).await.unwrap();
    let frame=tree["frameTree"]["childFrames"][0]["frame"]["id"].as_str().unwrap();
    let world=native.call(&World(json!({"frameId":frame,"worldName":"frontcap"}))).await.unwrap();
    let context=runtime::ExecutionContextId(world["executionContextId"].as_i64().unwrap());
    let payload=json!({"id":1,"op":"send","data":"retained child call"}).to_string();
    let expression=format!("__frontcapRetainStreamReport({})",serde_json::to_string(&payload).unwrap());
    let first=native.eval(&expression,Some(context.clone())).await.unwrap();
    let second=native.eval(&expression,Some(context)).await.unwrap();
    assert_ne!(first,second,"equal payloads are distinct calls");
    let original=reports(&native).await;
    let receipts=original["receipts"].as_array().unwrap();
    assert_eq!(receipts.len(),2);
    assert_eq!(receipts[0]["receiptId"],first);
    assert_eq!(receipts[1]["receiptId"],second);
    for receipt in receipts {
        assert_eq!(receipt["frameId"],frame);
        assert_eq!(receipt["payload"],payload);
        assert!(!receipt["documentId"].as_str().unwrap().is_empty());
        assert_eq!(receipt["unixTimeUs"].as_str().unwrap().parse::<u64>().unwrap(),epoch_us+before.0);
    }
    native.client.call(None,&target::DetachFromTargetParams {
        session_id:Some(target::SessionID(native.id.clone())),..Default::default()
    }).await.unwrap();
    drop(native);
    g.eval_main("document.querySelector('iframe').remove();true").await.unwrap();
    let replacement=attach(&endpoint).await;
    assert_eq!(reports(&replacement).await,original);
    replacement.call(&page::FrontcapForgetStreamReportsParams { receipt_ids: vec![first.as_str().unwrap().into()] }).await.unwrap();
    let left=reports(&replacement).await;
    assert_eq!(left["receipts"],json!([receipts[1]]));
    replacement.call(&page::FrontcapForgetStreamReportsParams { receipt_ids: vec![first.as_str().unwrap().into(), second.as_str().unwrap().into()] }).await.unwrap();
    assert_eq!(reports(&replacement).await["receipts"],json!([]));
    assert_eq!(g.now(),before);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn closes_before_admission_are_recorded_and_acknowledged_without_opening_streams() {
    use frontcap_core::{event::{EventKind,StreamDirection,StreamEventKind},log::BehaviorLog};
    let runs=tempfile::tempdir().unwrap();
    let rules=RuleSet::from_json(&json!([
        {"id":"page","match":{"url":"http://top.example.test/"},"action":{"type":"mock","status":200,
         "headers":{"content-type":"text/html"},"body":"<!doctype html><script>const s=new EventSource('https://api.example.test/feed');s.close();const w=new WebSocket('wss://api.example.test/feed');w.close()</script>"}},
        {"id":"sse","match":{"url":"https://api.example.test/feed"},"action":{"type":"stream_mock","messages":[]}},
        {"id":"ws","match":{"url":"wss://api.example.test/feed"},"action":{"type":"stream_mock","messages":[]}}
    ]).to_string()).unwrap();
    let mut options=support::options(runs.path(),"early-close","http://top.example.test/".into(),rules);
    options.passthrough_origins=Some(vec![]);
    let mut g=Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    g.advance(VirtualDuration(0)).await.unwrap();
    let native=attach(&g.endpoint()).await;
    assert_eq!(reports(&native).await["receipts"],json!([]),"closed-before-admission receipts must not accumulate");
    let events=BehaviorLog::read(&g.trace_path()).unwrap();
    let outgoing:Vec<_>=events.iter().filter_map(|event|match &event.kind {
        EventKind::Stream(stream) if stream.direction==StreamDirection::Out=>Some(stream),_=>None,
    }).collect();
    assert_eq!(outgoing.len(),2);
    assert!(outgoing.iter().all(|stream|stream.event==StreamEventKind::Close && stream.receipt.is_some()));
    assert_ne!(outgoing[0].stream,outgoing[1].stream);
    assert!(!events.iter().any(|event|matches!(&event.kind,EventKind::Stream(stream) if stream.event==StreamEventKind::Open)));
    g.stop().await.unwrap();
}
