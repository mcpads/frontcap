use super::*;
use crate::{
    ios_host::{Config, Target},
    ios_host_session,
    simulator_app::tests::{acquire, setup},
};
use frontcap_core::{
    native::{
        ios_host::{Completion, Operation, Ready},
        request::ClockRequest,
    },
    state::save_atomic,
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};
fn id(value: &str) -> RequestId {
    RequestId::new(value).unwrap()
}
fn producer(root: &Path, page_id: &str, generation: &str) -> (Page, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = crate::webview::Endpoint {
        generation: generation.into(),
        pid: std::process::id(),
        port: listener.local_addr().unwrap().port(),
    };
    let page = Page {
        view: None, document: None, superseded: false, url_truncated: false,
        id: page_id.into(),
        endpoint: endpoint.clone(),
        alive: true,
        control_stopped: false,
        url: "https://example.invalid/game".into(),
    };
    let receipt = root
        .join("Documents/webview-pages")
        .join(page_id)
        .join("clock-control.json");
    std::fs::create_dir_all(receipt.parent().unwrap()).unwrap();
    let worker = thread::spawn(move || {
        let mut before = 0;
        for _ in 0..2 {
            let end = Instant::now() + Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < end, "clock request never arrived");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            // Acceptance is polled, but framed request reads must wait for the payload.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut header = [0; 4];
            stream.read_exact(&mut header).unwrap();
            let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
            stream.read_exact(&mut bytes).unwrap();
            let request: ClockRequest = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request.generation, endpoint.generation);
            let now = before + request.delta_ns;
            let state = |ns: u64| crate::webview::State {
                timer_queue: None,
                frames: None,
                elapsed_ms: ns as f64 / 1e6,
                performance_ms: ns as f64 / 1e6,
                date_ms: 1_700_000_000_000.0 + ns as f64 / 1e6,
                advancing: false,
                failure: None,
                pending_timers: 0,
                cancelled_on_failure: 0,
            };
            let advancing = request.operation == ClockOperation::Advance;
            let reply = Reply {
                observation: Default::default(),
                generation: endpoint.generation.clone(),
                pid: endpoint.pid,
                port: endpoint.port,
                last_id: request.request_id,
                request,
                now_ns: now,
                replayed: false,
                status: Status::Ok,
                applied: Some(advancing),
                document: Some(endpoint.generation.clone()),
                state: Some(state(now)),
                progress: advancing.then(|| crate::webview::Progress {
                    before: state(before),
                    requested_ms: now as f64 / 1e6,
                    after: state(now),
                }),
                error: None,
            };
            save_atomic(&receipt, &reply).unwrap();
            let bytes = serde_json::to_vec(&reply).unwrap();
            stream
                .write_all(&(bytes.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&bytes).unwrap();
            before = now;
        }
    });
    (page, worker)
}
#[test]
fn ios_host_pages_keep_attachment_identity_and_recover_before_host_publication() {
    page_selection(false, false);
}
#[test]
fn ios_host_releases_live_page_selection_without_retirement_or_losing_original_results() {
    page_selection(true, false);
}
#[test]
fn ios_host_selects_new_document_channel_without_rebinding_old_view_authority() {
    page_selection(true, true);
}
fn page_selection(release: bool, replacement_document: bool) {
    let (fixture, mut app_config) = setup("normal");
    app_config.page_registry = true;
    let config = Config {
        lifetime_ms: 900_000, simulator: fixture.config.clone(),
        app: app_config.clone(),
        contacts: false,
        system_ui: None,
    };
    let root = fixture.root.path();
    let directory = root.join("session");
    std::fs::create_dir_all(directory.join("target")).unwrap();
    let cancel = AtomicBool::new(false);
    let end = Instant::now() + Duration::from_secs(30);
    let app = App::launch(
        acquire(&fixture),
        app_config,
        &directory.join("target/app"),
        end,
        &cancel,
    )
    .unwrap();
    std::fs::create_dir_all(root.join("Documents")).unwrap();
    let inventory_file = root.join("Documents/webview-pages.json");
    let mut inventory = Inventory {
        navigation: None,
        pid: app.process().pid,
        revision: 1,
        capacity_exceeded: false,
        pages: vec![],
    };
    save_atomic(&inventory_file, &inventory).unwrap();
    let mut target =
        Target::connect(app, &directory.join("target/host"), false, end, &cancel).unwrap();
    let ready = Ready {
        owner: target.owner().clone(),
        contacts: None,
        pages: true, app_lifecycle: false,
        system_ui: None,
    };
    config.validate_ready(&ready).unwrap();
    save_atomic(&directory.join("host-ready.json"), &ready).unwrap();
    save_atomic(&directory.join("launch.json"),&serde_json::json!({"host":true,"config":crate::webview_session::Config {lifetime_ms: 900_000, simulator:config.simulator.clone(),app:config.app.clone(),contacts:false,system_ui:None,lifecycle:None,service_url_environment:None},"deadline_ns":1})).unwrap();
    std::fs::create_dir(directory.join("target/simulator")).unwrap();
    save_atomic(
        &directory.join("target/simulator/simulator.json"),
        &ready.owner.simulator,
    )
    .unwrap();
    let call = |target: &mut Target, name: &str, operation: &Operation| {
        ios_host_session::execute(target, &directory, &id(name), operation, end, &cancel)
    };
    assert!(
        matches!(call(&mut target,"empty",&Operation::DiscoverPages).unwrap(),Completion::Pages {inventory,..} if inventory.pages.is_empty())
    );
    let (mut first, first_worker) = producer(
        root,
        "550e8400-e29b-41d4-a716-446655440000",
        "550e8400-e29b-41d4-a716-446655440001",
    );
    if replacement_document {
        first.view = Some(first.id.clone());
        first.document = Some(first.endpoint.generation.clone());
    }
    inventory.revision += 1;
    inventory.pages.push(first.clone());
    save_atomic(&inventory_file, &inventory).unwrap();
    call(&mut target, "discover-one", &Operation::DiscoverPages).unwrap();
    assert!(
        call(
            &mut target,
            "bad-selection",
            &Operation::AttachPage {
                discovery: id("empty"),
                page: first.id.clone()
            }
        )
        .is_err()
    );
    if !release {
        first.url.push_str("?loaded");
        inventory.pages[0] = first.clone();
        inventory.revision += 1;
        save_atomic(&inventory_file, &inventory).unwrap();
        let error = call(&mut target, "stale-selection", &Operation::AttachPage {
            discovery: id("discover-one"), page: first.id.clone(),
        }).unwrap_err();
        assert!(error.to_string().contains("discovery changed before attachment"));
        assert!(!directory.join("target/host/pages/stale-selection").exists());
        call(&mut target, "discover-fresh", &Operation::DiscoverPages).unwrap();
    }
    call(
        &mut target,
        "attach-one",
        &Operation::AttachPage {
            discovery: id(if release { "discover-one" } else { "discover-fresh" }),
            page: first.id.clone(),
        },
    )
    .unwrap();
    assert!(
        call(
            &mut target,
            "still-alive",
            &Operation::RetirePage {
                attachment: id("attach-one")
            }
        )
        .is_err()
    );
    let advance = Operation::Page {
        attachment: id("attach-one"),
        command: PageOperation::Advance {
            delta_ns: 100_000_000,
        },
    };
    let original = call(&mut target, "advance-one", &advance).unwrap();
    first_worker.join().unwrap();
    assert_eq!(
        call(&mut target, "advance-one", &advance).unwrap(),
        original
    );
    assert!(
        call(
            &mut target,
            "advance-one",
            &Operation::Page {
                attachment: id("attach-one"),
                command: PageOperation::Read
            }
        )
        .is_err()
    );
    let journal = directory.join("target/host/pages/attach-one/requests/requests.json");
    let before = std::fs::read(&journal).unwrap();
    std::fs::remove_file(directory.join("host-requests/advance-one/result.json")).unwrap();
    for _ in 0..2 {
        let result =
            ios_host_session::read_result(&directory, &config, &id("advance-one"), &advance)
                .unwrap()
                .unwrap();
        assert!(
            matches!(result,crate::webview_session::Reply::Completed {completion} if *completion==crate::webview_session::Completion::Host {completion:original.clone()})
        );
    }
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    assert!(
        !directory
            .join("host-requests/advance-one/result.json")
            .exists()
    );
    if replacement_document {
        inventory.pages[0].superseded = true;
        inventory.revision += 1;
        save_atomic(&inventory_file, &inventory).unwrap();
        assert!(call(&mut target, "old-document-live", &advance).is_err());
        assert_eq!(call(&mut target, "advance-one", &advance).unwrap(), original);
    }
    if release {
        let operation = Operation::ReleasePage { attachment: id("attach-one") };
        let released = call(&mut target, "release-one", &operation).unwrap();
        assert!(matches!(released, Completion::Released { .. }));
        assert_eq!(call(&mut target, "release-one", &operation).unwrap(), released);
        assert!(inventory.pages[0].alive && !inventory.pages[0].control_stopped);
        let request_root = directory.join("host-requests/release-one");
        assert!(!request_root.join("page-retirement.json").exists());
        std::fs::remove_file(request_root.join("result.json")).unwrap();
        for _ in 0..2 {
            let recovered = ios_host_session::read_result(&directory, &config, &id("release-one"), &operation).unwrap().unwrap();
            assert!(matches!(recovered, crate::webview_session::Reply::Completed {completion}
                if *completion == crate::webview_session::Completion::Host {completion:released.clone()}));
        }
        assert!(!request_root.join("result.json").exists());
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        let proof = request_root.join("page-release.json");
        let bytes = std::fs::read(&proof).unwrap();
        std::fs::remove_file(&proof).unwrap();
        assert!(ios_host_session::read_result(&directory, &config, &id("release-one"), &operation).unwrap().is_none());
        std::fs::write(&proof, &bytes).unwrap();
        let mut tampered: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        tampered["requests_sha256"] = serde_json::json!("0".repeat(64));
        save_atomic(&proof, &tampered).unwrap();
        assert!(ios_host_session::read_result(&directory, &config, &id("release-one"), &operation).is_err());
        std::fs::write(&proof, bytes).unwrap();
        assert!(call(&mut target, "reattach-released", &Operation::AttachPage {
            discovery: id("discover-one"), page:first.id.clone() }).is_err());
    } else {
    inventory.revision += 1;
    inventory.pages[0].alive = false;
    inventory.pages[0].control_stopped = true;
    save_atomic(&inventory_file, &inventory).unwrap();
    call(
        &mut target,
        "retire-one",
        &Operation::RetirePage {
            attachment: id("attach-one"),
        },
    )
    .unwrap();
    }
    let (mut second, second_worker) = producer(
        root,
        "550e8400-e29b-41d4-a716-446655440002",
        "550e8400-e29b-41d4-a716-446655440003",
    );
    if replacement_document {
        second.view = first.view.clone();
        second.document = Some(second.endpoint.generation.clone());
    }
    inventory.revision += 1;
    inventory.pages.push(second.clone());
    save_atomic(&inventory_file, &inventory).unwrap();
    call(&mut target, "discover-two", &Operation::DiscoverPages).unwrap();
    call(
        &mut target,
        "attach-two",
        &Operation::AttachPage {
            discovery: id("discover-two"),
            page: second.id.clone(),
        },
    )
    .unwrap();
    assert!(call(&mut target, "old-reference", &advance).is_err());
    let replacement = call(
        &mut target,
        "advance-two",
        &Operation::Page {
            attachment: id("attach-two"),
            command: PageOperation::Advance {
                delta_ns: 25_000_000,
            },
        },
    )
    .unwrap();
    second_worker.join().unwrap();
    assert!(
        matches!(replacement,Completion::Page {reply,..} if reply.now_ns==25_000_000 && reply.document.as_ref()==Some(&second.endpoint.generation))
    );
    if replacement_document {
        for field in ["document", "view", "superseded"] {
            let mut invalid = inventory.clone();
            invalid.revision += 1;
            match field {
                "document" => invalid.pages[0].document = second.document.clone(),
                "view" => invalid.pages[0].view = Some(second.id.clone()),
                _ => invalid.pages[0].superseded = false,
            }
            save_atomic(&inventory_file, &invalid).unwrap();
            assert!(call(&mut target, &format!("rebind-{field}"), &Operation::DiscoverPages).is_err());
        }
        save_atomic(&inventory_file, &inventory).unwrap();
    }
    let mut regressed = inventory.clone();
    regressed.revision -= 1;
    save_atomic(&inventory_file, &regressed).unwrap();
    assert!(call(&mut target, "regressed", &Operation::DiscoverPages).is_err());
    save_atomic(&inventory_file, &inventory).unwrap();
    let mut changed: frontcap_core::native::ios_host::Attachment =
        load(&directory.join("target/host/pages/attach-one/attachment.json"))
            .unwrap()
            .unwrap();
    changed.owner.endpoint = second.endpoint;
    save_atomic(
        &directory.join("target/host/pages/attach-one/attachment.json"),
        &changed,
    )
    .unwrap();
    assert!(
        ios_host_session::read_result(&directory, &config, &id("advance-one"), &advance).is_err()
    );
    assert!(target.stop().passed);
}

