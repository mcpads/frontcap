use super::*;
use crate::webview::Endpoint;
use crate::webview::{Progress, State as PageState};
use frontcap_core::process::ProcessIdentity;
fn id(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}
fn owner() -> Owner {
    Owner {
        simulator: "23357f73-a68f-4271-a765-c8fb5538acc3".into(),
        process: ProcessIdentity {
            pid: 42,
            started: "frozen-start".into(),
        },
        endpoint: Endpoint {
            pid: 42,
            port: 12345,
            generation: "87c86105-02ad-4d07-bc0c-58c922960225".into(),
        },
        attachment_sha256: "a".repeat(64),
    }
}
fn state(ms: f64) -> PageState {
    PageState {
        timer_queue: Some(serde_json::from_value(serde_json::json!({"timers":[
            {"id":1000000000000_u64,"kind":"timeout","dueMs":ms+20.0}],"omitted":0,"failure":null})).unwrap()),
        frames: None,
        elapsed_ms: ms,
        performance_ms: ms,
        date_ms: 1700000000000.0 + ms,
        advancing: false,
        failure: None,
        pending_timers: 1,
        cancelled_on_failure: 0,
    }
}
fn reply(request: ClockRequest, before: u64) -> Reply {
    let now = before + request.delta_ns;
    let advancing = request.operation == ClockOperation::Advance;
    Reply {
        observation: Default::default(),
        generation: request.generation.clone(),
        pid: 42,
        port: 12345,
        last_id: request.request_id,
        now_ns: now,
        replayed: false,
        status: Status::Ok,
        applied: Some(advancing),
        document: Some("23357f73-a68f-4271-a765-c8fb5538acc3".into()),
        state: Some(state(now as f64 / 1e6)),
        progress: advancing.then(|| Progress {
            before: state(before as f64 / 1e6),
            requested_ms: now as f64 / 1e6,
            after: state(now as f64 / 1e6),
        }),
        error: None,
        request,
    }
}
fn ready(store: &mut RequestStore) {
    store.prepare(id("ready"), ClockOperation::Read, 0).unwrap();
    let request = store.dispatch(&id("ready")).unwrap();
    store.complete(&id("ready"), reply(request, 0)).unwrap();
}
#[test]
fn reopening_preserves_uncertain_request_and_exact_receipt_without_redispatch() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("store");
    let mut store = RequestStore::create(&dir, owner()).unwrap();
    ready(&mut store);
    store
        .prepare(id("step"), ClockOperation::Advance, 50_000_000)
        .unwrap();
    let prepared = store.inspect();
    assert_eq!(prepared.pending.unwrap().dispatch, frontcap_core::native::webview_progress::Dispatch::Prepared);
    let request = store.dispatch(&id("step")).unwrap();
    let outstanding = store.inspect();
    assert_eq!(outstanding.pending.as_ref().unwrap().dispatch, frontcap_core::native::webview_progress::Dispatch::Unresolved);
    assert_eq!(store.inspect(), outstanding, "inspection cannot consume pending work");
    drop(store);
    let mut store = RequestStore::open(&dir, &owner()).unwrap();
    assert!(matches!(
        store.recovery(&id("step")).unwrap(),
        Recovery::Reconcile { .. }
    ));
    assert!(matches!(
        store.execute(
            id("step"),
            ClockOperation::Advance,
            50_000_000,
            Instant::now()
        ),
        Err(StoreError::AlreadyDispatched)
    ));
    assert!(matches!(
        store.prepare(id("another"), ClockOperation::Read, 0),
        Err(StoreError::Pending)
    ));
    assert!(matches!(
        store.prepare(id("step"), ClockOperation::Advance, 1),
        Err(StoreError::Conflict)
    ));
    let mut receipt = reply(request, 0);
    receipt.observation = serde_json::from_value(serde_json::json!({
        "visibility":"hidden",
        "events":{"received":9,"dropped":8,"events":[
            {"sequence":9,"elapsedMs":50,"kind":"visibility","state":"hidden"}]},
        "pointers":{"received":0,"dropped":0,"active":0,"maximumActive":0,"events":[]}
    }))
    .unwrap();
    store.complete(&id("step"), receipt.clone()).unwrap();
    drop(store);
    let mut store = RequestStore::open(&dir, &owner()).unwrap();
    // Even with a nonexistent target and expired deadline, completed lookup is offline.
    assert_eq!(
        store
            .execute(
                id("step"),
                ClockOperation::Advance,
                50_000_000,
                Instant::now()
            )
            .unwrap(),
        receipt
    );
    let inspected = store.inspect();
    assert!(inspected.pending.is_none());
    assert_eq!(inspected.last_completed.unwrap().reply, receipt);
    store.prepare(id("read"), ClockOperation::Read, 0).unwrap();
    let request = store.dispatch(&id("read")).unwrap();
    assert!(
        store
            .complete(&id("read"), reply(request.clone(), 0))
            .is_err()
    );
    store
        .complete(&id("read"), reply(request, 50_000_000))
        .unwrap();
}
#[test]
fn ownership_readiness_and_unknown_outcome_fence_new_admission() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("store");
    let mut store = RequestStore::create(&dir, owner()).unwrap();
    assert!(matches!(
        RequestStore::open(&dir, &owner()),
        Err(StoreError::Busy)
    ));
    assert!(
        store
            .prepare(id("early"), ClockOperation::Advance, 1)
            .is_err()
    );
    ready(&mut store);
    store
        .prepare(id("step"), ClockOperation::Advance, 50_000_000)
        .unwrap();
    let request = store.dispatch(&id("step")).unwrap();
    let mut receipt = reply(request, 0);
    receipt.document = Some("87c86105-02ad-4d07-bc0c-58c922960225".into());
    assert!(store.complete(&id("step"), receipt.clone()).is_err());
    receipt.status = Status::Unknown;
    receipt.applied = None;
    receipt.error = Some("completion timeout".into());
    store.complete(&id("step"), receipt).unwrap();
    drop(store);
    let mut wrong = owner();
    wrong.process.started = "replacement".into();
    assert!(matches!(
        RequestStore::open(&dir, &wrong),
        Err(StoreError::IdentityChanged)
    ));
    let mut store = RequestStore::open(&dir, &owner()).unwrap();
    assert!(store.prepare(id("later"), ClockOperation::Read, 0).is_err());
    assert!(matches!(
        store.recovery(&id("ready")).unwrap(),
        Recovery::Completed { .. }
    ));
}

