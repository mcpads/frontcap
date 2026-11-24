mod support;

use frontcap_core::event::{ControllerChange, EventKind, SampleValue};
use frontcap_core::ids::{RequestId, WatchName};
use frontcap_core::log::BehaviorLog;
use frontcap_core::predicate::{Locator, Predicate};
use frontcap_core::rules::{MockSpec, RuleSet};
use frontcap_recorder_protocol::client::ClientError;
use frontcap_recorder_protocol::{Call, ErrorCode, Op, Outcome, Reply};

fn rid(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}

fn rules() -> RuleSet {
    RuleSet::from_json(
        r#"[
      {"id":"seq","match":{"url":"https://api.example.test/seq"},
       "action":{"type":"sequence","after_last":"fail","steps":[
         {"status":200,"headers":{"access-control-allow-origin":"*"},"body":"first","latency_ms":0},
         {"status":200,"headers":{"access-control-allow-origin":"*"},"body":"second","latency_ms":0}]}},
      {"id":"held","match":{"url":"https://api.example.test/held"},"action":{"type":"hold"}}]"#,
    )
    .unwrap()
}

#[tokio::test]
async fn controller_restart_keeps_holds_sequences_watches_and_numbering() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let spec = support::spec(&ws, "cr-1", &format!("http://{addr}/restart.html"), rules());
    let run = spec.run_id.clone();
    let (dir, mut a) = support::spawn_recorder(&ws, spec).await;
    let Reply::Attached { lease, .. } = a.call(Call::Reattach { request_id: rid("a-1") }).await.unwrap() else { panic!() };
    let seq2 = WatchName::new("seq2").unwrap();
    let added = a
        .call(Call::Mutate {
            lease: Some(lease.clone()),
            request_id: rid("m-1"),
            op: Op::WatchAdd {
                name: seq2.clone(),
                predicate: Predicate::TextEquals { target: Locator::Css { selector: "#seq2".into() }, text: "second".into() },
                context: None,
            },
        })
        .await
        .unwrap();
    assert!(matches!(added, Reply::Done { outcome: Outcome::Sample { .. }, .. }), "{added:?}");
    a.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("m-2"), op: Op::Advance { by_us: 500_000 } }).await.unwrap();
    let Reply::Status { status } = a.call(Call::Status).await.unwrap() else { panic!() };
    assert_eq!(status.held.len(), 1, "{status:?}");
    let held = status.held[0].clone();
    drop(a); // the controller dies; its lease expires with the connection

    let mut b = support::connect(&dir).await;
    let mut attached = None;
    for _ in 0..100 {
        match b.call(Call::Reattach { request_id: rid("b-1") }).await {
            Ok(Reply::Attached { lease, status, .. }) => {
                attached = Some((lease, status));
                break;
            }
            _ => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    }
    let (lease, status) = attached.expect("reattached");
    assert_eq!(status.held, vec![held.clone()]);
    assert_eq!(status.watches, vec![seq2.clone()]);
    let mut headers = std::collections::BTreeMap::new();
    headers.insert("access-control-allow-origin".to_string(), "*".to_string());
    let responded = b
        .call(Call::Mutate {
            lease: Some(lease.clone()),
            request_id: rid("m-3"),
            op: Op::Respond { ordinal: held, response: MockSpec { status: 200, headers, body: "answered".into(), body_base64: None, latency_ms: 0 } },
        })
        .await
        .unwrap();
    assert!(matches!(responded, Reply::Done { outcome: Outcome::Ack, .. }), "{responded:?}");
    let until = b
        .call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("m-4"), op: Op::Until { watch: seq2.clone(), max_us: 5_000_000 } })
        .await
        .unwrap();
    let Reply::Done { outcome: Outcome::Clock { result, t_us, .. }, .. } = until else { panic!("{until:?}") };
    assert_eq!(result, frontcap_core::event::ClockOutcome::ConditionMet { watch: seq2.clone() });
    assert!((3_000_000..3_100_000).contains(&t_us.0), "the second sequence step arrives after the 3000 ms timer: {t_us:?}");
    b.call(Call::Mutate { lease: Some(lease), request_id: rid("m-5"), op: Op::Stop }).await.unwrap();
    support::wait_until_exited(&dir).await;

    let bundle = frontcap_core::bundle::Bundle::open(&ws.runs(), &run).unwrap();
    let events = BehaviorLog::read(&bundle.trace()).unwrap();
    assert!(events.windows(2).all(|w| w[1].seq == w[0].seq + 1));
    let ordinals: Vec<_> = events.iter().filter_map(|e| match &e.kind { EventKind::Request(r) => Some(r.id.clone()), _ => None }).collect();
    let unique: std::collections::BTreeSet<_> = ordinals.iter().collect();
    assert_eq!(unique.len(), ordinals.len(), "{ordinals:?}");
    let changes: Vec<_> = events.iter().filter_map(|e| match &e.kind { EventKind::Controller(c) => Some(c.change), _ => None }).collect();
    assert!(changes.contains(&ControllerChange::Detached), "{changes:?}");
    assert!(changes.iter().filter(|c| **c == ControllerChange::Attached).count() >= 2, "{changes:?}");
    assert!(events.iter().any(|e| matches!(&e.kind, EventKind::Watch(s) if s.name == seq2 && s.value == SampleValue::True)));
}