#[test]
fn release_keeps_pending_page_requests_owned_and_does_not_publish_success() {
    let (fixture, config) = setup("normal");
    let end = Instant::now() + Duration::from_secs(5);
    let cancel = AtomicBool::new(false);
    let mut app = App::launch(acquire(&fixture), config, &fixture.root.path().join("app"), end, &cancel).unwrap();
    let owner = Owner { simulator:app.simulator_id().into(), process:app.process().clone(),
        attachment_sha256:app.attachment_sha256().into() };
    let mut pages = Pages::new(&fixture.root.path().join("pages"), owner.clone()).unwrap();
    let endpoint = crate::webview::Endpoint {generation:"550e8400-e29b-41d4-a716-446655440001".into(),pid:owner.process.pid,port:12345};
    let attachment = Attachment {id:id("selected"), owner:ClockOwner {simulator:owner.simulator.clone(),
        process:owner.process.clone(),attachment_sha256:owner.attachment_sha256.clone(),endpoint:endpoint.clone()},
        page:Page {view:None,document:None,superseded:false,url_truncated:false,id:"550e8400-e29b-41d4-a716-446655440000".into(),endpoint,alive:true,control_stopped:false,url:String::new()},
        document:"550e8400-e29b-41d4-a716-446655440002".into()};
    let root = pages.directory.join("selected");std::fs::create_dir(&root).unwrap();
    let mut requests = RequestStore::create(&root.join("requests"), attachment.owner.clone()).unwrap();
    requests.prepare(id("pending"), ClockOperation::Read, 0).unwrap();
    pages.active = Some(Active {attachment, requests});
    let proof = fixture.root.path().join("release");std::fs::create_dir(&proof).unwrap();
    for dispatched in [false, true] {
        if dispatched {pages.active.as_mut().unwrap().requests.dispatch(&id("pending")).unwrap();}
        let before = std::fs::read(root.join("requests/requests.json")).unwrap();
        assert!(pages.release(&app, &id("selected"), &proof).unwrap_err().to_string().contains("resolve the original page request"));
        assert!(pages.active.is_some());
        assert!(!proof.join("page-release.json").exists());
        assert_eq!(std::fs::read(root.join("requests/requests.json")).unwrap(), before);
    }
    assert!(app.stop().passed);
}