#[test]
fn lost_live_reply_stays_dispatched_after_reopening() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut owner = owner();
    owner.process = ProcessIdentity::current();
    owner.endpoint.pid = owner.process.pid;
    owner.endpoint.port = listener.local_addr().unwrap().port();
    let tmp = tempfile::tempdir().unwrap();
    let descriptor = tmp.path().join("clock-control.json");
    let peer_descriptor = descriptor.clone();
    let peer_owner = owner.clone();
    let peer = thread::spawn(move || {
        for index in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0; 4];
            stream.read_exact(&mut header).unwrap();
            let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
            stream.read_exact(&mut bytes).unwrap();
            let request: ClockRequest = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request.request_id, index + 1);
            let mut receipt = reply(request, 0);
            receipt.pid = peer_owner.endpoint.pid;
            receipt.port = peer_owner.endpoint.port;
            save_atomic(&peer_descriptor, &receipt).unwrap();
            if index == 0 {
                let body = serde_json::to_vec(&receipt).unwrap();
                stream
                    .write_all(&(body.len() as u32).to_be_bytes())
                    .unwrap();
                stream.write_all(&body).unwrap();
            }
            // Second request may already have applied; disconnect before publishing a reply.
        }
        listener.set_nonblocking(true).unwrap();
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    });
    let dir = tmp.path().join("store");
    let mut store = RequestStore::create(&dir, owner.clone()).unwrap();
    let end = Instant::now() + Duration::from_secs(3);
    store
        .execute(id("ready"), ClockOperation::Read, 0, end)
        .unwrap();
    assert!(dir.join("ready.reply.json").exists());
    assert!(
        store
            .execute(id("step"), ClockOperation::Advance, 50_000_000, end)
            .is_err()
    );
    drop(store);
    let mut store = RequestStore::open(&dir, &owner).unwrap();
    assert!(matches!(
        store.execute(id("step"), ClockOperation::Advance, 50_000_000, end),
        Err(StoreError::AlreadyDispatched)
    ));
    let recovered = store.reconcile(&id("step"), &descriptor).unwrap();
    assert_eq!(recovered.now_ns, 50_000_000);
    assert_eq!(
        store
            .execute(id("step"), ClockOperation::Advance, 50_000_000, end)
            .unwrap(),
        recovered
    );
    peer.join().unwrap();
}

