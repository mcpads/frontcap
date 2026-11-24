use super::*;

#[test]
fn worker_entry() {
    let Some(directory) = std::env::var_os("FRONTCAP_ANDROID_WEBVIEW_SESSION_TEST") else {
        return;
    };
    let directory = PathBuf::from(directory);
    run_with(&directory, |_, _, _, cancelled| {
        std::fs::write(directory.join("acquisition-entered"), b"").unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while !cancelled.load(Ordering::Acquire) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(AcquisitionError {
            cause: if cancelled.load(Ordering::Acquire) {
                "acquisition cancelled".into()
            } else {
                "cancellation missing".into()
            },
            retirement: None,
        })
    })
    .unwrap();
}
#[test]
fn control_eof_prevents_or_cancels_acquisition_and_never_restarts_a_worker() {
    for start in [false, true] {
        let f = crate::device::tests::Fixture::new();
        let directory = f.root().join("webview-worker");
        std::fs::create_dir(&directory).unwrap();
        File::create_new(directory.join("session.lock")).unwrap();
        let mut config = f.config.clone();
        config.webview_app = Some(crate::android_webview::Profile {
            package: "org.example.app".into(),
            activity: "org.example.app.ClientActivity".into(),
            local_service: None,
        });
        let end = Instant::now() + Duration::from_secs(10);
        save_atomic(
            &directory.join("launch.json"),
            &Launch {
                config: Config {
                    device: config,
                    entry_url: "http://127.0.0.1/game".into(),
                    lifetime_ms: 30000,
                    input: None,
                },
                deadline_ns: deadline_ns(end).unwrap(),
            },
        )
        .unwrap();
        let budget = frontcap_core::write_budget::WriteBudget::create(&f.root().join("budget"),
            frontcap_core::ids::LaunchId::generate(), frontcap_core::write_budget::Limits {
                ordinary: 1024 * 1024, retirement: 1024 * 1024 }).unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
                "--exact",
                "android_webview_session::tests::worker_entry",
                "--nocapture",
            ])
            .env("FRONTCAP_ANDROID_WEBVIEW_SESSION_TEST", &directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::null());
        frontcap_core::write_budget::WriteBudget::configure_child(&mut command, Some(&budget)).unwrap();
        let mut child = command.spawn().unwrap();
        while !directory.join("worker.json").exists() {
            assert!(Instant::now() < end);
            assert!(child.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(5));
        }
        budget.charge(Class::Ordinary, 1024 * 1024 - budget.charged().unwrap().ordinary).unwrap();
        if start {
            let mut bytes = serde_json::to_vec(&Message::Start).unwrap();
            bytes.push(b'\n');
            child.stdin.as_mut().unwrap().write_all(&bytes).unwrap();
            while !directory.join("acquisition-entered").exists() {
                assert!(Instant::now() < end);
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(matches!(inspect(&directory).unwrap(), Recovery::Active));
        }
        child.stdin.take();
        while child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(5));
        }
        let Recovery::Finished { result, .. } = inspect(&directory).unwrap() else {
            panic!("missing terminal result")
        };
        assert!(budget.charged().unwrap().retirement > 0);
        assert!(result.retirement.is_none()); // This injected acquisition never owns a device.
        if start {
            assert_eq!(result.error.as_deref(), Some("acquisition cancelled"));
        } else {
            assert!(!directory.join("acquisition-entered").exists());
        }
        let original = std::fs::read(directory.join("result.json")).unwrap();
        assert!(run(&directory).is_err());
        assert_eq!(
            std::fs::read(directory.join("result.json")).unwrap(),
            original
        );
    }
}
#[test]
fn original_results_distinguish_missing_uncertain_completed_and_conflicting_requests() {
    let root = tempfile::tempdir().unwrap();
    let id = RequestId::new("original").unwrap();
    assert!(
        read_result(root.path(), &id, &Operation::Read)
            .unwrap()
            .is_none()
    );
    let request = root.path().join("requests/original");
    std::fs::create_dir_all(&request).unwrap();
    save_atomic(&request.join("operation.json"), &Operation::Read).unwrap();
    assert!(
        read_result(root.path(), &id, &Operation::Read)
            .unwrap()
            .is_none()
    );
    assert!(read_result(root.path(), &id, &Operation::InspectFrames).is_err());
    save_atomic(
        &request.join("result.json"),
        &Reply::Completed {
            completion: Box::new(Completion::Read {
                snapshot: frontcap_core::native::android_webview::Snapshot {
                    starting_randomness: None, canvas_kit: None,
                    media: None, connection_console: None,
                    page_events: None,
                    timer_activity: None,
        frame_activity: None,
                    navigation: None,
                    href: "http://127.0.0.1/game".into(),
                    ready_state: "complete".into(),
                    visibility: "visible".into(),
                    focused: true,
                    performance_ms: 123.0,
                    date_ms: 456.0,
                },
            }),
        },
    )
    .unwrap();
    let original = std::fs::read(request.join("result.json")).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            read_result(root.path(), &id, &Operation::Read).unwrap(),
            Some(Reply::Completed { .. })
        ));
    }
    assert_eq!(
        std::fs::read(request.join("result.json")).unwrap(),
        original
    );
    assert!(read_result(root.path(), &id, &Operation::HomeReturn).is_err());
}

