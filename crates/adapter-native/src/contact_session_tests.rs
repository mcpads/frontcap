use super::*;
use frontcap_core::{
    native::{contacts::Contact, webview},
    process::ProcessIdentity,
};
fn fixture() -> (tempfile::TempDir, Session) {
    fixture_with_capacity(24)
}
fn fixture_with_capacity(request_capacity: u64) -> (tempfile::TempDir, Session) {
    fixture_with_tap(request_capacity, false)
}
fn fixture_with_tap(request_capacity: u64, short_tap: bool) -> (tempfile::TempDir, Session) {
    let root = tempfile::tempdir().unwrap();
    let native = root.path().join("native");
    std::fs::create_dir(&native).unwrap();
    let process = ProcessIdentity::current();
    let generation = "550e8400-e29b-41d4-a716-446655440000".to_owned();
    save_atomic(
        &native.join("endpoint.json"),
        &Endpoint {
            generation: generation.clone(),
            pid: process.pid,
            available: true,
            short_tap, observed_contacts: true,
            request_capacity,
        },
    )
    .unwrap();
    let owner = Owner {
        simulator: generation.clone(),
        process: process.clone(),
        endpoint: webview::Endpoint {
            generation,
            pid: process.pid,
            port: 1234,
        },
        attachment_sha256: "a".repeat(64),
    };
    let session = Session::create(&native, &root.path().join("evidence"), owner).unwrap();
    (root, session)
}
fn id(value: &str) -> RequestId {
    RequestId::new(value).unwrap()
}
fn set(revision: u64) -> Operation {
    Operation::Set {
        expected_revision: revision,
        contacts: vec![Contact {
            id: 1,
            x: 0.5,
            y: 0.25,
        }],
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_millis(25)
}
fn receipt(session: &Session, number: usize, contacts: serde_json::Value) {
    let request = read(&session.directory.join(format!("request-{number}.json"))).unwrap();
    save_atomic(
        &session.directory.join(format!("reply-{number}.json")),
        &serde_json::json!({
            "generation":session.state.endpoint.generation,"request_id":number,"status":"enqueued",
            "contact_revision":request["expected_revision"].as_u64().unwrap()+1,"contacts":contacts,
            "changed":true,"mach_timestamp":100,"request":request
        }),
    )
    .unwrap();
}
#[test]
fn lost_contact_reply_blocks_new_input_and_recovers_without_republication() {
    let (_root, mut s) = fixture();
    let cancelled = AtomicBool::new(false);
    assert!(
        s.execute(id("press"), set(0), deadline(), &cancelled)
            .is_err()
    );
    // Native publication owns its inode, even when evidence happens to share a filesystem.
    use std::os::unix::fs::MetadataExt;
    let native = s.directory.join("request-1.json");
    let retained = s.evidence.join("request-1.json");
    assert_eq!(std::fs::read(&native).unwrap(), std::fs::read(&retained).unwrap());
    let a = std::fs::metadata(&native).unwrap();
    let b = std::fs::metadata(&retained).unwrap();
    assert_ne!((a.dev(), a.ino()), (b.dev(), b.ino()));
    assert!(!s.directory.join(".frontcap-request-1.pending").exists());
    assert!(
        s.execute(id("other"), set(0), deadline(), &cancelled)
            .is_err()
    );
    assert!(!s.directory.join("request-2.json").exists());
    receipt(&s, 1, serde_json::json!([{"id":1,"x":0.5,"y":0.25}]));
    // Deletion makes any redispatch visible. Recovery must only read the retained reply.
    std::fs::remove_file(s.directory.join("request-1.json")).unwrap();
    let answer = s
        .execute(id("press"), set(0), deadline(), &cancelled)
        .unwrap();
    assert!(matches!(answer, Receipt::Enqueued { revision: 1, .. }));
    assert!(!s.directory.join("request-1.json").exists());
    assert!(
        s.execute(
            id("press"),
            Operation::Cancel {
                expected_revision: 0
            },
            deadline(),
            &cancelled
        )
        .is_err()
    );
    assert!(
        s.execute(id("stale"), set(0), deadline(), &cancelled)
            .is_err()
    );
    let cancel = Operation::Cancel {
        expected_revision: 1,
    };
    assert!(
        s.execute(id("cancel"), cancel.clone(), deadline(), &cancelled)
            .is_err()
    );
    receipt(&s, 2, serde_json::json!([]));
    assert!(
        matches!(s.execute(id("cancel"),cancel,deadline(),&cancelled).unwrap(), Receipt::Enqueued {revision:2,contacts,..} if contacts.is_empty())
    );
    std::fs::remove_dir_all(&s.directory).unwrap();
    assert_eq!(
        s.execute(id("press"), set(0), deadline(), &cancelled)
            .unwrap(),
        answer
    );
}
#[test]
fn contact_publication_preserves_existing_native_request_and_never_retries_dispatch() {
    let (_root, mut s) = fixture();
    let path = s.directory.join("request-1.json");
    std::fs::write(&path, b"foreign request").unwrap();
    let error = s.execute(id("press"), set(0), deadline(), &AtomicBool::new(false)).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(&path).unwrap(), b"foreign request");
    assert!(!s.directory.join(".frontcap-request-1.pending").exists());
    std::fs::remove_file(&path).unwrap();
    assert!(s.execute(id("press"), set(0), deadline(), &AtomicBool::new(false)).is_err());
    assert!(!path.exists());
    assert!(!s.directory.join("request-2.json").exists());
}
#[test]
fn contact_admission_rejects_invalid_state_and_preserves_foreign_or_unknown_receipts() {
    let (_root, mut s) = fixture();
    let cancelled = AtomicBool::new(false);
    assert!(
        Session::create(
            &s.directory,
            &_root.path().join("second"),
            s.state.owner.clone()
        )
        .is_err()
    );
    let bad_op = Operation::Set {
        expected_revision: 0,
        contacts: vec![Contact {
            id: 1,
            x: f64::NAN,
            y: 0.2,
        }],
    };
    assert!(
        s.execute(id("bad"), bad_op, deadline(), &cancelled)
            .is_err()
    );
    assert!(!s.directory.join("request-1.json").exists());
    assert!(
        s.execute(id("press"), set(0), deadline(), &cancelled)
            .is_err()
    );
    for value in [
        serde_json::json!({"generation":"foreign","request_id":1,"status":"rejected","error":"invalid"}),
        serde_json::json!({"generation":s.state.endpoint.generation,"request_id":1,"status":"unknown"}),
    ] {
        save_atomic(&s.directory.join("reply-1.json"), &value).unwrap();
        assert!(
            s.execute(id("press"), set(0), deadline(), &cancelled)
                .is_err()
        );
        assert_eq!(read(&s.evidence.join("reply-1.json")).unwrap(), value);
        assert!(
            s.execute(id("replacement"), set(0), deadline(), &cancelled)
                .is_err()
        );
    }
    assert!(!s.directory.join("request-2.json").exists());
}

