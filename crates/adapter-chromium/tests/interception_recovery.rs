//! A stopped network request belongs to the generation, not its inspector session.
mod support;
use cdp::protocol::{fetch, target};
use frontcap_chromium::{Generation, session::Session};
use frontcap_core::{rules::RuleSet, time::VirtualDuration};
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

#[tokio::test]
async fn losing_the_controller_never_releases_a_held_request_or_its_callback() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    let received = Arc::new(AtomicUsize::new(0));
    let count = received.clone();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let body = if request.url() == "/held" {
                count.fetch_add(1, Ordering::SeqCst);
                "released"
            } else { "<!doctype html><p>Held request recovery</p>" };
            let _ = request.respond(tiny_http::Response::from_string(body)
                .with_header(tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap()));
        }
    });
    let rules = RuleSet::from_json(&format!(r#"[{{"id":"hold","match":{{"url":"http://{addr}/held"}},"action":{{"type":"hold"}}}}]"#)).unwrap();
    let runs = tempfile::tempdir().unwrap().keep();
    eprintln!("held request evidence (retained on failure): {}", runs.display());
    let detached = support::DetachedProfile::new();
    let mut options = support::options(&runs, "held-owner", format!("http://{addr}/index.html"), rules);
    detached.apply(&mut options);
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    g.eval_main("globalThis.completed=0;for(const body of ['first','second'])fetch('/held',{method:'POST',body}).then(r=>r.text()).then(()=>completed++);true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    assert_eq!(g.held_ordinals().len(), 2);
    assert_eq!(received.load(Ordering::SeqCst), 0);
    let before = g.eval_main("[performance.now(),globalThis.completed]").await.unwrap();
    assert_eq!(before, serde_json::json!([202, 0]));
    let endpoint = g.endpoint();
    g.abandon();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let client = cdp::Client::connect(&endpoint.browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(endpoint.target_id.clone()))
    }).await.unwrap();
    let native = Session::new(client, attached.session_id.0);
    let after = native.eval("[performance.now(),globalThis.completed]", None).await.unwrap();
    native.eval("fetch('/held',{method:'POST',body:'detached'}).then(r=>r.text()).then(()=>completed++);true", None).await.unwrap();
    native.client.close();
    assert_eq!((received.load(Ordering::SeqCst), after), (0, before), "controller loss must preserve both the network hold and the stopped callback state");

    let mut previous = None;
    for _ in 0..2 {
        let consumer = attach(&endpoint.browser_ws_url, &endpoint.target_id).await;
        let mut paused = consumer.subscribe::<fetch::RequestPausedEvent>();
        consumer.call(&patterns()).await.unwrap();
        let mut requests = Vec::new();
        for _ in 0..3 {
            let request = paused.next_timeout(std::time::Duration::from_secs(3)).await.unwrap()
                .expect("adoption must publish each retained request without advancing time");
            assert_eq!(request.request.url, format!("http://{addr}/held"));
            requests.push((request.request_id, serde_json::to_value(request.request).unwrap()));
        }
        assert_ne!(requests[0].0, requests[1].0, "identical URLs are distinct native requests");
        let bodies: std::collections::BTreeSet<_> = requests.iter().map(|(_, request)| request["postData"].as_str().unwrap()).collect();
        assert_eq!(bodies, ["detached", "first", "second"].into_iter().collect(), "requests created without a consumer must also be held and adopted");
        let rejected = consumer.call(&fetch::ContinueWithAuthParams::new(requests[0].0.clone(), fetch::AuthChallengeResponse {
            response: fetch::AuthChallengeResponseResponse::Default, username: None, password: None,
        })).await.unwrap_err();
        assert!(rejected.to_string().contains("not expected"), "{rejected}");
        if let Some(previous) = &previous {
            assert_eq!(&requests, previous, "adoption must retain identities, descriptor data and emission order");
        }
        previous = Some(requests);

        let competitor = attach(&endpoint.browser_ws_url, &endpoint.target_id).await;
        let error = competitor.call(&patterns()).await.unwrap_err();
        assert!(error.to_string().contains("already has a consumer"), "{error}");
        competitor.client.close();
        assert_eq!(consumer.eval("[performance.now(),globalThis.completed]", None).await.unwrap(), serde_json::json!([202, 0]));
        assert_eq!(received.load(Ordering::SeqCst), 0);
        consumer.client.close();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(received.load(Ordering::SeqCst), 0);
    std::fs::remove_dir_all(runs).unwrap();
}

async fn attach(browser_ws_url: &str, target_id: &str) -> Session {
    let client = cdp::Client::connect(browser_ws_url).await.unwrap();
    let attached = client.call(None, &target::AttachToTargetParams {
        flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(target_id.into()))
    }).await.unwrap();
    Session::new(client, attached.session_id.0)
}