#[test]
fn initial_navigation_distinguishes_pending_host_load_from_settled_error() {
    let mut snapshot = frontcap_core::native::android_webview::Snapshot {
                    starting_randomness: None, canvas_kit: None,
                    media: None, connection_console: None,
                    page_events: None,
        timer_activity: None,
        frame_activity: None,
        navigation: None,
        href: "about:blank".into(),
        ready_state: "complete".into(),
        visibility: "visible".into(),
        focused: true,
        performance_ms: 0.0,
        date_ms: 0.0,
    };
    assert!(!snapshot.initial_navigation_finished());
    for href in ["http://127.0.0.1/host", "chrome-error://chromewebdata/"] {
        snapshot.href = href.into();
        snapshot.ready_state = "loading".into();
        assert!(!snapshot.initial_navigation_finished());
        snapshot.ready_state = "complete".into();
        assert!(snapshot.initial_navigation_finished());
    }
}


#[test]
fn saved_completion_rejects_another_operation_or_effect_without_changing_evidence() {
    use crate::android_timers::Action;
    let root = tempfile::tempdir().unwrap();
    let id = RequestId::new("advance").unwrap();
    let request = root.path().join("requests/advance");
    std::fs::create_dir_all(&request).unwrap();
    let operation = Operation::Timers { action: Action::Advance { milliseconds: 200 } };
    save_atomic(&request.join("operation.json"), &operation).unwrap();
    for (completion, accepted) in [
        (Completion::Frames { receipt: None }, false),
        (Completion::Timers { completion: crate::android_timers::Completion {
            document: "abcd0000-0000-0000-0000-000000000001".into(),
            action: Action::Pause, ticks_base: 42.0, budget_expired: false,
        } }, false),
        (Completion::Timers { completion: crate::android_timers::Completion {
            document: "abcd0000-0000-0000-0000-000000000001".into(),
            action: Action::Advance { milliseconds: 200 }, ticks_base: 42.0, budget_expired: true,
        } }, true),
    ] {
        save_atomic(&request.join("result.json"), &Reply::Completed { completion: Box::new(completion) }).unwrap();
        let original = std::fs::read(request.join("result.json")).unwrap();
        assert_eq!(read_result(root.path(), &id, &operation).is_ok(), accepted);
        assert_eq!(std::fs::read(request.join("result.json")).unwrap(), original);
    }
    let frame = crate::android_frames::Request {
        document: "abcd0000-0000-0000-0000-000000000001".into(), id: 1, timestamp: 20.0,
    };
    let operation = Operation::Frame { request: frame.clone() };
    save_atomic(&request.join("operation.json"), &operation).unwrap();
    for (returned_id, accepted) in [(1, true), (2, false)] {
        let mut returned = frame.clone();
        returned.id = returned_id;
        let completion = Completion::Frame { receipt: crate::android_frames::Receipt {
            document: frame.document.clone(), legacy_used: false,
            last: Some(crate::android_frames::Outcome {
                request: returned, state: crate::android_frames::State::Pending, result: None, error: None,
            }),
        } };
        save_atomic(&request.join("result.json"), &Reply::Completed { completion: Box::new(completion) }).unwrap();
        assert_eq!(read_result(root.path(), &id, &operation).is_ok(), accepted);
    }
    save_atomic(&request.join("result.json"), &Reply::Failed { error: "delivery uncertain".into() }).unwrap();
    assert!(matches!(read_result(root.path(), &id, &operation).unwrap(), Some(Reply::Failed { .. })));
}