#[tokio::test]
async fn repeated_request_id_is_answered_from_the_journal() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let (dir, mut a) = support::spawn_recorder(&ws, support::spec(&ws, "cr-2", &format!("http://{addr}/appear.html"), RuleSet::default())).await;
    let Reply::Attached { lease, .. } = a.call(Call::Reattach { request_id: rid("a-1") }).await.unwrap() else { panic!() };
    let call = || Call::Mutate { lease: Some(lease.clone()), request_id: rid("m-1"), op: Op::Advance { by_us: 100_000 } };
    let first = a.call(call()).await.unwrap();
    let again = a.call(call()).await.unwrap();
    assert_eq!(first, again);
    let Reply::Status { status } = a.call(Call::Status).await.unwrap() else { panic!() };
    assert_eq!(status.t_us.0, 100_000, "applied once");
    assert_eq!(status.last_completed_request_id, Some(rid("m-1")));
    let conflict = a.call(Call::Mutate { lease: Some(lease.clone()), request_id: rid("m-1"), op: Op::Advance { by_us: 1 } }).await;
    assert!(matches!(conflict, Err(ClientError::Recorder(ref e)) if e.code == ErrorCode::RequestIdConflict), "{conflict:?}");
    // A failed request is a definite outcome too, answered again from the journal.
    let missing = || Call::Mutate { lease: Some(lease.clone()), request_id: rid("m-2"), op: Op::Act { then: None, action: frontcap_core::action::Action::Click { target: Locator::Css { selector: "#nothing".into() } }, context: None } };
    let e1 = a.call(missing()).await.unwrap_err();
    let e2 = a.call(missing()).await.unwrap_err();
    assert_eq!(e1.to_string(), e2.to_string());
    drop(a);
    support::stop(&dir).await;
}

#[tokio::test]
async fn request_survives_controller_disconnect() {
    let addr = support::serve();
    let (_tmp, ws) = support::workspace();
    let (dir, mut a) = support::spawn_recorder(&ws, support::spec(&ws, "cr-3", &format!("http://{addr}/appear.html"), RuleSet::default())).await;
    let Reply::Attached { lease, .. } = a.call(Call::Reattach { request_id: rid("a-1") }).await.unwrap() else { panic!() };
    // The controller sends a long advance and dies without reading the answer.
    a.send_without_waiting(Call::Mutate { lease: Some(lease), request_id: rid("m-1"), op: Op::Advance { by_us: 2_000_000 } }).await.unwrap();
    drop(a);
    let mut b = support::connect(&dir).await;
    let mut done = false;
    for _ in 0..500 {
        let Reply::Status { status } = b.call(Call::Status).await.unwrap() else { panic!() };
        // The lost controller's lease expiry also runs held-input cleanup; reattach waits for it.
        if status.last_completed_request_id == Some(rid("m-1")) && status.running.is_none() && !status.input_cleanup_pending {
            assert_eq!(status.t_us.0, 2_000_000);
            done = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(done, "the request ran to completion without its controller");
    let Reply::Attached { completed_while_detached, .. } = b.call(Call::Reattach { request_id: rid("b-1") }).await.unwrap() else { panic!() };
    assert_eq!(completed_while_detached, vec![rid("m-1")]);
    drop(b);
    support::stop(&dir).await;
}
