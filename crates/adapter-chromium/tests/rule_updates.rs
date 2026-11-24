mod support;
use frontcap_chromium::{ChromiumError, generation::{Generation, ReconnectOptions}};
use frontcap_core::{event::EventKind, ids::{RequestId, RunId}, log::BehaviorLog, predicate::WatchRegistry, rules::{MockSpec, RuleSet}, time::VirtualDuration};

fn rules(action: serde_json::Value) -> RuleSet {
    RuleSet::from_json(&serde_json::json!([{"id":"api","match":{"url":"https://api.example.test/*"},"action":action}]).to_string()).unwrap()
}

#[tokio::test]
async fn published_replacement_keeps_old_holds_and_recovers_before_future_requests() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let detached = support::DetachedProfile::new();
    let mut options = support::options(runs.path(), "rule-replacement", format!("http://{addr}/fetch.html"), rules(serde_json::json!({"type":"hold"})));
    detached.apply(&mut options);
    let profile = options.profile.clone();
    let mut g = Generation::launch(options).await.unwrap();
    let launch_id = g.launch_id().clone();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    g.eval_main("go(['https://api.example.test/old'])").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let held = g.held(); assert_eq!(held.len(), 1);
    let before = g.network_rules().await.unwrap();
    let replacement = rules(serde_json::json!({"type":"mock","status":200,"body":"new","latency_ms":20}));
    let request = RequestId::new("install").unwrap();
    let origins = vec![format!("http://{addr}")];
    let receipt = g.replace_network_rules(&request, before.revision, &before.boundary, replacement.clone(), &origins).await.unwrap();
    assert_eq!(receipt.revision, 1);
    assert_eq!(g.now(), before.boundary.t_us);
    assert_eq!(g.held(), held);
    assert_eq!(g.held_requests()[0].1.rules_revision, 0, "old hold retains its selected installation");
    assert_eq!(g.replace_network_rules(&request, before.revision, &before.boundary, replacement.clone(), &origins).await.unwrap(), receipt);
    assert!(matches!(g.replace_network_rules(&RequestId::new("stale").unwrap(), before.revision, &before.boundary, replacement.clone(), &origins).await, Err(ChromiumError::ObservationChanged(_))));
    let endpoint = g.endpoint(); g.abandon();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let (mut g, _) = Generation::reconnect(ReconnectOptions {
        repo_root: support::repo_root(), runs_root: runs.path().to_path_buf(), run: RunId::new("rule-replacement").unwrap(),
        launch_id, profile, endpoint, watches: WatchRegistry::default(), watchdog: std::time::Duration::from_secs(10),
        granted_until: None, dialog_rules: Default::default(), upstream: None,
    }).await.unwrap();
    assert_eq!(g.network_rules().await.unwrap().revision, 1);
    assert_eq!(g.rule_receipt(&request).unwrap(), Some(receipt));
    assert_eq!(g.held(), held);
    assert_eq!(g.held_requests()[0].1.rules_revision, 0, "old hold retains its selected installation");
    g.eval_main("go(['https://api.example.test/new'])").await.unwrap();
    g.advance(VirtualDuration::from_ms(25)).await.unwrap();
    assert_eq!(g.held(), held, "replacement does not answer the old hold");
    let entries = g.eval_main("window.__log").await.unwrap();
    let new = entries.as_array().unwrap().iter().find(|e| e[0]=="done" && e[1]=="https://api.example.test/new").unwrap();
    assert_eq!(new[3], "new");
    assert_eq!(new[4].as_u64().unwrap()*1000, before.boundary.t_us.0+20_000);
    g.respond(&held[0], MockSpec { status: 201, headers: Default::default(), body: "old".into(), body_base64: None, latency_ms: 0 }).await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let entries = g.eval_main("window.__log").await.unwrap();
    assert!(entries.as_array().unwrap().iter().any(|e|e[0]=="done" && e[1]=="https://api.example.test/old" && e[2]==201 && e[3]=="old"));
    let trace = BehaviorLog::read(&g.trace_path()).unwrap();
    let requests: Vec<_> = trace.iter().filter_map(|e|match &e.kind {
        EventKind::Request(r) if r.url.starts_with("https://api.example.test/") => Some((r.url.as_str(), r.rules_revision)), _ => None,
    }).collect();
    assert_eq!(requests, vec![("https://api.example.test/old",0),("https://api.example.test/new",1)]);
    assert_eq!(trace.iter().filter(|e|matches!(e.kind, EventKind::RulesInstalled(_))).count(),2);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn invalid_inputs_do_not_publish_or_change_active_rules() {
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "rule-refusal", format!("http://{addr}/fetch.html"), RuleSet::default())).await.unwrap();
    g.advance(VirtualDuration::from_ms(100)).await.unwrap();
    let before = g.network_rules().await.unwrap();
    let origins = vec![format!("http://{addr}")];
    let mut duplicate = rules(serde_json::json!({"type":"hold"}));
    duplicate.rules.push(duplicate.rules[0].clone());
    assert!(g.replace_network_rules(&RequestId::new("duplicate").unwrap(), 0, &before.boundary, duplicate, &origins).await.is_err());
    let missing = rules(serde_json::json!({"type":"replay","cassette":runs.path().join("missing.jsonl"),"after_last":"fail"}));
    assert!(g.replace_network_rules(&RequestId::new("missing").unwrap(), 0, &before.boundary, missing, &origins).await.is_err());
    assert_eq!(g.network_rules().await.unwrap(), before);
    assert!(!runs.path().join("rule-refusal/rules/0001.json").exists());
    assert_eq!(BehaviorLog::read(&g.trace_path()).unwrap().iter().filter(|e|matches!(e.kind, EventKind::RulesInstalled(_))).count(), 1);
    assert!(g.rule_receipt(&RequestId::new("missing").unwrap()).unwrap().is_none());
    let accepted = g.replace_network_rules(&RequestId::new("valid").unwrap(), 0, &before.boundary, RuleSet::default(), &origins).await.unwrap();
    assert_eq!(accepted.revision, 1);
    g.stop().await.unwrap();
}