#[test]
fn observed_contacts_bind_window_and_keep_native_rejection_distinct_from_unknown() {
    let (_root, mut s) = fixture();
    let cancelled = AtomicBool::new(false);
    let mut window = Window {
        generation: s.state.endpoint.generation.clone(),
        pid: s.state.endpoint.pid,
        token: "foreground-1".into(),
        width: 100.0,
        height: 200.0,
        scale: 2.0,
        fullscreen: true,
    };
    save_atomic(&s.directory.join("window.json"), &window).unwrap();
    assert_eq!(s.window().unwrap(), window);
    let original = window.clone();
    window.token = "replacement".into();
    save_atomic(&s.directory.join("window.json"), &window).unwrap();
    assert!(
        s.execute_observed(id("moved"), set(0), original, deadline(), &cancelled)
            .is_err()
    );
    assert!(!s.directory.join("request-1.json").exists());
    assert!(
        s.execute_observed(id("press"), set(0), window.clone(), deadline(), &cancelled)
            .is_err()
    );
    let request = read(&s.directory.join("request-1.json")).unwrap();
    assert_eq!(
        request["expected_window"],
        serde_json::to_value(&window).unwrap()
    );
    save_atomic(
        &s.directory.join("reply-1.json"),
        &serde_json::json!({"generation":s.state.endpoint.generation,
        "request_id":1,"status":"rejected","error":"observation_window_changed"}),
    )
    .unwrap();
    assert!(
        matches!(s.reconcile(&id("press"),deadline(),&cancelled).unwrap(),Receipt::Rejected {reason} if reason=="observation_window_changed")
    );
    window.fullscreen = false;
    save_atomic(&s.directory.join("window.json"), &window).unwrap();
    assert!(s.window().is_err());
}