#[test]
fn retained_receipt_recovers_only_the_original_dispatched_effect() {
    let tmp = tempfile::tempdir().unwrap();
    let directory = tmp.path().join("store");
    let descriptor = tmp.path().join("clock-control.json");
    let mut store = RequestStore::create(&directory, owner()).unwrap();
    ready(&mut store);
    store
        .prepare(id("advance"), ClockOperation::Advance, 50_000_000)
        .unwrap();
    assert!(matches!(
        store.reconcile(&id("advance"), &descriptor),
        Err(StoreError::NotDispatched)
    ));
    let request = store.dispatch(&id("advance")).unwrap();
    drop(store);
    let mut store = RequestStore::open(&directory, &owner()).unwrap();
    // Old, in-flight and foreign records cannot resolve a lost response.
    let old_request = ClockRequest {
        request_id: 1,
        operation: ClockOperation::Read,
        delta_ns: 0,
        ..request.clone()
    };
    save_atomic(&descriptor, &reply(old_request, 0)).unwrap();
    assert!(store.reconcile(&id("advance"), &descriptor).is_err());
    let completed = reply(request, 0);
    let text = serde_json::to_string(&completed).unwrap();
    std::fs::write(
        &descriptor,
        text.replace("\"status\":\"ok\"", "\"status\":\"executing\""),
    )
    .unwrap();
    assert!(store.reconcile(&id("advance"), &descriptor).is_err());
    let mut foreign = completed.clone();
    foreign.pid += 1;
    save_atomic(&descriptor, &foreign).unwrap();
    assert!(store.reconcile(&id("advance"), &descriptor).is_err());
    assert!(matches!(
        store.recovery(&id("advance")).unwrap(),
        Recovery::Reconcile { .. }
    ));
    save_atomic(&descriptor, &completed).unwrap();
    assert_eq!(
        store.reconcile(&id("advance"), &descriptor).unwrap(),
        completed
    );
    let raw: String = load(&directory.join("advance.reply.json"))
        .unwrap()
        .unwrap();
    assert_eq!(serde_json::from_str::<Reply>(&raw).unwrap(), completed);
    std::fs::remove_file(&descriptor).unwrap();
    assert_eq!(
        store.reconcile(&id("advance"), &descriptor).unwrap(),
        completed
    );
    store.prepare(id("next"), ClockOperation::Read, 0).unwrap();
}

#[test]
fn allowance_binding_and_exhaustion_preserve_original_dispatch() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    for completed in [false,true] {
        let temp=tempfile::tempdir().unwrap();let root=temp.path();let dir=root.join("store");
        let budget=WriteBudget::create(&root.join("budget"),LaunchId::new("g-webview").unwrap(),
            Limits {ordinary:65536,retirement:4096}).unwrap();
        let mut store=RequestStore::create_with_budget(&dir,owner(),Some(&budget)).unwrap();
        let path=dir.join("requests.json");
        assert_eq!(budget.charged().unwrap().ordinary,std::fs::metadata(&path).unwrap().len());
        store.prepare(id("first"),ClockOperation::Read, 0).unwrap();
        let request=store.dispatch(&id("first")).unwrap();let result=reply(request.clone(),0);
        if completed {store.complete(&id("first"),result.clone()).unwrap();}
        drop(store);
        assert!(RequestStore::open_with_budget(&dir,&owner(),None).is_err());
        let other=WriteBudget::create(&root.join("other"),LaunchId::new("g-other").unwrap(),
            Limits {ordinary:65536,retirement:4096}).unwrap();
        assert!(RequestStore::open_with_budget(&dir,&owner(),Some(&other)).is_err());
        budget.charge(Class::Ordinary,65536-budget.charged().unwrap().ordinary).unwrap();
        let mut store=RequestStore::open_with_budget(&dir,&owner(),Some(&budget)).unwrap();
        let before=std::fs::read(&path).unwrap();
        let completion=store.complete(&id("first"),result);
        if completed {
            completion.unwrap();
            assert!(store.prepare(id("next"),ClockOperation::Read, 0).is_err());
            assert!(matches!(store.recovery(&id("next")),Err(StoreError::Unknown)));
        }
        else {
            assert!(completion.is_err());
            assert!(matches!(store.recovery(&id("first")).unwrap(),Recovery::Reconcile {request:original} if original==request));
        }
        assert!(store.dispatch(&id("first")).is_err());
        assert!(store.save_receipt(&dir.join("refused-receipt"),&"original wire reply").is_err());
        assert!(!dir.join("refused-receipt").exists());
        assert_eq!(std::fs::read(&path).unwrap(),before);
        assert_eq!(budget.charged().unwrap(),Limits {ordinary:65536,retirement:0});
    }
}
