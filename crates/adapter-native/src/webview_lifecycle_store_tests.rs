use super::*;
use frontcap_core::{
    native::{ios_ui::UiIdentity, webview, webview_lifecycle::ResultState},
    process::ProcessIdentity,
};
fn id(value: &str) -> RequestId {
    RequestId::new(value).unwrap()
}
fn owner() -> Owner {
    let simulator = "23357f73-a68f-4271-a765-c8fb5538acc3".to_owned();
    Owner {
        app: webview::Owner {
            simulator: simulator.clone(),
            process: ProcessIdentity {
                pid: 42,
                started: "app start".into(),
            },
            endpoint: webview::Endpoint {
                pid: 42,
                port: 1234,
                generation: "87c86105-02ad-4d07-bc0c-58c922960225".into(),
            },
            attachment_sha256: "a".repeat(64),
        },
        runner: UiIdentity {
            simulator,
            generation: "abcd0000-0000-0000-0000-000000000002".into(),
            pid: 43,
        },
        runner_process: ProcessIdentity {
            pid: 43,
            started: "runner start".into(),
        },
        port: 1235,
        products_sha256: "b".repeat(64),
    }
}
fn reply(request: Request) -> Reply {
    let background = request.operation == Operation::Background;
    Reply {
        generation: owner().runner.generation,
        pid: 43,
        port: 1235,
        last_id: request.request_id,
        replayed: false,
        status: Status::Ok,
        result: Some(ResultState {
            operation: request.operation,
            app_state: if background { 3 } else { 4 },
            frame_points: if background {
                [0.0; 4]
            } else {
                [0.0, 0.0, 852.0, 393.0]
            },
            input: "none".into(),
            touches: None,
        }),
        error: None,
        request,
    }
}
fn launch(store: &mut RequestStore) {
    store.prepare(id("attach"), Operation::Launch).unwrap();
    let request = store.dispatch(&id("attach")).unwrap();
    store.complete(&id("attach"), reply(request)).unwrap();
}
#[test]
fn lost_home_reply_recovers_original_receipt_after_reopen_without_redispatch() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store");
    let original = temp.path().join("original.json");
    let mut store = RequestStore::create(&path, owner()).unwrap();
    launch(&mut store);
    store.prepare(id("home"), Operation::Background).unwrap();
    let request = store.dispatch(&id("home")).unwrap();
    drop(store);
    let mut store = RequestStore::open(&path, &owner()).unwrap();
    assert!(matches!(
        store.prepare(id("return"), Operation::Foreground),
        Err(StoreError::Pending)
    ));
    assert!(matches!(
        store.dispatch(&id("home")),
        Err(StoreError::AlreadyDispatched)
    ));
    let progress = store.inspect();
    assert_eq!(progress.pending.as_ref().unwrap().id, id("home"));
    assert_eq!(progress.pending.as_ref().unwrap().dispatch, frontcap_core::native::webview_progress::Dispatch::Unresolved);
    assert_eq!(store.inspect(), progress);
    let expected = reply(request);
    let mut executing = expected.clone();
    executing.status = Status::Executing;
    executing.result = None;
    executing.last_id -= 1;
    save_atomic(&original, &executing).unwrap();
    assert!(store.reconcile(&id("home"), &original).is_err());
    assert!(matches!(
        store.recovery(&id("home")).unwrap(),
        Recovery::Reconcile { .. }
    ));
    save_atomic(&original, &expected).unwrap();
    assert_eq!(store.reconcile(&id("home"), &original).unwrap(), expected);
    assert!(store.inspect().pending.is_none());
    assert_eq!(store.inspect().last_completed.unwrap().reply, expected);
    std::fs::remove_file(&original).unwrap();
    drop(store);
    let mut store = RequestStore::open(&path, &owner()).unwrap();
    assert_eq!(store.reconcile(&id("home"), &original).unwrap(), expected);
    store.prepare(id("return"), Operation::Foreground).unwrap();
    let request = store.dispatch(&id("return")).unwrap();
    store.complete(&id("return"), reply(request)).unwrap();
    assert!(matches!(
        store.prepare(id("home"), Operation::Foreground),
        Err(StoreError::Conflict)
    ));
}
#[test]
fn stale_or_wrong_state_receipts_leave_effect_unresolved_and_owner_changes_reject() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store");
    let mut store = RequestStore::create(&path, owner()).unwrap();
    assert!(store.prepare(id("home"), Operation::Background).is_err());
    launch(&mut store);
    store.prepare(id("home"), Operation::Background).unwrap();
    let request = store.dispatch(&id("home")).unwrap();
    for case in ["request", "runner", "state", "frame"] {
        let mut bad = reply(request.clone());
        match case {
            "request" => bad.request.request_id += 1,
            "runner" => bad.pid += 1,
            "state" => bad.result.as_mut().unwrap().app_state = 4,
            _ => bad.result.as_mut().unwrap().frame_points[2] = 10.0,
        }
        assert!(store.complete(&id("home"), bad).is_err());
        assert!(matches!(
            store.recovery(&id("home")).unwrap(),
            Recovery::Reconcile { .. }
        ));
    }
    drop(store);
    let mut changed = owner();
    changed.runner_process.started = "replacement".into();
    assert!(matches!(
        RequestStore::open(&path, &changed),
        Err(StoreError::IdentityChanged)
    ));
    let mut changed = owner();
    changed.products_sha256 = "c".repeat(64);
    assert!(matches!(
        RequestStore::open(&path, &changed),
        Err(StoreError::IdentityChanged)
    ));
}
#[test]
fn rejected_transition_is_retained_and_fences_new_lifecycle_effects() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("store");
    let mut store = RequestStore::create(&path, owner()).unwrap();
    launch(&mut store);
    store.prepare(id("home"), Operation::Background).unwrap();
    let request = store.dispatch(&id("home")).unwrap();
    let mut rejected = reply(request);
    rejected.status = Status::Rejected;
    rejected.result = None;
    rejected.error = Some("invalid_lifecycle".into());
    rejected.last_id -= 1;
    store.complete(&id("home"), rejected.clone()).unwrap();
    assert!(matches!(
        store.prepare(id("return"), Operation::Foreground),
        Err(StoreError::Pending)
    ));
    assert_eq!(
        store.reconcile(&id("home"), Path::new("absent")).unwrap(),
        rejected
    );
}