#[test]
fn contact_recovery_uses_only_matching_verified_native_history() {
    use frontcap_core::native::{android_webview::{Debugger, ContactDriver, Prepared}, contacts};
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    File::create_new(root.path().join("session.lock")).unwrap();
    let id = RequestId::new("down").unwrap();
    let basis = RequestId::new("capture").unwrap();
    let input = contacts::Operation::Set { expected_revision: 0,
        contacts: vec![contacts::Contact { id: 1, x: 0.5, y: 0.5 }] };
    let operation = Operation::Contacts { observation: basis.clone(), input: input.clone() };
    let owner = Debugger {
        emulator: ProcessIdentity::of(std::process::id()).unwrap().unwrap(),
        package: "dev.example.consumer".into(), activity: "dev.example.consumer.Main".into(),
        serial: "emulator-5580".into(), pid: 123, process_start_ticks: 42,
        socket: "webview_devtools_remote_123".into(), apk_sha256: "a".repeat(64), source_sha256: "b".repeat(64),
    };
    let driver = ContactDriver { short_tap: false,
        generation: "abcd0000-0000-0000-0000-000000000001".into(), lifetime_ms: 30000,
        request_capacity: 24, pid: 456, process_start_ticks: 43,
        guest_directory: "/data/local/tmp/test".into(), jar_sha256: "c".repeat(64), consumer: frontcap_core::native::android_webview::ContactConsumer::WebView(owner.clone()),
    };
    let ready = Ready { owner, page_id: "page".into(), document: driver.generation.clone(),
        prepared: Prepared { random_seed:None, script_identifier: "script".into(), url: "http://localhost/game".into(), attachment_sha256: "d".repeat(64) },
        contacts: Some(driver.clone()) };
    let request = root.path().join("requests/down");
    std::fs::create_dir_all(&request).unwrap();
    save_atomic(&request.join("operation.json"), &operation).unwrap();
    save_atomic(&root.path().join("ready.json"), &ready).unwrap();
    let target = root.path().join("target/contact-driver");
    std::fs::create_dir_all(&target).unwrap();
    let native_id = child_id(&id, "input").to_string();
    let receipt = json!({"status":"injected", "contact_revision":1, "contacts":[{"id":1,"x":49.5,"y":39.5}]});
    let mut state = json!({"owner":driver, "next":2, "revision":1, "basis":null, "pending":null,
        "entries":{native_id.clone():{"observation":basis,"operation":input,"request":{},"reply":receipt}}});
    save_atomic(&target.join("state.json"), &state).unwrap();
    let original = std::fs::read(target.join("state.json")).unwrap();
    let wait_after_exit = || {
        // A real exited control child with no transport acknowledgement. Device effects remain
        // fixture evidence here; installed worker-loss and target retirement are separate checks.
        let mut child = Command::new("/bin/sh").args(["-c", "exit 0"])
            .stdin(Stdio::piped()).spawn().unwrap();
        let control = child.stdin.take();
        assert!(control.is_some());
        child.wait().unwrap();
        let mut session = Session { child: Some(child), control, directory: root.path().into(), number: 1 };
        let result = session.wait_reply(&id, &operation, Instant::now() + Duration::from_secs(1), &AtomicBool::new(false));
        assert!(session.control.is_none());
        // A recovered receipt must not reopen admission to this dead worker.
        assert!(session.execute(RequestId::new("fresh").unwrap(), Operation::Read,
            Instant::now() + Duration::from_secs(1), &AtomicBool::new(false)).is_err());
        assert!(matches!(session.stop(Instant::now() + Duration::from_secs(1)).unwrap(), Recovery::Unresolved));
        result
    };
    assert!(matches!(wait_after_exit().unwrap(), Reply::Completed { completion }
        if *completion == Completion::Contacts { receipt: receipt.clone() }));
    for _ in 0..2 {
        let Some(Reply::Completed { completion }) = read_result(root.path(), &id, &operation).unwrap() else { panic!() };
        assert_eq!(*completion, Completion::Contacts { receipt: receipt.clone() });
    }
    assert!(!request.join("result.json").exists());
    assert_eq!(std::fs::read(target.join("state.json")).unwrap(), original);
    for field in ["owner", "observation", "input"] {
        let mut changed = state.clone();
        match field {
            "owner" => changed["owner"]["process_start_ticks"] = json!(999),
            "observation" => changed["entries"][&native_id]["observation"] = json!("other"),
            _ => changed["entries"][&native_id]["operation"]["expected_revision"] = json!(7),
        }
        save_atomic(&target.join("state.json"), &changed).unwrap();
        assert!(read_result(root.path(), &id, &operation).is_err());
        assert!(wait_after_exit().is_err());
    }
    state["entries"][&native_id]["reply"] = serde_json::Value::Null;
    state["pending"] = json!(native_id);
    save_atomic(&target.join("state.json"), &state).unwrap();
    save_atomic(&target.join("producer-reply.json"), &receipt).unwrap();
    assert!(read_result(root.path(), &id, &operation).unwrap().is_none());
    assert!(wait_after_exit().is_err());
    assert!(!request.join("result.json").exists());
    save_atomic(&request.join("result.json"), &Reply::Failed { error: "original failure".into() }).unwrap();
    assert!(matches!(wait_after_exit().unwrap(), Reply::Failed { error } if error == "original failure"));
}