fn patterns() -> fetch::EnableParams {
    fetch::EnableParams {
        patterns: Some(vec![fetch::RequestPattern {
            url_pattern: Some("*".into()),
            request_stage: Some(fetch::RequestStage::Request),
            ..Default::default()
        }]),
        ..Default::default()
    }
}

fn latency_rules() -> RuleSet {
    RuleSet::from_json(
        r#"[{"id":"slow","match":{"url":"https://api.example.test/slow"},
             "action":{"type":"mock","status":200,"headers":{"access-control-allow-origin":"*"},"body":"slow","latency_ms":500}}]"#,
    )
    .unwrap()
}

/// The binding of a delayed response is durable before the request stops holding virtual time, and names
/// the absolute instant of its answer; the answer's payload is recorded before it is sent.
#[tokio::test]
async fn bindings_precede_release_and_carry_the_deadline() {
    use frontcap_chromium::bindings::{BindingEntry, BindingJournal, Decision, Resolution};
    let addr = support::serve();
    let runs = tempfile::tempdir().unwrap();
    let mut g = Generation::launch(support::options(runs.path(), "bindings", format!("http://{addr}/latency.html"), latency_rules())).await.unwrap();
    let journal = runs.path().join("bindings").join("interceptions.jsonl");
    g.advance(VirtualDuration::from_ms(300)).await.unwrap();
    let entries = BindingJournal::read(&journal).unwrap();
    let bound: Vec<_> = entries.iter().filter_map(|e| match e { BindingEntry::Bound(b) if b.url.ends_with("/slow") => Some(b.clone()), _ => None }).collect();
    assert_eq!(bound.len(), 1, "{entries:?}");
    let b = &bound[0];
    assert_eq!(b.issued_us, 100_000);
    assert!(matches!(&b.decision, Decision::Deliver { due_us: 600_000, spec, .. } if spec.body == "slow"), "{:?}", b.decision);
    assert!(!b.fetch_id.is_empty() && b.network_id.is_some());
    assert_eq!(g.held_ordinals(), vec![b.ordinal.clone()], "bound and held under the same ordinal");
    assert!(!entries.iter().any(|e| matches!(e, BindingEntry::Dispatched { ordinal, .. } if *ordinal == b.ordinal)), "not answered yet");
    g.advance(VirtualDuration::from_ms(400)).await.unwrap();
    let dispatched: Vec<_> = BindingJournal::read(&journal).unwrap().into_iter().filter_map(|e| match e {
        BindingEntry::Dispatched { ordinal, fetch_id, resolution } if ordinal == b.ordinal => Some((fetch_id, resolution)),
        _ => None,
    }).collect();
    let digest = frontcap_core::canonical::sha256_bytes_hex(b"slow");
    assert_eq!(dispatched, vec![(b.fetch_id.clone(), Resolution::Fulfill { status: 200, body_sha256: digest })]);
    g.stop().await.unwrap();
}

