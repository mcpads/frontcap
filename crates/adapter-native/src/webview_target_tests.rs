use super::*;
use frontcap_core::native::contacts::Window;
use crate::{
    request::ClockRequest,
    simulator_app::tests::{acquire, setup},
    webview::State,
};
use std::{
    io::{Read, Write},
    net::TcpListener,
    thread,
};

#[test]
fn readiness_waits_for_page_and_retains_the_owned_app() {
    let (fixture, config) = setup("normal");
    let directory = fixture.root.path();
    let app = App::launch(
        acquire(&fixture),
        config,
        &directory.join("app"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = Endpoint {
        generation: "23357f73-a68f-4271-a765-c8fb5538acc3".into(),
        pid: std::process::id(),
        port: listener.local_addr().unwrap().port(),
    };
    std::fs::create_dir(directory.join("Documents")).unwrap();
    save_atomic(&directory.join("Documents/clock-control.json"), &endpoint).unwrap();
    let server = thread::spawn(move || {
        for ready in [false, true] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0; 4];
            stream.read_exact(&mut header).unwrap();
            let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
            stream.read_exact(&mut bytes).unwrap();
            let request: ClockRequest = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request.operation, ClockOperation::Read);
            assert_eq!(request.delta_ns, 0);
            let reply = Reply {
                observation: Default::default(),
                generation: endpoint.generation.clone(),
                pid: endpoint.pid,
                port: endpoint.port,
                last_id: request.request_id,
                request,
                now_ns: 0,
                replayed: false,
                status: if ready { Status::Ok } else { Status::NotReady },
                applied: Some(false),
                document: ready.then(|| "87c86105-02ad-4d07-bc0c-58c922960225".into()),
                state: ready.then_some(State {
                    timer_queue: None, frames: None,
                    elapsed_ms: 0.0,
                    date_ms: 1700000000000.0,
                    performance_ms: 0.0,
                    advancing: false,
                    failure: None,
                    pending_timers: 0,
                    cancelled_on_failure: 0,
                }),
                progress: None,
                error: None,
            };
            let bytes = serde_json::to_vec(&reply).unwrap();
            stream
                .write_all(&(bytes.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&bytes).unwrap();
        }
    });
    let mut target = Target::connect(
        app,
        &directory.join("target"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert!(directory.join("target/clock-ready.json").exists());
    assert!(
        directory
            .join("target/requests/mcp-webview-acquire-2.reply.json")
            .exists()
    );
    assert!(
        target
            .execute(
                RequestId::new("mcp-webview-acquire-1").unwrap(),
                ClockOperation::Read,
                0,
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    // Idle process monitoring must not depend on a potentially stalled simulator inventory.
    let calls_before = std::fs::read(directory.join("calls")).unwrap();
    target
        .ensure_process_running(
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert_eq!(
        std::fs::read(directory.join("calls")).unwrap(),
        calls_before
    );
    let original_start = target.owner.process.started.clone();
    target.owner.process.started = "different process incarnation".into();
    assert!(
        target
            .ensure_process_running(
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    target.owner.process.started = original_start;
    assert!(!directory.join("deleted").exists());
    // Attach the native file fixture, but exercise the real target capture/input admission.
    let hid = directory.join("Documents/frontcap-hid");
    std::fs::create_dir(&hid).unwrap();
    let endpoint = frontcap_core::native::contacts::Endpoint {
        generation: target.owner.endpoint.generation.clone(),
        pid: std::process::id(),
        available: true,
        short_tap: false, observed_contacts: true,
        request_capacity: 24,
    };
    save_atomic(&hid.join("endpoint.json"), &endpoint).unwrap();
    let window = Window {
        generation: endpoint.generation.clone(),
        pid: endpoint.pid,
        token: "full-screen".into(),
        width: 1.0,
        height: 1.0,
        scale: 1.0,
        fullscreen: true,
    };
    save_atomic(&hid.join("window.json"), &serde_json::json!({})).unwrap();
    let path = hid.join("window.json");
    let ready_window = window.clone();
    let publish = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        save_atomic(&path, &ready_window).unwrap();
    });
    target.enable_contacts(Instant::now() + Duration::from_secs(3), &AtomicBool::new(false)).unwrap();
    publish.join().unwrap();
    let file = std::fs::File::create(directory.join("pixel.png")).unwrap();
    let mut encoder = png::Encoder::new(file, 1, 1);
    encoder.set_color(png::ColorType::Rgb);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&[0, 0, 0])
        .unwrap();
    let script=std::fs::read_to_string(&fixture.config.xcrun).unwrap().replace("elif args[1]=='bootstatus':pass",
        "elif args[1]=='io':pathlib.Path(args[-1]).write_bytes((p/'pixel.png').read_bytes())\nelif args[1]=='bootstatus':pass");
    std::fs::write(&fixture.config.xcrun, script).unwrap();
    let input = ContactOperation::Set {
        expected_revision: 0,
        contacts: vec![frontcap_core::native::contacts::Contact {
            id: 1,
            x: 0.5,
            y: 0.5,
        }],
    };
    let observation = RequestId::new("screen").unwrap();
    let input_id = RequestId::new("press").unwrap();
    let cancelled = AtomicBool::new(false);
    assert!(
        target
            .input(
                input_id.clone(),
                observation.clone(),
                input.clone(),
                Instant::now() + Duration::from_secs(2),
                &cancelled
            )
            .is_err()
    );
    let screen = target
        .capture(
            observation.clone(),
            Instant::now() + Duration::from_secs(3),
            &cancelled,
        )
        .unwrap();
    let bytes = std::fs::read(&screen).unwrap();
    std::fs::write(&screen, b"changed").unwrap();
    assert!(
        target
            .input(
                input_id.clone(),
                observation.clone(),
                input.clone(),
                Instant::now() + Duration::from_secs(2),
                &cancelled
            )
            .is_err()
    );
    assert!(!hid.join("request-1.json").exists());
    std::fs::write(&screen, &bytes).unwrap();
    assert!(
        target
            .input(
                input_id.clone(),
                observation.clone(),
                input.clone(),
                Instant::now() + Duration::from_millis(400),
                &cancelled
            )
            .is_err()
    );
    assert!(hid.join("request-1.json").exists());
    assert!(
        target
            .input(
                RequestId::new("another").unwrap(),
                observation.clone(),
                input.clone(),
                Instant::now() + Duration::from_secs(2),
                &cancelled
            )
            .is_err()
    );
    assert!(!hid.join("request-2.json").exists());
    let request: serde_json::Value = load(&hid.join("request-1.json")).unwrap().unwrap();
    let native = serde_json::json!({"generation":endpoint.generation,"request_id":1,"status":"enqueued",
        "request":request,"contact_revision":1,"contacts":[{"id":1,"x":0.5,"y":0.5}],"changed":true,"mach_timestamp":1});
    save_atomic(&hid.join("reply-1.json"), &native).unwrap();
    assert!(matches!(
        target
            .reconcile_input(
                &input_id,
                Instant::now() + Duration::from_secs(2),
                &cancelled
            )
            .unwrap(),
        ContactReceipt::Enqueued { revision: 1, .. }
    ));
    assert!(
        target
            .input(
                input_id,
                observation,
                input,
                Instant::now() + Duration::from_secs(2),
                &cancelled
            )
            .is_ok()
    );
    qualify_lifecycle(&mut target, &fixture);
    let next = RequestId::new("cancel-screen").unwrap();
    target
        .capture(
            next.clone(),
            Instant::now() + Duration::from_secs(3),
            &cancelled,
        )
        .unwrap();
    assert_eq!(target.input_basis().unwrap().expected_revision, 1);
    assert!(target.input_basis().unwrap().requires_cancel);
    assert!(
        target
            .input(
                RequestId::new("blocked-set").unwrap(),
                next.clone(),
                ContactOperation::Set {
                    expected_revision: 1,
                    contacts: vec![]
                },
                Instant::now() + Duration::from_secs(2),
                &cancelled
            )
            .is_err()
    );
    assert!(!hid.join("request-2.json").exists());
    assert!(
        !target
            .input.input_bindings
            .contains_key(&RequestId::new("blocked-set").unwrap())
    );
    assert!(target.input_basis().unwrap().requires_cancel);

    assert!(
        target
            .input(
                RequestId::new("cancel").unwrap(),
                next,
                ContactOperation::Cancel {
                    expected_revision: 1
                },
                Instant::now() + Duration::from_millis(400),
                &cancelled
            )
            .is_err()
    );
    let executing =
        serde_json::json!({"generation":endpoint.generation,"request_id":2,"status":"executing"});
    save_atomic(&hid.join("reply-2.json"), &executing).unwrap();
    assert!(
        target
            .capture(
                RequestId::new("diagnostic").unwrap(),
                Instant::now() + Duration::from_secs(3),
                &cancelled
            )
            .unwrap()
            .exists()
    );
    assert!(target.input_basis().is_none());
    let original = std::fs::read(directory.join("Documents/clock-control.json")).unwrap();
    let script = std::fs::read_to_string(&fixture.config.xcrun).unwrap().replace(
        "state.write_text('[]');(p/'deleted').write_text('yes')",
        "import fcntl;lock=(p/'Documents/frontcap-hid/requests.lock').open('r+');fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB);lock.close();state.write_text('[]');(p/'deleted').write_text('yes');import shutil;shutil.rmtree(p/'Documents')");
    std::fs::write(&fixture.config.xcrun, script).unwrap();
    assert!(crate::store::lock(&hid).is_err(), "live input retains its native lock");
    assert!(target.stop().passed);
    let retained_lifecycle: frontcap_core::native::webview_lifecycle::Reply =
        load(&directory.join("target/last-lifecycle-receipt.json"))
            .unwrap()
            .unwrap();
    assert_eq!(
        retained_lifecycle.request.operation,
        frontcap_core::native::webview_lifecycle::Operation::Foreground
    );
    assert_eq!(
        target
            .reconcile_lifecycle(
                &RequestId::new("return").unwrap(),
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false)
            )
            .unwrap(),
        retained_lifecycle
    );
    assert!(!directory.join("Documents/clock-control.json").exists());
    assert_eq!(
        load::<serde_json::Value>(&directory.join("target/contacts/reply-2.json"))
            .unwrap()
            .unwrap(),
        executing
    );
    assert_eq!(
        std::fs::read(directory.join("target/last-native-receipt.json")).unwrap(),
        original
    );
    assert!(target.stop().passed);
    server.join().unwrap();
}

#[test]
fn foreign_endpoint_never_becomes_ready_and_retires_the_owned_app() {
    let (fixture, config) = setup("normal");
    let directory = fixture.root.path();
    let app = App::launch(
        acquire(&fixture),
        config,
        &directory.join("app"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
    )
    .unwrap();
    std::fs::create_dir(directory.join("Documents")).unwrap();
    save_atomic(
        &directory.join("Documents/clock-control.json"),
        &Endpoint {
            generation: "23357f73-a68f-4271-a765-c8fb5538acc3".into(),
            pid: std::process::id() + 1,
            port: 12345,
        },
    )
    .unwrap();
    let failure = match Target::connect(
        app,
        &directory.join("target"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
    ) {
        Err(e) => e,
        Ok(_) => panic!("accepted foreign endpoint"),
    };
    assert!(failure.retirement.unwrap().passed);
    assert!(!directory.join("target/clock-ready.json").exists());
    assert!(directory.join("deleted").exists());
}

fn qualify_lifecycle(target: &mut Target, fixture: &crate::simulator::tests::Fixture) {
    use frontcap_core::native::webview_lifecycle::{
        Operation, Reply, Request, ResultState, Status,
    };
    let mut runner = crate::xctest::tests::config(fixture);
    runner.app.bundle = "dev.frontcap.consumer".into();
    crate::xctest::tests::prepare_ready_runner(fixture, &runner);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let script = std::fs::read_to_string(&fixture.config.xcrun).unwrap().replace(
        "pointer=p/'container-path'",
        "pointer=p/('runner-container-path' if args[3].endswith('.xctrunner') else 'container-path')");
    std::fs::write(&fixture.config.xcrun, script).unwrap();
    let script = std::fs::read_to_string(&runner.xcodebuild)
        .unwrap()
        .replace("root/'container-path'", "root/'runner-container-path'")
        .replace("'port':1234", &format!("'port':{port}"));
    std::fs::write(&runner.xcodebuild, script).unwrap();
    let descriptor = fixture
        .root
        .path()
        .join("ABCD0000-0000-0000-0000-000000000001/container")
        .join(&runner.app.ui_descriptor);
    let thread_descriptor = descriptor.clone();
    let server = thread::spawn(move || {
        for operation in [
            Operation::Launch,
            Operation::Background,
            Operation::Foreground,
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut header = [0; 4];
            stream.read_exact(&mut header).unwrap();
            let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
            stream.read_exact(&mut bytes).unwrap();
            let request: Request = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(request.operation, operation);
            let prior: serde_json::Value = load(&thread_descriptor).unwrap().unwrap();
            let reply = Reply {
                generation: request.generation.clone(),
                pid: prior["pid"].as_u64().unwrap() as u32,
                port,
                last_id: request.request_id,
                request,
                replayed: false,
                status: Status::Ok,
                result: Some(ResultState {
                    operation,
                    app_state: if operation == Operation::Background {
                        3
                    } else {
                        4
                    },
                    frame_points: if operation == Operation::Background {
                        [0.0; 4]
                    } else {
                        [0.0, 0.0, 852.0, 393.0]
                    },
                    input: "none".into(),
                    touches: None,
                }),
                error: None,
            };
            save_atomic(&thread_descriptor, &reply).unwrap();
            if operation != Operation::Background {
                let bytes = serde_json::to_vec(&reply).unwrap();
                stream
                    .write_all(&(bytes.len() as u32).to_be_bytes())
                    .unwrap();
                stream.write_all(&bytes).unwrap();
            }
        }
    });
    let process = target.owner.process.clone();
    let deadline = Instant::now() + Duration::from_secs(20);
    let cancelled = AtomicBool::new(false);
    target
        .enable_lifecycle(runner.into(), deadline, &cancelled)
        .unwrap();
    assert!(target.lifecycle.as_ref().unwrap().requests.is_foreground());
    let home = RequestId::new("home").unwrap();
    assert!(
        target
            .lifecycle(home.clone(), Operation::Background, deadline, &cancelled)
            .is_err()
    );
    assert!(
        target
            .lifecycle(home.clone(), Operation::Background, deadline, &cancelled)
            .is_err()
    );
    assert!(!target.lifecycle.as_ref().unwrap().requests.is_foreground());
    assert_eq!(
        target
            .reconcile_lifecycle(&home, deadline, &cancelled)
            .unwrap()
            .result
            .unwrap()
            .app_state,
        3
    );
    target
        .lifecycle(
            RequestId::new("return").unwrap(),
            Operation::Foreground,
            deadline,
            &cancelled,
        )
        .unwrap();
    assert_eq!(target.app.process(), &process);
    assert!(target.input.capture_basis.is_none());
    server.join().unwrap();
}

#[test]
fn replacement_readiness_waits_for_a_new_endpoint_without_contacting_the_old_one() {
    let (fixture, config) = setup("normal");
    let directory = fixture.root.path();
    let mut app = App::launch(acquire(&fixture), config, &directory.join("app"),
        Instant::now()+Duration::from_secs(10), &AtomicBool::new(false)).unwrap();
    let old_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    old_listener.set_nonblocking(true).unwrap();
    let previous = Owner {
        simulator: app.simulator_id().into(), process: app.process().clone(), attachment_sha256: app.attachment_sha256().into(),
        endpoint: Endpoint { generation:"23357f73-a68f-4271-a765-c8fb5538acc3".into(), pid:std::process::id(), port:old_listener.local_addr().unwrap().port() },
    };
    let descriptor=directory.join("Documents/clock-control.json");
    std::fs::create_dir(directory.join("Documents")).unwrap();
    save_atomic(&descriptor,&previous.endpoint).unwrap();
    let listener=TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint=Endpoint { generation:"abcd0000-0000-0000-0000-000000000009".into(), pid:std::process::id(), port:listener.local_addr().unwrap().port() };
    let expected=endpoint.clone();
    let server=thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        save_atomic(&descriptor,&endpoint).unwrap();
        listener.set_nonblocking(true).unwrap();
        let until=Instant::now()+Duration::from_secs(10);
        let mut stream=loop {
            if let Ok((stream,_))=listener.accept() {break stream;}
            assert!(Instant::now()<until,"replacement did not connect");
            thread::sleep(Duration::from_millis(10));
        };
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut header=[0;4]; stream.read_exact(&mut header).unwrap();
        let mut bytes=vec![0;u32::from_be_bytes(header) as usize]; stream.read_exact(&mut bytes).unwrap();
        let request: ClockRequest=serde_json::from_slice(&bytes).unwrap();
        assert_eq!(request.generation,endpoint.generation);
        assert_eq!(request.operation,ClockOperation::Read);
        let reply=serde_json::json!({"generation":endpoint.generation,"pid":endpoint.pid,"port":endpoint.port,
            "last_id":request.request_id,"request":request,"now_ns":0,"replayed":false,"status":"not_ready","applied":false,
            "document":null,
            "state":null,
            "progress":null,"error":null});
        let bytes=serde_json::to_vec(&reply).unwrap();
        stream.write_all(&(bytes.len() as u32).to_be_bytes()).unwrap(); stream.write_all(&bytes).unwrap();
    });
    let epoch=directory.join("replacement"); std::fs::create_dir(&epoch).unwrap();
    let (requests,owner)=Target::initialize(&mut app,&epoch,Some(&previous),Instant::now()+Duration::from_secs(5),&AtomicBool::new(false)).unwrap();
    assert_eq!(owner.endpoint,expected);
    assert!(requests.inspect().pending.is_none());
    assert_eq!(requests.inspect().last_completed.unwrap().reply.status,Status::NotReady);
    assert!(old_listener.accept().is_err(),"never dispatch to a previous-generation descriptor");
    server.join().unwrap(); assert!(app.stop().passed);
}