#[test]
fn independent_retirement_preserves_missing_worker_outcome_and_rejects_changed_proof() {
    use crate::resource_group::{self, Group};
    let fixture = crate::device::tests::Fixture::new();
    let root = fixture.root().canonicalize().unwrap();
    let mut device = fixture.config.clone();
    device.host_app = Some(crate::android_webview::Profile {
        package: "org.example.app".into(), activity: "org.example.app.ClientActivity".into(), local_service: None,
    });
    let config = Config { device, entry_url: String::new(), lifetime_ms: 30000, input: None };
    let group_config = resource_group::Config { host: root.join("host"), target: root.join("target"),
        evidence_root: Some(root.clone()), artifact_budget_bytes: None, temporary_storage_root: None, scratch_volume: None, scratch_volume_root: None, resource_executable: "/bin/sh".into(),
        resource_arguments: group_arguments("resource_entry"), ports: vec![config.device.port, config.device.port+1],
        deadline_ns: monotonic_ns().unwrap()+30_000_000_000 };
    File::create_new(root.join("session.lock")).unwrap();
    save_atomic(&root.join("launch.json"), &Launch { config, deadline_ns: group_config.deadline_ns }).unwrap();
    let mut command = Command::new("/bin/sh"); command.args(group_arguments("group_entry"));
    let mut group = Group::start(&mut command, &group_config, &root.join("target-resources"), Instant::now()+Duration::from_secs(5)).unwrap();
    group.start_resource(RequestId::new("emulator").unwrap(), Command::new("/bin/sleep").arg("300"),
        Instant::now()+Duration::from_secs(5), &AtomicBool::new(false)).unwrap();
    group.capture(&Command::new("/usr/bin/true"), &root.join("target/helper"),
        Instant::now()+Duration::from_secs(5), &AtomicBool::new(false), 4096).unwrap();
    // Real group evidence, with a minimal worker envelope whose original process is directly owned.
    let mut worker = Command::new("/bin/sleep").arg("300").spawn().unwrap();
    let identity = ProcessIdentity::of(worker.id()).unwrap().unwrap();
    save_atomic(&root.join("worker.json"), &identity).unwrap();
    assert!(matches!(inspect(&root).unwrap(), Recovery::Unresolved));
    worker.kill().unwrap(); worker.wait().unwrap();
    assert!(matches!(inspect(&root).unwrap(), Recovery::ResourceCleanup { .. }));
    assert!(group.stop(Instant::now()+Duration::from_secs(15)).unwrap().passed);
    // A missing worker envelope remains missing: only the independent cleanup can be recovered.
    assert!(!root.join("result.json").exists());
    let original = std::fs::read(root.join("target-resources/retirement.json")).unwrap();
    for _ in 0..2 {
        let outcome = inspect(&root).unwrap();
        assert!(matches!(outcome, Recovery::WorkerLost { .. }));
        assert!(outcome.retirement().unwrap().passed);
        assert!(read_result(&root, &RequestId::new("unpublished").unwrap(), &Operation::Read).unwrap().is_none());
    }
    assert_eq!(std::fs::read(root.join("target-resources/retirement.json")).unwrap(), original);
    assert!(!root.join("result.json").exists());
    // Later host reuse must not invalidate an original completed retirement or acquire its lease.
    save_atomic(&root.join("host/native-session.json"), &crate::host::HostSession { directory: root.join("later-target"), retired: false }).unwrap();
    assert!(inspect(&root).unwrap().retirement().unwrap().passed);
    let config_path = root.join("target-resources/config.json");
    let config_bytes = std::fs::read(&config_path).unwrap();
    let mut wrong: resource_group::Config = load(&config_path).unwrap().unwrap(); wrong.target = root.join("unrelated");
    save_atomic(&config_path, &wrong).unwrap(); assert!(inspect(&root).is_err());
    std::fs::write(config_path, config_bytes).unwrap();
    let helper = root.join("target/helper/exchange.json");
    let helper_bytes = std::fs::read(&helper).unwrap(); std::fs::write(&helper, b"changed original helper").unwrap();
    assert!(inspect(&root).is_err()); std::fs::write(&helper, helper_bytes).unwrap();
    assert!(inspect(&root).unwrap().retirement().unwrap().passed);
    assert!(!root.join("result.json").exists());
}
fn group_arguments(entry: &str) -> Vec<String> {
    vec!["-c".into(), format!("export FRONTCAP_RESOURCE_GROUP_DIRECTORY=\"$2\"; exec \"$1\" --exact resource_group::tests::{entry} --nocapture"),
        "retirement-reader".into(), std::env::current_exe().unwrap().to_str().unwrap().into()]
}