/// Engine patch 24: each retained pause carries the virtual instant its request was issued, also for one
/// issued while no controller existed, and every pause the target ends leaves a receipt saying how.
#[tokio::test]
async fn retained_pauses_carry_their_issue_instant_and_resolution_receipts() {
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let addr = server.server_addr().to_ip().unwrap();
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let _ = request.respond(tiny_http::Response::from_string("<!doctype html><p>stamps</p>")
                .with_header(tiny_http::Header::from_bytes("Content-Type", "text/html").unwrap()));
        }
    });
    let rules = RuleSet::from_json(&format!(r#"[{{"id":"hold","match":{{"url":"http://{addr}/held"}},"action":{{"type":"hold"}}}}]"#)).unwrap();
    let runs = tempfile::tempdir().unwrap().keep();
    let detached = support::DetachedProfile::new();
    let mut options = support::options(&runs, "stamps", format!("http://{addr}/index.html"), rules);
    detached.apply(&mut options);
    let origin_us = options.profile.initial_date_ms * 1000;
    let mut g = Generation::launch(options).await.unwrap();
    g.advance(VirtualDuration::from_ms(201)).await.unwrap();
    g.eval_main("globalThis.done=[];fetch('/held',{method:'POST',body:'fulfil'}).then(r=>r.text()).then(t=>done.push(t),()=>done.push('fulfil failed'));true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    g.eval_main("fetch('/held',{method:'POST',body:'fail'}).then(()=>done.push('fail answered'),()=>done.push('fail failed'));true").await.unwrap();
    g.advance(VirtualDuration::from_ms(1)).await.unwrap();
    let endpoint = g.endpoint();
    g.abandon();
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let native = attach(&endpoint.browser_ws_url, &endpoint.target_id).await;
    native.eval("globalThis.ac=new AbortController();fetch('/held',{method:'POST',body:'abort',signal:ac.signal}).catch(()=>done.push('aborted'));true", None).await.unwrap();
    native.client.close();

    let consumer = attach(&endpoint.browser_ws_url, &endpoint.target_id).await;
    let mut paused = consumer.subscribe::<fetch::RequestPausedEvent>();
    consumer.call(&patterns()).await.unwrap();
    let mut by_body = std::collections::BTreeMap::new();
    for _ in 0..3 {
        let p = paused.next_timeout(std::time::Duration::from_secs(3)).await.unwrap().expect("retained pause");
        by_body.insert(p.request.post_data.clone().unwrap(), p);
    }
    let stamp = |body: &str| by_body[body].frontcap_issue_time_us.as_deref().map(|s| s.parse::<u64>().unwrap());
    assert_eq!(stamp("fulfil"), Some(origin_us + 201_000), "issued at 201 ms");
    assert_eq!(stamp("fail"), Some(origin_us + 202_000), "issued at 202 ms");
    assert_eq!(stamp("abort"), Some(origin_us + 203_000), "issued with no controller, at the frozen 203 ms");
    assert!(by_body.values().all(|p| p.network_id.is_some()), "every request has its DevTools identity, also one issued with no controller");

    let id = |body: &str| by_body[body].request_id.clone();
    consumer.call(&fetch::FulfillRequestParams { body: Some(cdp::Binary("b2s=".into())), ..fetch::FulfillRequestParams::new(id("fulfil"), 200) }).await.unwrap();
    consumer.call(&fetch::FailRequestParams::new(id("fail"), cdp::protocol::network::ErrorReason::Failed)).await.unwrap();
    consumer.eval("ac.abort();true", None).await.unwrap();
    let mut receipts = Vec::new();
    for _ in 0..50 {
        receipts = consumer.call(&fetch::FrontcapReceiptsParams::default()).await.unwrap().receipts;
        if receipts.len() == 3 { break; }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let outcome = |body: &str| receipts.iter().find(|r| r.request_id == id(body)).map(|r| (r.outcome, r.response_code, r.body_sha256.clone()));
    let ok = frontcap_core::canonical::sha256_bytes_hex(b"ok");
    assert_eq!(outcome("fulfil"), Some((fetch::FrontcapReceiptOutcome::Fulfilled, Some(200), Some(ok))));
    assert_eq!(outcome("fail"), Some((fetch::FrontcapReceiptOutcome::Failed, None, None)));
    assert_eq!(outcome("abort"), Some((fetch::FrontcapReceiptOutcome::Cancelled, None, None)));
    consumer.call(&fetch::FrontcapForgetReceiptsParams::new(vec![id("fulfil"), id("fail")])).await.unwrap();
    let left: Vec<_> = consumer.call(&fetch::FrontcapReceiptsParams::default()).await.unwrap().receipts.into_iter().map(|r| r.request_id).collect();
    assert_eq!(left, vec![id("abort")], "forgotten receipts are gone; others stay");
    consumer.client.close();
    identity_kill(&endpoint);
    std::fs::remove_dir_all(runs).unwrap();
}

fn identity_kill(endpoint: &frontcap_chromium::generation::Endpoint) {
    if let Ok(Some(identity)) = frontcap_core::process::ProcessIdentity::of(endpoint.browser_pid) {
        let _ = identity.kill();
    }
}