#[test]
fn page_receipt_retention_uses_retirement_only_for_stop() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let (fixture, config) = setup("normal");
    let root = fixture.root.path();
    let cancel = AtomicBool::new(false);
    let end = Instant::now() + Duration::from_secs(10);
    let mut app = App::launch(acquire(&fixture), config, &root.join("app"), end, &cancel).unwrap();
    let owner = Owner { simulator: app.simulator_id().into(), process: app.process().clone(),
        attachment_sha256: app.attachment_sha256().into() };
    let mut pages = Pages::new(&root.join("pages"), owner).unwrap();
    let budget = WriteBudget::create(&root.join("budget"), LaunchId::generate(),
        Limits { ordinary: 1024, retirement: 1024 }).unwrap();
    pages.budget = Some(budget.clone());
    let page = Page { view: None, document: None, superseded: false, url_truncated: false,
        id: "550e8400-e29b-41d4-a716-446655440001".into(),
        endpoint: crate::webview::Endpoint { generation: "550e8400-e29b-41d4-a716-446655440002".into(),
            pid: app.process().pid, port: 12345 }, alive: true, control_stopped: false,
        url: "https://example.invalid/".into() };
    let request = id("attached");
    let destination = pages.directory.join(request.as_str());
    std::fs::create_dir(&destination).unwrap();
    save_atomic(&destination.join("candidate.json"), &page).unwrap();
    let source = receipt(&app, &page);
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let original = b"{\"original\":true}";
    std::fs::write(&source, original).unwrap();
    budget.charge(Class::Ordinary, 1024).unwrap();
    assert!(pages.retain(&app).is_err());
    assert!(!destination.join("last-native-receipt.json").exists());
    pages.retain_for_stop(&app).unwrap();
    assert_eq!(std::fs::read(destination.join("last-native-receipt.json")).unwrap(), original);
    assert_eq!(budget.charged().unwrap().retirement, original.len() as u64);
    budget.charge(Class::Retirement, 1024 - original.len() as u64).unwrap();
    std::fs::write(&source, b"{\"later\":true}").unwrap();
    assert!(pages.retain_for_stop(&app).is_err());
    assert_eq!(std::fs::read(destination.join("last-native-receipt.json")).unwrap(), original);
    assert!(app.stop().passed);
}