#[tokio::test]
async fn old_stream_schedule_survives_replacement_and_recorder_loss() {
    for replay in [false, true] {
        let addr = support::serve();
        let runs = tempfile::tempdir().unwrap();
        let detached = support::DetachedProfile::new();
        let cassette = runs.path().join("stream.jsonl");
        let url = "https://api.example.test/feed";
        let events = [(0,"open",None),(10_000,"message",Some("old-first")),(100_000,"message",Some("old-second"))];
        let text = events.into_iter().map(|(at_us,event,data)| frontcap_core::cassette::StreamEvent {
            key: frontcap_core::cassette::Key::of("GET",url,None), session:0, at_us,
            event:event.into(), data:data.map(str::to_string), sse_event:None, sse_id:None, code:None,
        }.to_line()+"\n").collect::<String>();
        std::fs::write(&cassette,text).unwrap();
        let action = if replay { serde_json::json!({"type":"replay","cassette":cassette,"after_last":"fail"}) }
            else { serde_json::json!({"type":"stream_mock","messages":[{"at_ms":10,"data":"old-first"},{"at_ms":100,"data":"old-second"}]}) };
        let mut options = support::options(runs.path(), "stream-replacement", format!("http://{addr}/stream2.html"), rules(action));
        detached.apply(&mut options);
        let profile = options.profile.clone();
        let mut g = Generation::launch(options).await.unwrap();
        let launch_id = g.launch_id().clone();
        g.advance(VirtualDuration::from_ms(20)).await.unwrap();
        let open = "(()=>{const s=new EventSource('https://api.example.test/feed');s.onmessage=e=>log.push([e.data,Math.round(performance.now())]);return 0})()";
        g.eval_main(open).await.unwrap();
        g.advance(VirtualDuration::from_ms(20)).await.unwrap();
        assert_eq!(g.eval_main("log").await.unwrap(),serde_json::json!([["old-first",30]]));
        let before = g.network_rules().await.unwrap();
        let replacement = rules(serde_json::json!({"type":"stream_mock","messages":[{"at_ms":100,"data":"new"}]}));
        g.replace_network_rules(&RequestId::new("replace-stream").unwrap(),0,&before.boundary,replacement,&[format!("http://{addr}")]).await.unwrap();
        let endpoint = g.endpoint(); g.abandon();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (mut g, _) = Generation::reconnect(ReconnectOptions {
            repo_root:support::repo_root(), runs_root:runs.path().to_path_buf(), run:RunId::new("stream-replacement").unwrap(),
            launch_id,profile,endpoint,watches:WatchRegistry::default(),watchdog:std::time::Duration::from_secs(10),
            granted_until:None,dialog_rules:Default::default(),upstream:None,
        }).await.unwrap();
        assert_eq!(g.now(),before.boundary.t_us);
        g.advance(VirtualDuration::from_ms(20)).await.unwrap();
        g.eval_main(open).await.unwrap();
        g.advance(VirtualDuration::from_ms(120)).await.unwrap();
        assert_eq!(g.eval_main("log").await.unwrap(),serde_json::json!([["old-first",30],["old-second",120],["new",160]]),"replay={replay}");
        g.stop().await.unwrap();
    }
}