#[test]
fn socket_completion_and_lost_reply_preserve_one_dispatch_and_offline_recovery() {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        time::{Duration, Instant},
    };
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let runner = Child(
        std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap(),
    );
    for lose_reply in [false, true] {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut bound = owner();
        bound.app.process = ProcessIdentity::current();
        bound.app.endpoint.pid = bound.app.process.pid;
        bound.runner_process = ProcessIdentity::of(runner.0.id()).unwrap().unwrap();
        bound.runner.pid = bound.runner_process.pid;
        bound.port = listener.local_addr().unwrap().port();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("store");
        let receipt = temp.path().join("original.json");
        let server_receipt = receipt.clone();
        let server_owner = bound.clone();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut header = [0; 4];
            socket.read_exact(&mut header).unwrap();
            let mut body = vec![0; u32::from_be_bytes(header) as usize];
            socket.read_exact(&mut body).unwrap();
            let request: Request = serde_json::from_slice(&body).unwrap();
            assert_eq!(request.operation, Operation::Launch);
            assert_eq!(request.request_id, 1);
            let mut answer = reply(request);
            answer.pid = server_owner.runner.pid;
            answer.port = server_owner.port;
            save_atomic(&server_receipt, &answer).unwrap();
            if !lose_reply {
                let bytes = serde_json::to_vec(&answer).unwrap();
                socket
                    .write_all(&(bytes.len() as u32).to_be_bytes())
                    .unwrap();
                socket.write_all(&bytes).unwrap();
            }
            drop(socket);
            (listener, answer)
        });
        let mut store = RequestStore::create(&path, bound.clone()).unwrap();
        let result = store.execute(
            id("activate"),
            Operation::Launch,
            Instant::now() + Duration::from_secs(5),
        );
        let (listener, expected) = server.join().unwrap();
        if lose_reply {
            assert!(result.is_err());
        } else {
            assert_eq!(result.unwrap(), expected);
        }
        drop(store);
        let mut store = RequestStore::open(&path, &bound).unwrap();
        let repeated = store.execute(
            id("activate"),
            Operation::Launch,
            Instant::now() + Duration::from_secs(5),
        );
        if lose_reply {
            assert!(matches!(repeated, Err(StoreError::AlreadyDispatched)));
        } else {
            assert_eq!(repeated.unwrap(), expected);
        }
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(
            store.reconcile(&id("activate"), &receipt).unwrap(),
            expected
        );
        assert!(path.join("receipt-1.json").is_file());
    }
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
        store.prepare(id("first"),Operation::Launch).unwrap();
        let request=store.dispatch(&id("first")).unwrap();let result=reply(request.clone());
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
            assert!(store.prepare(id("next"),Operation::Background).is_err());
            assert!(matches!(store.recovery(&id("next")),Err(StoreError::Unknown)));
        }
        else {
            assert!(completion.is_err());
            assert!(matches!(store.recovery(&id("first")).unwrap(),Recovery::Reconcile {request:original} if original==request));
        }
        assert!(store.dispatch(&id("first")).is_err());
        assert!(store.save_receipt(&dir.join("refused-receipt"),b"original wire reply").is_err());
        assert!(!dir.join("refused-receipt").exists());
        assert_eq!(std::fs::read(&path).unwrap(),before);
        assert_eq!(budget.charged().unwrap(),Limits {ordinary:65536,retirement:0});
    }
}