#[test]
fn native_echo_normalizes_numbers_without_accepting_changed_contact_values() {
    let (_root, mut s) = fixture();
    let cancelled = AtomicBool::new(false);
    let operation = Operation::Set {
        expected_revision: 0,
        contacts: vec![Contact {
            id: 1,
            x: 1.0,
            y: 0.0,
        }],
    };
    assert!(
        s.execute(id("edge"), operation, deadline(), &cancelled)
            .is_err()
    );
    receipt(&s, 1, serde_json::json!([{"id":1,"x":1,"y":0}]));
    let path = s.directory.join("reply-1.json");
    let mut raw = read(&path).unwrap();
    raw["request"]["contacts"][0]["x"] = serde_json::json!(0);
    raw["request"]["contacts"][0]["y"] = serde_json::json!(0);
    save_atomic(&path, &raw).unwrap();
    assert!(s.reconcile(&id("edge"), deadline(), &cancelled).is_err());
    raw["request"]["contacts"][0]["x"] = serde_json::json!(1);
    save_atomic(&path, &raw).unwrap();
    assert!(matches!(
        s.reconcile(&id("edge"), deadline(), &cancelled).unwrap(),
        Receipt::Enqueued { revision: 1, .. }
    ));
}

#[test]
fn advertised_capacity_allows_longer_sessions_and_preserves_replay_at_exhaustion() {
    let (_root, mut session) = fixture_with_capacity(26);
    let cancelled = AtomicBool::new(false);
    assert_eq!(session.admission(), Some((0, 26)));
    for revision in 0..26 {
        let identity = id(&format!("input-{revision}"));
        let operation = Operation::Set {
            expected_revision: revision,
            contacts: vec![],
        };
        // Publication has no native fixture responder until the retained reply is supplied.
        assert!(
            session
                .execute(
                    identity.clone(),
                    operation.clone(),
                    Instant::now() + Duration::from_millis(500),
                    &cancelled
                )
                .is_err()
        );
        receipt(&session, revision as usize + 1, serde_json::json!([]));
        session
            .execute(
                identity,
                operation,
                Instant::now() + Duration::from_millis(500),
                &cancelled,
            )
            .unwrap();
    }
    assert_eq!(session.admission(), None);
    assert!(
        session
            .execute(
                id("over-capacity"),
                Operation::Cancel {
                    expected_revision: 26
                },
                Instant::now() + Duration::from_millis(500),
                &cancelled
            )
            .is_err()
    );
    assert!(!session.directory.join("request-27.json").exists());
    std::fs::remove_file(session.directory.join("request-1.json")).unwrap();
    assert!(matches!(
        session
            .execute(
                id("input-0"),
                Operation::Set {
                    expected_revision: 0,
                    contacts: vec![]
                },
                Instant::now() + Duration::from_millis(500),
                &cancelled
            )
            .unwrap(),
        Receipt::Enqueued { revision: 1, .. }
    ));
    assert!(!session.directory.join("request-1.json").exists());
}

#[test]
fn legacy_capacity_defaults_and_invalid_advertisements_are_rejected() {
    let value = serde_json::json!({"generation":"550e8400-e29b-41d4-a716-446655440000","pid":1,"available":true});
    let endpoint: Endpoint = serde_json::from_value(value).unwrap();
    assert_eq!(endpoint.request_capacity, 24);
    endpoint.validate(1).unwrap();
    for request_capacity in [0, frontcap_core::native::contacts::MAX_REQUEST_CAPACITY + 1] {
        assert!(
            Endpoint {
                request_capacity,
                ..endpoint.clone()
            }
            .validate(1)
            .is_err()
        );
    }
}

#[test]
fn interruption_requires_a_new_confirmed_cancel_and_never_replays_uncertain_reset() {
    let (_root, mut session) = fixture();
    let cancelled = AtomicBool::new(false);
    session.interrupt().unwrap();
    assert!(!session.requires_cancel());
    assert!(session.execute(id("held"), set(0), deadline(), &cancelled).is_err());
    receipt(&session, 1, serde_json::json!([{"id":1,"x":0.5,"y":0.25}]));
    session.reconcile(&id("held"), deadline(), &cancelled).unwrap();
    assert!(session.execute(id("old-cancel"), Operation::Cancel { expected_revision: 1 }, deadline(), &cancelled).is_err());
    session.interrupt().unwrap();
    assert!(session.requires_cancel());
    let saved: serde_json::Value = frontcap_core::state::load(&session.evidence.join("contacts.json")).unwrap().unwrap();
    assert_eq!(saved["cancel_after"], 2);
    receipt(&session, 2, serde_json::json!([]));
    session.reconcile(&id("old-cancel"), deadline(), &cancelled).unwrap();
    assert!(session.requires_cancel(), "pre-interruption cancel cannot release the fence");
    assert!(session.execute(id("new-set"), set(2), deadline(), &cancelled).is_err());
    assert!(!session.directory.join("request-3.json").exists());
    assert!(session.execute(id("rejected-reset"), Operation::Cancel { expected_revision: 2 }, deadline(), &cancelled).is_err());
    save_atomic(&session.directory.join("reply-3.json"), &serde_json::json!({"generation":session.state.endpoint.generation,"request_id":3,"status":"rejected","error":"fixture rejection"})).unwrap();
    session.reconcile(&id("rejected-reset"), deadline(), &cancelled).unwrap();
    assert!(session.requires_cancel());
    let reset = Operation::Cancel { expected_revision: 2 };
    assert!(session.execute(id("reset"), reset.clone(), deadline(), &cancelled).is_err());
    let original = std::fs::read(session.directory.join("request-4.json")).unwrap();
    save_atomic(&session.directory.join("reply-4.json"), &serde_json::json!({"generation":session.state.endpoint.generation,"request_id":4,"status":"executing"})).unwrap();
    assert!(session.execute(id("reset"), reset, deadline(), &cancelled).is_err());
    assert_eq!(std::fs::read(session.directory.join("request-4.json")).unwrap(), original);
    assert!(session.requires_cancel());
    let progress = session.inspect();
    assert_eq!(progress.pending.as_ref().unwrap().id, id("reset"));
    assert_eq!(progress.pending.as_ref().unwrap().native_request_id, 4);
    assert_eq!(session.inspect(), progress);
    assert_eq!(std::fs::read(session.directory.join("request-4.json")).unwrap(), original);
    receipt(&session, 4, serde_json::json!([]));
    session.reconcile(&id("reset"), deadline(), &cancelled).unwrap();
    assert!(!session.requires_cancel());
    assert!(session.inspect().pending.is_none());
    assert_eq!(session.inspect().last_completed.unwrap().id, id("reset"));
    session.validate_new_input(&set(3)).unwrap();
    let saved: serde_json::Value = frontcap_core::state::load(&session.evidence.join("contacts.json")).unwrap().unwrap();
    assert!(saved["cancel_after"].is_null());
}

#[test]
fn native_host_consumer_needs_no_page_endpoint_and_rejects_malformed_page_owners() {
    let owner=frontcap_core::native::ios_host::Owner { simulator:"550e8400-e29b-41d4-a716-446655440000".into(),process:ProcessIdentity::current(),attachment_sha256:"a".repeat(64) };
    let value=serde_json::to_value(&owner).unwrap();
    let consumer:Consumer=serde_json::from_value(value.clone()).unwrap();
    assert!(matches!(&consumer,Consumer::Host(_)));consumer.validate().unwrap();
    assert_eq!(consumer.process(),&owner.process);
    let root=tempfile::tempdir().unwrap();let native=root.path().join("native");std::fs::create_dir(&native).unwrap();
    save_atomic(&native.join("endpoint.json"),&Endpoint {generation:owner.simulator.clone(),pid:owner.process.pid,available:true,short_tap: false, observed_contacts:true,request_capacity:24}).unwrap();
    let session=Session::create(&native,&root.path().join("evidence"),owner).unwrap();
    assert_eq!(serde_json::to_value(&session.state.owner).unwrap(),value);
    let mut malformed=value;malformed["endpoint"]=serde_json::json!({"pid":1});
    assert!(serde_json::from_value::<Consumer>(malformed).is_err());
}

#[test]
fn short_tap_rejects_unsupported_producer_without_publishing_input() {
    let (_root,mut session)=fixture();
    let operation=Operation::Tap{expected_revision:0,x:0.5,y:0.25};
    assert!(session.validate_new_input(&operation).is_err());
    let error=session.execute(id("tap"),operation,deadline(),&AtomicBool::new(false)).unwrap_err();
    assert!(error.to_string().contains("unavailable"));
    assert!(session.state.entries.is_empty());
    assert!(!session.directory.join("request-1.json").exists());
}

#[test]
fn short_tap_requires_bounded_pair_and_recovers_original_without_republication() {
    let (_root,mut s)=fixture_with_tap(24,true);
    let cancelled=AtomicBool::new(false);
    let operation=Operation::Tap{expected_revision:0,x:0.5,y:0.25};
    assert!(s.execute(id("unobserved"),operation.clone(),deadline(),&cancelled).is_err());
    assert!(s.state.entries.is_empty());
    let window=Window{generation:s.endpoint().generation.clone(),pid:s.endpoint().pid,
        token:"foreground".into(),width:100.0,height:200.0,scale:2.0,fullscreen:true};
    save_atomic(&s.directory.join("window.json"),&window).unwrap();
    assert!(s.execute_observed(id("tap"),operation.clone(),window.clone(),deadline(),&cancelled).is_err());
    let request=read(&s.directory.join("request-1.json")).unwrap();
    let frame=|down,at|serde_json::json!({"id":1,"x":50,"y":50,"down":down,"enqueued":true,"mach_timestamp":at});
    let valid=serde_json::json!({"generation":s.endpoint().generation,"request_id":1,"request":request,
        "status":"enqueued","contact_revision":1,"contacts":[],"changed":true,"mach_timestamp":200_000_000,
        "tap":{"timebase_numer":1,"timebase_denom":1,"frames":[frame(true,1000),frame(false,20_001_000)]}});
    let mut missing=valid.clone();missing.as_object_mut().unwrap().remove("tap");
    let mut partial=valid.clone();partial["tap"]["frames"]=serde_json::json!([frame(true,1000)]);
    let mut late=valid.clone();late["tap"]["frames"][1]["mach_timestamp"]=100_001_001u64.into();
    let mut backward=valid.clone();backward["tap"]["frames"][1]["mach_timestamp"]=999.into();
    let mut phase=valid.clone();phase["tap"]["frames"][1]["down"]=true.into();
    let mut foreign=valid.clone();foreign["tap"]["frames"][1]["x"]=51.into();
    let mut zero=valid.clone();zero["tap"]["timebase_denom"]=0.into();
    let mut not_sent=valid.clone();not_sent["tap"]["frames"][1]["enqueued"]=false.into();
    let mut unknown=valid.clone();unknown["status"]="unknown".into();
    for raw in [missing,partial,late,backward,phase,foreign,zero,not_sent,unknown] {
        save_atomic(&s.directory.join("reply-1.json"),&raw).unwrap();
        assert!(s.reconcile(&id("tap"),deadline(),&cancelled).is_err());
        assert!(s.state.entries[0].receipt.is_none());
        assert!(s.validate_new_input(&operation).is_err());
        assert!(!s.directory.join("request-2.json").exists());
    }
    save_atomic(&s.directory.join("reply-1.json"),&valid).unwrap();
    let original=s.reconcile(&id("tap"),deadline(),&cancelled).unwrap();
    assert!(matches!(&original,Receipt::Enqueued{tap:Some(_),revision:1,contacts,..} if contacts.is_empty()));
    std::fs::remove_file(s.directory.join("request-1.json")).unwrap();
    assert_eq!(s.execute_observed(id("tap"),operation,window,deadline(),&cancelled).unwrap(),original);
    assert!(!s.directory.join("request-1.json").exists());
    assert!(s.execute(id("held"),set(1),deadline(),&cancelled).is_err());
    receipt(&s,2,serde_json::json!([{"id":1,"x":0.5,"y":0.25}]));
    s.reconcile(&id("held"),deadline(),&cancelled).unwrap();
    assert!(s.validate_new_input(&Operation::Tap{expected_revision:2,x:0.5,y:0.25}).is_err());
    assert!(!s.directory.join("request-3.json").exists());
}
