use super::*;
use frontcap_core::{
    canonical::sha256_bytes_hex,
    ids::RunId,
    native::request::{Completion, NativeRequest, Operation},
    process::ProcessIdentity,
};

struct Fixture {
    root: tempfile::TempDir,
    launch: LaunchId,
    id: RequestId,
    ready: frontcap_core::native::android_app::Owner,
    store: RequestStore,
}

#[test]
fn flutter_external_entry_recovers_only_original_acceptance_after_recorder_exit() {
    use frontcap_core::native::{external_entry::{Request, Target}, flutter::{Owner, Platform}};
    use frontcap_native::{external_entry::Dispatches, flutter_session};
    for (platform, device, target) in [
        (Platform::Android, "emulator-5580", Target::Android { serial: "emulator-5580".into(), package: "org.example.client".into() }),
        (Platform::Ios, "ABCD0000-0000-0000-0000-000000000001", Target::Ios { simulator: "abcd0000-0000-0000-0000-000000000001".into(), bundle: "org.example.client".into() }),
    ] {
        let f = Fixture::new(); let root = f.root.path(); let session = root.join("native-session");
        let owner = Owner { platform: platform.clone(), device: device.into(), generation: "a".repeat(48), source_sha256: "b".repeat(64) };
        let config: frontcap_native::flutter_target::Launch = serde_json::from_value(serde_json::json!({
            "worker":{"python":"/unused", "worker_script":"/unused", "config":{"flutter":"/unused", "project":"/unused",
                "platform":platform, "source_sha256":owner.source_sha256, "acquisition_timeout_s":5, "lifetime_s":60}}, "http_script":"/unused"
        })).unwrap();
        let mut spec: LaunchSpec = required(&super::super::spec_path(root)).unwrap();
        spec.target = crate::native::target::Launch::Flutter(config.clone());
        save_atomic(&super::super::spec_path(root), &spec).unwrap();
        save_atomic(&session.join("launch.json"), &config).unwrap();
        save_atomic(&session.join("ready.json"), &owner).unwrap();
        std::fs::create_dir_all(session.join("session/worker")).unwrap();
        let ready = flutter_session::Ready { system_ui: None, owner: owner.clone(),
            endpoint: frontcap_native::flutter_bridge::Endpoint { port: 12345, token: "c".repeat(48) },
            external_url_target: Some(target.clone()) };
        let worker_ready = session.join("session/worker/ready.json");
        save_atomic(&worker_ready, &ready).unwrap();
        let request = Request { target, generation: owner.generation.clone(), url: "client-test://item/1".into() };
        let op = Op::DispatchUrl { request: request.clone() };
        let id = RequestId::new("entry").unwrap(); let lost = RequestId::new("lost-entry").unwrap();
        let mut journal = Journal::open(&runtime::journal_path(root)).unwrap();
        for id in [&id, &lost] {
            journal.begin(id, &args_sha256(&serde_json::to_value((&op, 5000_u64)).unwrap())).unwrap();
            record(root, id, &op, 5000).unwrap(); journal.dispatched(id).unwrap();
        }
        let lookup = |id: &RequestId| saved_result(root, &f.launch, id);
        let pending = RequestResult::Pending { dispatched: true };
        assert_eq!(lookup(&id).unwrap(), pending);
        let directory = session.join("external-entry");
        let mut entries = Dispatches::create(&directory, request.target.clone(), request.generation.clone()).unwrap();
        let accepted = entries.dispatch(&id, request.clone(), |_| Ok(())).unwrap();
        assert!(entries.dispatch(&lost, request.clone(), |_| Err(io::Error::other("lost OS reply"))).is_err());
        drop(entries);
        let original = std::fs::read(runtime::journal_path(root)).unwrap();
        let history = std::fs::read(directory.join("requests.jsonl")).unwrap();
        let expected = RequestResult::Completed { answer: Box::new(Answer::Ok(Reply::Native {
            body: NativeReply::Done { request_id: id.clone(), outcome: Box::new(Outcome::ExternalUrlAccepted { acceptance: accepted }) },
        })) };
        assert_eq!(lookup(&id).unwrap(), expected);
        assert_eq!(lookup(&lost).unwrap(), pending);
        let mut changed = ready.clone(); changed.external_url_target = None;
        save_atomic(&worker_ready, &changed).unwrap(); assert!(lookup(&id).is_err());
        changed = ready.clone(); changed.owner.generation = "d".repeat(48);
        save_atomic(&worker_ready, &changed).unwrap(); assert!(lookup(&id).is_err());
        save_atomic(&worker_ready, &ready).unwrap();
        let mut changed = config; changed.worker.config.source_sha256 = "d".repeat(64);
        save_atomic(&session.join("launch.json"), &changed).unwrap(); assert!(lookup(&id).is_err());
        assert_eq!(std::fs::read(runtime::journal_path(root)).unwrap(), original);
        assert_eq!(std::fs::read(directory.join("requests.jsonl")).unwrap(), history);
    }
}

#[test]
fn external_entry_recovery_requires_original_acceptance_and_frozen_owner_without_replay() {
    use frontcap_core::native::{android_app::Owner, external_entry::{Request, Target}};
    use frontcap_native::external_entry::Dispatches;
    let f = Fixture::new();
    let root = f.root.path();
    let session = root.join("native-session");
    let owner = Owner { package: "org.example.client".into(), activity: "org.example.client.Main".into(),
        serial: "emulator-5580".into(), generation: f.ready.generation.clone(), pid: 123,
        source_sha256: "a".repeat(64), apk_sha256: "b".repeat(64) };
    let mut spec: LaunchSpec = required(&super::super::spec_path(root)).unwrap();
    let crate::native::target::Launch::Android(config) = &mut spec.target else { panic!() };
    config.view_app = Some(frontcap_native::android_app::Profile {
        launch_extras: Default::default(), service_url_extra: None,
        package: owner.package.clone(), activity: owner.activity.clone(),
        capture_script: "/unused".into(), clock_script: "/unused".into(), edit_script: "/unused".into(),
        tap_script: "/unused".into(), scroll_script: "/unused".into(),
    });
    save_atomic(&super::super::spec_path(root), &spec).unwrap();
    save_atomic(&session.join("view-ready.json"), &owner).unwrap();
    let request = Request { target: Target::Android { serial: owner.serial.clone(), package: owner.package.clone() },
        generation: owner.generation.clone(), url: "client-test://item/1".into() };
    let op = Op::DispatchUrl { request: request.clone() };
    let id = RequestId::new("entry").unwrap();
    let lost = RequestId::new("lost-entry").unwrap();
    let mut journal = Journal::open(&runtime::journal_path(root)).unwrap();
    for id in [&id, &lost] {
        journal.begin(id, &args_sha256(&serde_json::to_value((&op, 5000_u64)).unwrap())).unwrap();
        record(root, id, &op, 5000).unwrap();
        journal.dispatched(id).unwrap();
    }
    let lookup = |id: &RequestId| saved_result(root, &f.launch, id);
    let pending = RequestResult::Pending { dispatched: true };
    assert_eq!(lookup(&id).unwrap(), pending);
    std::fs::create_dir_all(session.join("device")).unwrap();
    let directory = session.join("device/external-entry");
    let mut dispatches = Dispatches::create(&directory, request.target.clone(), request.generation.clone()).unwrap();
    assert_eq!(lookup(&id).unwrap(), pending);
    let accepted = dispatches.dispatch(&id, request.clone(), |_| Ok(())).unwrap();
    assert!(dispatches.dispatch(&lost, request.clone(), |_| Err(io::Error::other("lost OS reply"))).is_err());
    drop(dispatches);
    let recorder_history = std::fs::read(runtime::journal_path(root)).unwrap();
    let delivery_history = std::fs::read(directory.join("requests.jsonl")).unwrap();
    let expected = RequestResult::Completed { answer: Box::new(Answer::Ok(Reply::Native {
        body: NativeReply::Done { request_id: id.clone(), outcome: Box::new(Outcome::ExternalUrlAccepted { acceptance: accepted }) },
    })) };
    assert_eq!(lookup(&id).unwrap(), expected);
    assert_eq!(lookup(&id).unwrap(), expected);
    assert_eq!(lookup(&lost).unwrap(), pending);

    let original_runtime = runtime::load_runtime(root).unwrap().unwrap();
    let mut live = original_runtime.clone(); live.recorder = ProcessIdentity::current();
    runtime::save_runtime(root, &live).unwrap();
    assert_eq!(lookup(&id).unwrap(), pending);
    runtime::save_runtime(root, &original_runtime).unwrap();

    for field in ["source", "generation", "package"] {
        let mut changed = owner.clone();
        match field {
            "source" => changed.source_sha256 = "c".repeat(64),
            "generation" => changed.generation = "abcd0000-0000-0000-0000-000000000009".into(),
            _ => changed.package = "dev.other.client".into(),
        }
        save_atomic(&session.join("view-ready.json"), &changed).unwrap();
        assert!(lookup(&id).is_err());
    }
    save_atomic(&session.join("view-ready.json"), &owner).unwrap();
    let mut changed = request; changed.url = "client-test://item/2".into();
    save_atomic(&root.join("native-requests/entry.json"), &Intent {
        op: Op::DispatchUrl { request: changed }, budget_ms: 5000,
    }).unwrap();
    assert!(lookup(&id).is_err());
    assert_eq!(std::fs::read(runtime::journal_path(root)).unwrap(), recorder_history);
    assert_eq!(std::fs::read(directory.join("requests.jsonl")).unwrap(), delivery_history);
}
impl Fixture {
    fn new() -> Self { Self::with_session_root(None) }
    fn with_session_root(session_root: Option<&Path>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let launch = LaunchId::generate();
        let id = RequestId::new("read").unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let recorder = ProcessIdentity::of(child.id()).unwrap().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
        runtime::save_runtime(
            root.path(),
            &runtime::RuntimeRecord {
                launch_id: launch.clone(),
                state: runtime::RuntimeState::Ready,
                recorder,
                browser: None,
                endpoint: None,
                socket: runtime::socket_path(root.path()),
            },
        )
        .unwrap();
        let spec = LaunchSpec {
            request_id: RequestId::new("launch").unwrap(),
            launch_id: launch.clone(),
            run_id: RunId::new("run").unwrap(),
            target: crate::native::target::Launch::Android(
                frontcap_native::device::AndroidLaunch {
                    artifact_budget_bytes: None,
                    host_app: None,
            view_app: Some(frontcap_native::android_app::Profile { launch_extras: Default::default(), service_url_extra: None, package:"org.example.client".into(), activity:"org.example.client.Main".into(), capture_script:"/capture".into(), edit_script:"/edit".into(), tap_script:"/tap".into(), clock_script:"/clock".into(), scroll_script:"/scroll".into() }),
            webview_app: None,
                    emulator: "unused".into(),
                    adb: "unused".into(),
                    adb_server_port: None,
                    avd: "unused".into(),
                    avd_home: None,
                    avd_inventory: None,
                    temporary_storage_root: None,
                    scratch_volume: None,
                    confine_emulator_writes: false,
                    port: 5580,
                    cores: None,
                    memory_mb: None,
                    startup_diagnostics: false,
                    startup_broadcast_sample_ms: 0,
                    startup_broadcast_interval_ms: 250,
                    apk: "unused".into(),
                    apk_sha256: "b".repeat(64),
                    initial_data: None,
                    python: "unused".into(),
                    bridge_script: "unused".into(),
                    input_registration: Default::default(),
                    clock_transport: Default::default(),
                    source_sha256: "a".repeat(64),
                },
            ),
            session_executable: "unused".into(),
            acquisition_budget_ms: 5000,
            min_free_bytes: None,
            write_budget: None,
            session_root: session_root.map(Path::to_path_buf),
            session_volume: None,
            service: None,
        };
        save_atomic(&super::super::spec_path(root.path()), &spec).unwrap();
        let ready = frontcap_core::native::android_app::Owner {
            serial:"emulator-5580".into(), package:"org.example.client".into(), activity:"org.example.client.Main".into(),
            generation:"4d75203e-0a3e-4c68-8e3b-77f0a831ab83".into(), pid:123,
            source_sha256: "a".repeat(64),
            apk_sha256: "b".repeat(64),
        };
        let session = session_root.map(|parent| parent.join(launch.as_str()))
            .unwrap_or_else(|| root.path().join("native-session"));
        std::fs::create_dir(&session).unwrap();
        if let Some(parent) = session_root {
            let mut roots = super::super::storage::Roots::runtime(root.path()).unwrap();
            roots.route_session(parent, &launch).unwrap();
            save_atomic(&root.path().join("native-storage-roots.json"), &roots).unwrap();
            super::super::storage::record_session_location(root.path(), &spec, None).unwrap();
            roots.register_session(&session).unwrap();
            save_atomic(&root.path().join("native-storage-acquired-roots.json"), &roots).unwrap();
        }
        save_atomic(&session.join("view-ready.json"), &ready).unwrap();
        let store = RequestStore::create(
            &session.join("requests"),
            ready.identity(),
            ready.generation.clone(),
            ready.source_sha256.clone(),
        )
        .unwrap();
        let op = Op::Execute {
            operation: Operation::ReadClock {},
        };
        let mut journal = Journal::open(&runtime::journal_path(root.path())).unwrap();
        journal
            .begin(
                &id,
                &args_sha256(&serde_json::to_value((&op, 5000_u64)).unwrap()),
            )
            .unwrap();
        record(root.path(), &id, &op, 5000).unwrap();
        journal.dispatched(&id).unwrap();
        Self {
            root,
            launch,
            id,
            ready,
            store,
        }
    }
    fn result(&self) -> io::Result<RequestResult> {
        saved_result(self.root.path(), &self.launch, &self.id)
    }
    fn complete(&mut self) -> (Completion, std::path::PathBuf) {
        self.store
            .prepare(self.id.clone(), Operation::ReadClock {})
            .unwrap();
        let NativeRequest::Clock(request) = self.store.dispatch(&self.id).unwrap() else {
            panic!()
        };
        let bytes = serde_json::to_vec(&serde_json::json!({
            "status": "ok", "request": request, "generation": self.ready.generation,
            "pid": 123, "last_id": 1, "before_ns": 0, "now_ns": 0, "applied": false
        }))
        .unwrap();
        let digest = sha256_bytes_hex(&bytes);
        let dir = super::super::session_directory(self.root.path()).unwrap().join("device/view-clock/receipts");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{digest}.json"));
        std::fs::write(&path, bytes).unwrap();
        let completion = Completion::Clock {
            request,
            generation: self.ready.generation.clone(),
            pid: 123,
            last_id: 1,
            before_ns: 0,
            now_ns: 0,
            applied: false,
            receipt_sha256: digest,
        };
        self.store.complete(&self.id, completion.clone()).unwrap();
        (completion, path)
    }
}

#[test]
fn uikit_external_entry_recovery_reads_only_acceptance_bound_to_frozen_owner() {
    use frontcap_core::native::{external_entry::{Request, Target}, ios_request::Owner};
    use frontcap_native::external_entry::Dispatches;
    let f = Fixture::new();
    let root = f.root.path();
    let mut config: frontcap_native::ios_target::IosLaunch =
        serde_json::from_str(include_str!("../../tests/fixtures/native/ios-launch.json")).unwrap();
    config.surface = frontcap_native::ios_execution::UiSurface::Uikit;
    let mut spec: LaunchSpec = required(&super::super::spec_path(root)).unwrap();
    spec.target = super::super::target::Launch::Ios(config.clone());
    save_atomic(&super::super::spec_path(root), &spec).unwrap();
    let owner: Owner = serde_json::from_value(serde_json::json!({
        "runner": {"simulator":"abcd0000-0000-0000-0000-000000000001", "generation":"abcd0000-0000-0000-0000-000000000002", "pid":41},
        "app": {"simulator":"abcd0000-0000-0000-0000-000000000001", "generation":"abcd0000-0000-0000-0000-000000000003", "pid":42},
        "controller_id":config.controller_id, "source_sha256":config.xctest.simulator.source_sha256
    })).unwrap();
    let target = root.join("native-session/target");
    std::fs::create_dir(&target).unwrap();
    save_atomic(&target.join("request-owner.json"), &owner).unwrap();
    save_atomic(&target.join("launch.json"), &config.xctest).unwrap();
    save_atomic(&root.join("native-session/ready.json"), &owner).unwrap();
    let request = Request { target: Target::Ios { simulator: owner.app.simulator.clone(), bundle: config.xctest.app.bundle.clone() },
        generation: owner.app.generation.clone(), url: "client-test://item/1".into() };
    let op = Op::DispatchUrl { request: request.clone() };
    let id = RequestId::new("entry").unwrap();
    let lost = RequestId::new("lost-entry").unwrap();
    let mut journal = Journal::open(&runtime::journal_path(root)).unwrap();
    for id in [&id, &lost] {
        journal.begin(id, &args_sha256(&serde_json::to_value((&op, 5000_u64)).unwrap())).unwrap();
        record(root, id, &op, 5000).unwrap();
        journal.dispatched(id).unwrap();
    }
    let lookup = |id: &RequestId| saved_result(root, &f.launch, id);
    let pending = RequestResult::Pending { dispatched: true };
    assert_eq!(lookup(&id).unwrap(), pending);
    let directory = target.join("external-entry");
    let mut dispatches = Dispatches::create(&directory, request.target.clone(), request.generation.clone()).unwrap();
    assert_eq!(lookup(&id).unwrap(), pending);
    let acceptance = dispatches.dispatch(&id, request.clone(), |_| Ok(())).unwrap();
    assert!(dispatches.dispatch(&lost, request.clone(), |_| Err(io::Error::other("lost OS reply"))).is_err());
    drop(dispatches);
    let history = std::fs::read(runtime::journal_path(root)).unwrap();
    let delivery = std::fs::read(directory.join("requests.jsonl")).unwrap();
    let expected = RequestResult::Completed { answer: Box::new(Answer::Ok(Reply::Native {
        body: NativeReply::Done { request_id: id.clone(), outcome: Box::new(Outcome::ExternalUrlAccepted { acceptance }) },
    })) };
    for _ in 0..2 {
        assert_eq!(lookup(&id).unwrap(), expected);
        assert_eq!(lookup(&lost).unwrap(), pending);
    }
    let mut changed = owner.clone(); changed.app.generation = owner.runner.generation.clone();
    save_atomic(&root.join("native-session/ready.json"), &changed).unwrap();
    assert!(lookup(&id).is_err());
    save_atomic(&root.join("native-session/ready.json"), &owner).unwrap();
    for change in ["bundle", "source"] {
        let mut changed = config.clone();
        match change {
            "bundle" => changed.xctest.app.bundle = "dev.other.client".into(),
            _ => changed.xctest.simulator.source_sha256 = "c".repeat(64),
        }
        spec.target = super::super::target::Launch::Ios(changed);
        save_atomic(&super::super::spec_path(root), &spec).unwrap();
        assert!(lookup(&id).is_err());
    }
    spec.target = super::super::target::Launch::Ios(config);
    save_atomic(&super::super::spec_path(root), &spec).unwrap();
    assert_eq!(lookup(&id).unwrap(), expected);
    assert_eq!(std::fs::read(runtime::journal_path(root)).unwrap(), history);
    assert_eq!(std::fs::read(directory.join("requests.jsonl")).unwrap(), delivery);
}

#[test]
fn exited_recorder_recovers_exact_completion_without_rewriting_dispatched_history() {
    let mut f = Fixture::new();
    let (completion, _) = f.complete();
    let journal_path = runtime::journal_path(f.root.path());
    let before = std::fs::read(&journal_path).unwrap();
    let expected = RequestResult::Completed {
        answer: Box::new(Answer::Ok(Reply::Native {
            body: NativeReply::Done {
                request_id: f.id.clone(),
                outcome: Box::new(Outcome::Completed { completion }),
            },
        })),
    };
    assert_eq!(f.result().unwrap(), expected);
    assert_eq!(f.result().unwrap(), expected);
    assert_eq!(std::fs::read(journal_path).unwrap(), before);
    assert!(matches!(
        Journal::read_state(&runtime::journal_path(f.root.path()), &f.id).unwrap(),
        Some(RequestState::Dispatched)
    ));
}

#[test]
fn incomplete_or_legacy_evidence_never_authorizes_replay_or_claims_completion() {
    let mut f = Fixture::new();
    assert_eq!(
        f.result().unwrap(),
        RequestResult::Pending { dispatched: true }
    );
    f.store
        .prepare(f.id.clone(), Operation::ReadClock {})
        .unwrap();
    assert_eq!(
        f.result().unwrap(),
        RequestResult::Pending { dispatched: true }
    );
    f.store.dispatch(&f.id).unwrap();
    assert_eq!(
        f.result().unwrap(),
        RequestResult::Pending { dispatched: true }
    );
    std::fs::remove_file(f.root.path().join("native-requests/read.json")).unwrap();
    assert_eq!(
        f.result().unwrap(),
        RequestResult::Pending { dispatched: true }
    );
}

#[test]
fn live_recorder_remains_authority_even_with_committed_session_result() {
    let mut f = Fixture::new();
    f.complete();
    let mut runtime = runtime::load_runtime(f.root.path()).unwrap().unwrap();
    runtime.recorder = ProcessIdentity::current();
    runtime::save_runtime(f.root.path(), &runtime).unwrap();
    assert_eq!(
        f.result().unwrap(),
        RequestResult::Pending { dispatched: true }
    );
}

#[test]
fn altered_receipt_identity_or_operation_cannot_be_recovered_as_success() {
    let mut f = Fixture::new();
    let (_, receipt) = f.complete();
    let bytes = std::fs::read(&receipt).unwrap();
    std::fs::write(&receipt, b"{}").unwrap();
    assert!(f.result().is_err());
    std::fs::write(&receipt, bytes).unwrap();
    let ready_path = f.root.path().join("native-session/view-ready.json");
    let mut foreign = f.ready.clone();
    foreign.package = "org.example.other".into();
    save_atomic(&ready_path, &foreign).unwrap();
    assert!(f.result().is_err());
    save_atomic(&ready_path, &f.ready).unwrap();
    save_atomic(
        &f.root.path().join("native-requests/read.json"),
        &Intent {
            op: Op::Execute {
                operation: Operation::Advance { by_us: 1 },
            },
            budget_ms: 5000,
        },
    )
    .unwrap();
    assert!(f.result().is_err());
}

#[test]
fn stop_recovery_requires_confirmed_retirement_and_released_session_ownership() {
    let f = Fixture::new();
    let id = RequestId::new("stop").unwrap();
    let mut journal = Journal::open(&runtime::journal_path(f.root.path())).unwrap();
    journal
        .begin(
            &id,
            &args_sha256(&serde_json::to_value((Op::Stop, 5000_u64)).unwrap()),
        )
        .unwrap();
    record(f.root.path(), &id, &Op::Stop, 5000).unwrap();
    journal.dispatched(&id).unwrap();
    let session_dir = f.root.path().join("native-session");
    let lock = std::fs::File::create(session_dir.join("session.lock")).unwrap();
    save_atomic(
        &session_dir.join("worker.json"),
        &ProcessIdentity::current(),
    )
    .unwrap();
    let result_path = session_dir.join("result.json");
    let retirement = frontcap_core::native::ownership::Retirement {
        passed: true,
        errors: vec![],
        signal_error: None,
    };
    save_atomic(
        &result_path,
        &session::SessionResult {
            error: None,
            retirement: Some(retirement.clone()),
        },
    )
    .unwrap();
    lock.lock().unwrap();
    assert_eq!(
        saved_result(f.root.path(), &f.launch, &id).unwrap(),
        RequestResult::Pending { dispatched: true }
    );
    lock.unlock().unwrap();
    assert!(matches!(
        saved_result(f.root.path(), &f.launch, &id).unwrap(),
        RequestResult::Completed { .. }
    ));
    // Target retirement alone cannot complete a Stop that also owns a retained session volume.
    let spec_path = super::super::spec_path(f.root.path());
    let original_spec: LaunchSpec = required(&spec_path).unwrap();
    let mut bounded = original_spec.clone();
    let mut ios: frontcap_native::ios_target::IosLaunch = serde_json::from_str(
        include_str!("../../tests/fixtures/native/ios-launch.json")).unwrap();
    ios.surface = frontcap_native::ios_execution::UiSurface::Uikit;
    bounded.target = super::super::target::Launch::Ios(ios);
    bounded.session_volume = Some(frontcap_native::storage_volume::Config {
        python: "/usr/bin/python3".into(), worker: "/worker.py".into(), size_mib: 256, reserve_mib: 8,
    });
    save_atomic(&spec_path, &bounded).unwrap();
    assert_eq!(saved_result(f.root.path(), &f.launch, &id).unwrap(), RequestResult::Pending { dispatched: true });
    save_atomic(&spec_path, &original_spec).unwrap();
    let failed = frontcap_core::native::ownership::Retirement {
        passed: false,
        errors: vec!["target still live".into()],
        ..retirement
    };
    save_atomic(
        &result_path,
        &session::SessionResult {
            error: None,
            retirement: Some(failed),
        },
    )
    .unwrap();
    assert_eq!(
        saved_result(f.root.path(), &f.launch, &id).unwrap(),
        RequestResult::Pending { dispatched: true }
    );
}

#[test]
fn ios_lost_recorder_results_use_frozen_surface_and_verified_receipts() {
    use frontcap_core::native::{
        ios::{ClockCompletion, ClockIdentity, ClockStatus},
        ios_request::{
            Completion as IosCompletion, Operation as IosOperation, Owner, Request as IosRequest,
        },
        ios_ui::UiIdentity,
    };
    use frontcap_native::ios_store::RequestStore as IosStore;
    let f = Fixture::new();
    let config: frontcap_native::ios_target::IosLaunch =
        serde_json::from_str(include_str!("../../tests/fixtures/native/ios-launch.json")).unwrap();
    let mut spec: LaunchSpec = required(&super::super::spec_path(f.root.path())).unwrap();
    spec.target = super::super::target::Launch::Ios(config.clone());
    save_atomic(&super::super::spec_path(f.root.path()), &spec).unwrap();
    let owner = Owner {
        runner: UiIdentity {
            simulator: "abcd0000-0000-0000-0000-000000000001".into(),
            generation: "abcd0000-0000-0000-0000-000000000002".into(),
            pid: 41,
        },
        app: ClockIdentity {
            simulator: "abcd0000-0000-0000-0000-000000000001".into(),
            generation: "abcd0000-0000-0000-0000-000000000003".into(),
            pid: 42,
        },
        controller_id: config.controller_id.clone(),
        source_sha256: config.xctest.simulator.source_sha256.clone(),
    };
    let target = f.root.path().join("native-session/target");
    std::fs::create_dir(&target).unwrap();
    save_atomic(&target.join("request-owner.json"), &owner).unwrap();
    save_atomic(&target.join("launch.json"), &config.xctest).unwrap();
    save_atomic(&f.root.path().join("native-session/ready.json"), &owner).unwrap();
    let id = RequestId::new("ios-read").unwrap();
    let op = Op::ExecuteIos {
        operation: IosOperation::ReadClock,
    };
    let mut journal = Journal::open(&runtime::journal_path(f.root.path())).unwrap();
    journal
        .begin(
            &id,
            &args_sha256(&serde_json::to_value((&op, 5000u64)).unwrap()),
        )
        .unwrap();
    record(f.root.path(), &id, &op, 5000).unwrap();
    journal.dispatched(&id).unwrap();
    drop(journal);
    let history = std::fs::read(runtime::journal_path(f.root.path())).unwrap();
    let mut store = IosStore::create(&target.join("requests"), owner.clone()).unwrap();
    assert_eq!(
        saved_result(f.root.path(), &f.launch, &id).unwrap(),
        RequestResult::Pending { dispatched: true }
    );
    store.prepare(id.clone(), IosOperation::ReadClock).unwrap();
    let IosRequest::Clock(request) = store.dispatch(&id).unwrap() else {
        panic!()
    };
    assert_eq!(
        saved_result(f.root.path(), &f.launch, &id).unwrap(),
        RequestResult::Pending { dispatched: true }
    );
    let raw=serde_json::to_vec(&serde_json::json!({"request":request,"generation":owner.app.generation,"pid":42,"last_id":1,"status":"ok","before_ns":0,"now_ns":0,"applied":false,"advance_result":null,"drain_result":null,"port":1234})).unwrap();
    let digest = sha256_bytes_hex(&raw);
    let receipts = target.join("clock-client/receipts");
    std::fs::create_dir_all(&receipts).unwrap();
    let path = receipts.join(format!("{digest}.json"));
    std::fs::write(&path, raw).unwrap();
    let completion = IosCompletion::Clock(ClockCompletion {
        request,
        generation: owner.app.generation.clone(),
        pid: 42,
        last_id: 1,
        status: ClockStatus::Ok,
        before_ns: 0,
        now_ns: 0,
        applied: false,
        advance_result: None,
        drain_result: None,
        receipt_sha256: digest,
    });
    store.complete(&id, completion.clone()).unwrap();
    let expected = RequestResult::Completed {
        answer: Box::new(Answer::Ok(Reply::Native {
            body: NativeReply::Done {
                request_id: id.clone(),
                outcome: Box::new(Outcome::IosCompleted { completion }),
            },
        })),
    };
    assert_eq!(
        saved_result(f.root.path(), &f.launch, &id).unwrap(),
        expected
    );
    assert_eq!(
        saved_result(f.root.path(), &f.launch, &id).unwrap(),
        expected
    );
    assert_eq!(
        std::fs::read(runtime::journal_path(f.root.path())).unwrap(),
        history
    );
    std::fs::write(path, b"corrupt").unwrap();
    assert!(saved_result(f.root.path(), &f.launch, &id).is_err());
    // An interrupted generic observation must remain pending after the recorder exits.
    // Reconstructing the legacy fixture operation would conflict with its saved binding.
    let mut generic = config.clone();
    generic.surface = frontcap_native::ios_execution::UiSurface::Uikit;
    spec.target = super::super::target::Launch::Ios(generic);
    save_atomic(&super::super::spec_path(f.root.path()), &spec).unwrap();
    let observed = RequestId::new("partial-observation").unwrap();
    let mut journal = Journal::open(&runtime::journal_path(f.root.path())).unwrap();
    journal.begin(&observed, &args_sha256(&serde_json::to_value((Op::Observe, 5000_u64)).unwrap())).unwrap();
    record(f.root.path(), &observed, &Op::Observe, 5000).unwrap();
    journal.dispatched(&observed).unwrap();
    drop(journal);
    store.prepare(observed.clone(), IosOperation::ObserveTargets).unwrap();
    store.dispatch(&observed).unwrap();
    let history = std::fs::read(runtime::journal_path(f.root.path())).unwrap();
    let bindings = std::fs::read(target.join("requests/requests.json")).unwrap();
    for _ in 0..2 {
        assert_eq!(saved_result(f.root.path(), &f.launch, &observed).unwrap(),
            RequestResult::Pending { dispatched: true });
    }
    assert_eq!(std::fs::read(runtime::journal_path(f.root.path())).unwrap(), history);
    assert_eq!(std::fs::read(target.join("requests/requests.json")).unwrap(), bindings);
    let mut changed = owner;
    changed.controller_id = "abcd0000-0000-0000-0000-000000000005".into();
    save_atomic(&f.root.path().join("native-session/ready.json"), &changed).unwrap();
    assert!(
        saved_result(f.root.path(), &f.launch, &id)
            .unwrap_err()
            .to_string()
            .contains("owner differs")
    );
}

#[test]
fn android_webview_recovery_reads_original_result_without_replaying_or_rewriting() {
    use frontcap_native::android_webview_session::{self as worker, Operation as A};
    let f = Fixture::new();
    let mut spec: LaunchSpec = required(&super::super::spec_path(f.root.path())).unwrap();
    let crate::native::target::Launch::Android(mut device) = spec.target else { panic!() };
    device.view_app = None;
    device.webview_app = Some(frontcap_native::android_webview::Profile {
        package: "dev.example.consumer".into(), activity: "dev.example.consumer.MainActivity".into(), local_service: None,
    });
    spec.target = crate::native::target::Launch::AndroidWebview(worker::Config {
        device, entry_url: "http://127.0.0.1/game".into(), lifetime_ms: 30000, input: None,
    });
    save_atomic(&super::super::spec_path(f.root.path()), &spec).unwrap();
    // The profile must survive runtime/platform decoding without falling into native Android.
    let decoded: LaunchSpec = required(&super::super::spec_path(f.root.path())).unwrap();
    assert!(matches!(decoded.target, crate::native::target::Launch::AndroidWebview(_)));
    let ready = frontcap_recorder_protocol::native::Ready::AndroidWebview {
        owner: frontcap_core::native::android_webview::Debugger {
            emulator: ProcessIdentity::of(std::process::id()).unwrap().unwrap(), package: "dev.example.consumer".into(), activity: "dev.example.consumer.MainActivity".into(), serial: "emulator-5580".into(), pid: 123, process_start_ticks: 42, socket: "webview_devtools_remote_123".into(), apk_sha256: "b".repeat(64), source_sha256: "a".repeat(64),
        }, contacts: false,
    };
    assert!(ready.validate_operation(&Op::ExecuteAndroidWebview { operation: A::Read }).is_ok());
    assert!(ready.validate_operation(&Op::ExecuteAndroidWebview { operation: A::Observe { input_basis: true } }).is_err());
    assert!(ready.validate_operation(&Op::ExecuteAndroidWebview { operation: A::Timers { action: frontcap_core::native::android_timers::Action::Advance { milliseconds: 0 } } }).is_err());
    assert!(ready.validate_operation(&Op::Execute { operation: Operation::ReadClock {} }).is_err());
    let frontcap_recorder_protocol::native::Ready::AndroidWebview { owner, .. } = &ready else { panic!() };
    let worker_ready = worker::Ready {
        owner: owner.clone(), page_id: "selected-page".into(), document: "abcd0000-0000-0000-0000-000000000001".into(),
        prepared: frontcap_core::native::android_webview::Prepared { random_seed:None, script_identifier: "script".into(), url: "http://127.0.0.1/game".into(), attachment_sha256: "c".repeat(64) }, contacts: None,
    };
    save_atomic(&f.root.path().join("native-session/ready.json"), &worker_ready).unwrap();
    let id = RequestId::new("android-page-read").unwrap();
    let op = Op::ExecuteAndroidWebview { operation: A::Read };
    let mut journal = Journal::open(&runtime::journal_path(f.root.path())).unwrap();
    journal.begin(&id, &args_sha256(&serde_json::to_value((&op, 5000_u64)).unwrap())).unwrap();
    record(f.root.path(), &id, &op, 5000).unwrap();
    journal.dispatched(&id).unwrap();
    let history = std::fs::read(runtime::journal_path(f.root.path())).unwrap();
    let dir = f.root.path().join("native-session/requests").join(id.as_str());
    std::fs::create_dir(&dir).unwrap();
    save_atomic(&dir.join("operation.json"), &A::Read).unwrap();
    assert_eq!(saved_result(f.root.path(), &f.launch, &id).unwrap(), RequestResult::Pending { dispatched: true });
    let completion = frontcap_core::native::android_webview::Completion::Read {
        snapshot: frontcap_core::native::android_webview::Snapshot {
            starting_randomness: None, canvas_kit: None,
            media: None, connection_console: None,
            page_events: None,
            frame_activity: Some(Box::new(frontcap_core::native::android_frames::Activity {
                document: worker_ready.document.clone(), pending: 2, dispatching: false,
                last_timestamp: Some(0.0), delivered: 1,
            })),
            timer_activity: Some(serde_json::from_value(serde_json::json!({"document":worker_ready.document,"sampledMs":42.0,
                "methodsObserved":true,"failure":null,"registered":2,"invoked":1,"canceled":0,
                "executing":0,"untrackedRegistrations":1,"uncertainCancellations":0,
                "tracked":1,"omitted":0,"timers":[{"id":4,"registration":2,"kind":"interval",
                    "registeredMs":20.0,"requestedDelayMs":10.0,"calls":1}]})).unwrap()),
            navigation: Some(serde_json::from_value(serde_json::json!({"document":worker_ready.document,
                "current":{"href":"http://127.0.0.1/game","hrefTruncated":false,"focused":true},
                "historyObserved":true,"failure":null,"received":0,"dropped":0,"events":[]})).unwrap()),
            href: "http://127.0.0.1/game".into(), ready_state: "complete".into(), visibility: "visible".into(), focused: true, performance_ms: 42.0, date_ms: 1000.0,
        },
    };
    save_atomic(&dir.join("result.json"), &worker::Reply::Completed { completion: Box::new(completion.clone()) }).unwrap();
    for _ in 0..2 {
        assert_eq!(saved_result(f.root.path(), &f.launch, &id).unwrap(), RequestResult::Completed {
            answer: Box::new(Answer::Ok(Reply::Native { body: NativeReply::Done {
                request_id: id.clone(), outcome: Box::new(Outcome::AndroidWebviewCompleted { completion: completion.clone() }),
            }})),
        });
    }
    // A retained timer observation is bound independently even when navigation is absent.
    let mut foreign = completion.clone();
    let frontcap_core::native::android_webview::Completion::Read { snapshot } = &mut foreign else { panic!() };
    snapshot.navigation = None;
    snapshot.timer_activity.as_mut().unwrap().document = "abcd0000-0000-0000-0000-000000000002".into();
    save_atomic(&dir.join("result.json"), &worker::Reply::Completed { completion: Box::new(foreign) }).unwrap();
    assert!(saved_result(f.root.path(), &f.launch, &id).is_err());
    save_atomic(&dir.join("result.json"), &worker::Reply::Completed { completion: Box::new(completion) }).unwrap();
    assert_eq!(std::fs::read(runtime::journal_path(f.root.path())).unwrap(), history);
    let ready_path = f.root.path().join("native-session/ready.json");
    for field in ["source", "apk", "device", "page", "document"] {
        let mut changed = worker_ready.clone();
        match field {
            "source" => changed.owner.source_sha256 = "d".repeat(64),
            "apk" => changed.owner.apk_sha256 = "d".repeat(64),
            "device" => changed.owner.serial = "emulator-5582".into(),
            "document" => changed.document = "abcd0000-0000-0000-0000-000000000009".into(),
            _ => changed.prepared.url = "http://127.0.0.1/other".into(),
        }
        save_atomic(&ready_path, &changed).unwrap();
        assert!(saved_result(f.root.path(), &f.launch, &id).is_err());
    }
    std::fs::remove_file(&ready_path).unwrap();
    assert_eq!(saved_result(f.root.path(), &f.launch, &id).unwrap(), RequestResult::Pending { dispatched: true });
    save_atomic(&ready_path, &worker_ready).unwrap();
    save_atomic(&dir.join("operation.json"), &A::HomeReturn).unwrap();
    assert!(saved_result(f.root.path(), &f.launch, &id).is_err());

    let failed_id = RequestId::new("android-home-failed").unwrap();
    let failed_op = Op::ExecuteAndroidWebview { operation: A::HomeReturn };
    journal.begin(&failed_id, &args_sha256(&serde_json::to_value((&failed_op, 5000_u64)).unwrap())).unwrap();
    record(f.root.path(), &failed_id, &failed_op, 5000).unwrap();
    journal.dispatched(&failed_id).unwrap();
    let failed_dir = f.root.path().join("native-session/requests").join(failed_id.as_str());
    std::fs::create_dir(&failed_dir).unwrap();
    save_atomic(&failed_dir.join("operation.json"), &A::HomeReturn).unwrap();
    let lifecycle = frontcap_core::native::android_progress::Lifecycle {
        uncertain: true, last_completed: None,
        pending: Some(frontcap_core::native::android_progress::Transition {
            id: RequestId::new("original-home-child").unwrap(),
            operation: frontcap_core::native::android_webview::WebViewLifecycle::Home,
            owner: owner.clone(), receipt: None,
        }),
    };
    let failure = worker::Reply::HomeReturnFailed { error: "page resume failed after uncertain Home".into(), lifecycle: Some(Box::new(lifecycle.clone())) };
    save_atomic(&failed_dir.join("result.json"), &failure).unwrap();
    let history = std::fs::read(runtime::journal_path(f.root.path())).unwrap();
    let original = std::fs::read(failed_dir.join("result.json")).unwrap();
    for _ in 0..2 {
        assert_eq!(saved_result(f.root.path(), &f.launch, &failed_id).unwrap(), RequestResult::Completed {
            answer: Box::new(Answer::Ok(Reply::Native { body: NativeReply::Done {
                request_id: failed_id.clone(), outcome: Box::new(Outcome::AndroidWebviewUnresolved {
                    error: "page resume failed after uncertain Home".into(), lifecycle: Some(lifecycle.clone()),
                }),
            }})),
        });
    }
    assert_eq!(std::fs::read(runtime::journal_path(f.root.path())).unwrap(), history);
    assert_eq!(std::fs::read(failed_dir.join("result.json")).unwrap(), original);
    let mut wrong = lifecycle;
    wrong.pending.as_mut().unwrap().owner.pid += 1;
    save_atomic(&failed_dir.join("result.json"), &worker::Reply::HomeReturnFailed { error: "wrong owner".into(), lifecycle: Some(Box::new(wrong)) }).unwrap();
    assert!(saved_result(f.root.path(), &f.launch, &failed_id).is_err());
}

#[test]
fn webview_clock_recovery_requires_matching_original_worker_and_page_evidence() {
    use frontcap_core::native::{request::ClockOperation, webview::{self, Operation as W}};
    use frontcap_native::{webview_session, webview_store};
    let f = Fixture::new();
    let root = f.root.path();
    let session = root.join("native-session");
    let config: webview_session::Config = serde_json::from_value(serde_json::json!({
        "simulator":{"xcrun":"/unused", "runtime":"ios", "device_type":"phone", "source_sha256":"a".repeat(64)},
        "app":{"app":"/unused/app", "bundle":"dev.example.game", "environment":{}, "mute_audio":true,
            "library":{"path":"/unused/library", "sha256":"b".repeat(64)},
            "script":{"path":"/unused/script", "sha256":"c".repeat(64)}}
    })).unwrap();
    let mut spec: LaunchSpec = required(&super::super::spec_path(root)).unwrap();
    spec.target = crate::native::target::Launch::Webview(Box::new(config.clone()));
    save_atomic(&super::super::spec_path(root), &spec).unwrap();
    save_atomic(&session.join("launch.json"), &serde_json::json!({"config":config,"deadline_ns":1})).unwrap();
    let owner = webview::Owner {
        simulator: "abcd0000-0000-0000-0000-000000000001".into(),
        process: ProcessIdentity { pid:42, started:"original-app".into() },
        endpoint: webview::Endpoint { generation:"abcd0000-0000-0000-0000-000000000002".into(), pid:42, port:12345 },
        attachment_sha256: frontcap_core::canonical::sha256_hex(&frontcap_core::canonical::canonical_of(&(
            &config.app.library.sha256, &config.app.script.as_ref().map(|script| &script.sha256), config.app.mute_audio))),
    };
    std::fs::create_dir_all(session.join("target/simulator")).unwrap();
    std::fs::create_dir_all(session.join("target/app")).unwrap();
    std::fs::create_dir_all(session.join("target/webview")).unwrap();
    save_atomic(&session.join("target/simulator/simulator.json"), &owner.simulator).unwrap();
    save_atomic(&session.join("target/simulator/launch.json"), &config.simulator).unwrap();
    save_atomic(&session.join("target/app/process.json"), &owner.process).unwrap();
    let id = RequestId::new("original-clock").unwrap();
    let operation = W::Clock { kind:ClockOperation::Read, delta_ns:0 };
    let op = Op::ExecuteWebview { operation:operation.clone() };
    let mut journal = Journal::open(&runtime::journal_path(root)).unwrap();
    journal.begin(&id, &args_sha256(&serde_json::to_value((&op, 5000_u64)).unwrap())).unwrap();
    record(root, &id, &op, 5000).unwrap(); journal.dispatched(&id).unwrap();
    let pending = RequestResult::Pending { dispatched:true };
    let lookup = || saved_result(root, &f.launch, &id);
    assert_eq!(lookup().unwrap(), pending);
    let command = session.join("commands/1"); std::fs::create_dir_all(&command).unwrap();
    let binding = serde_json::json!({"command":"execute","number":1,"id":id,"operation":operation,"deadline_ns":1});
    save_atomic(&command.join("command.json"), &binding).unwrap();
    assert_eq!(lookup().unwrap(), pending);
    let store_dir = session.join("target/webview/requests");
    let mut store = webview_store::RequestStore::create(&store_dir, owner.clone()).unwrap();
    store.prepare(id.clone(), ClockOperation::Read, 0).unwrap();
    let request = store.dispatch(&id).unwrap();
    let clock: webview::Reply = serde_json::from_value(serde_json::json!({
        "generation":owner.endpoint.generation,"pid":42,"port":12345,"request":request,"last_id":1,
        "now_ns":0,"replayed":false,"status":"ok","applied":false,
        "document":"abcd0000-0000-0000-0000-000000000003",
        "state":{"elapsedMs":0.0,"dateMs":1700000000000.0,"performanceMs":0.0,"advancing":false,
            "failure":null,"pendingTimers":1,"cancelledOnFailure":0,
            "timerQueue":{"timers":[{"id":1000000000000_u64,"kind":"timeout","dueMs":400.0}],"omitted":0,"failure":null}},
        "progress":null,"error":null
    })).unwrap();
    let completion = webview::Completion::Clock { reply:clock.clone() };
    let result = webview_session::Reply::Completed { completion:Box::new(completion.clone()) };
    save_atomic(&command.join("result.json"), &result).unwrap();
    assert_eq!(lookup().unwrap(), pending, "worker publication alone cannot resolve an unfinished clock journal");
    store.complete(&id, clock).unwrap(); drop(store);
    let history = std::fs::read(runtime::journal_path(root)).unwrap();
    let page = std::fs::read(store_dir.join("requests.json")).unwrap();
    let expected = RequestResult::Completed { answer:Box::new(Answer::Ok(Reply::Native {
        body:NativeReply::Done { request_id:id.clone(), outcome:Box::new(Outcome::WebviewCompleted { completion: Box::new(completion) }) },
    })) };
    assert_eq!(lookup().unwrap(), expected); assert_eq!(lookup().unwrap(), expected);
    let mut changed = binding.clone(); changed["operation"]["delta_ns"] = 1.into();
    save_atomic(&command.join("command.json"), &changed).unwrap(); assert!(lookup().is_err());
    save_atomic(&command.join("command.json"), &binding).unwrap();
    let mut changed = serde_json::to_value(&result).unwrap();
    changed["completion"]["reply"]["state"]["timerQueue"]["timers"][0]["dueMs"] = 401.0.into();
    save_atomic(&command.join("result.json"), &changed).unwrap(); assert!(lookup().is_err());
    save_atomic(&command.join("result.json"), &result).unwrap();
    save_atomic(&session.join("target/app/process.json"), &ProcessIdentity {pid:43,started:"replacement".into()}).unwrap();
    assert!(lookup().is_err());
    save_atomic(&session.join("target/app/process.json"), &owner.process).unwrap();
    // Current ready may describe a replacement; original epoch provenance remains authoritative.
    save_atomic(&session.join("ready.json"), &serde_json::json!({"replacement":true})).unwrap();
    assert_eq!(lookup().unwrap(), expected);
    assert_eq!(std::fs::read(runtime::journal_path(root)).unwrap(), history);
    assert_eq!(std::fs::read(store_dir.join("requests.json")).unwrap(), page);
    std::fs::remove_file(command.join("result.json")).unwrap();
    assert_eq!(lookup().unwrap(), pending);
}

#[test]
fn android_host_recovery_preserves_original_capture_and_rejects_page_authority_or_foreign_owner() {
    use frontcap_core::native::{android_host as host, android_webview::{AppProcess, Completion as W}};
    use frontcap_native::android_webview_session as worker;
    let f = Fixture::new();
    let root = f.root.path();
    let session = root.join("native-session");
    let mut spec: LaunchSpec = required(&super::super::spec_path(root)).unwrap();
    let crate::native::target::Launch::Android(mut device) = spec.target else { panic!() };
    device.view_app = None;
    device.host_app = Some(frontcap_native::android_webview::Profile {
        package: "dev.example.consumer".into(), activity: "dev.example.consumer.MainActivity".into(), local_service: None,
    });
    let config = worker::Config { device, entry_url: String::new(), lifetime_ms: 30000, input: None };
    spec.target = crate::native::target::Launch::AndroidHost(config.clone());
    save_atomic(&super::super::spec_path(root), &spec).unwrap();
    let decoded: LaunchSpec = required(&super::super::spec_path(root)).unwrap();
    assert!(matches!(decoded.target, crate::native::target::Launch::AndroidHost(_)));
    save_atomic(&session.join("launch.json"), &serde_json::json!({"config":config,"deadline_ns":1})).unwrap();
    let owner = AppProcess { emulator: ProcessIdentity::of(std::process::id()).unwrap().unwrap(),
        package: "dev.example.consumer".into(), activity: "dev.example.consumer.MainActivity".into(),
        serial: "emulator-5580".into(), pid:123, process_start_ticks:42, apk_sha256:"b".repeat(64), source_sha256:"a".repeat(64) };
    let ready = frontcap_recorder_protocol::native::Ready::AndroidHost { owner:owner.clone(), contacts:false };
    assert!(ready.validate_operation(&Op::ExecuteAndroidHost { operation:host::Operation::Restart { previous:owner.clone().into(), url:None } }).is_ok());
    for state in [frontcap_core::native::app_state::State::Background,frontcap_core::native::app_state::State::Foreground] {
        assert!(ready.validate_operation(&Op::ExecuteAndroidHost {operation:host::Operation::SetAppState {owner:owner.clone().into(),state}}).is_ok());
        let mut foreign=owner.clone();foreign.process_start_ticks+=1;
        assert!(ready.validate_operation(&Op::ExecuteAndroidHost {operation:host::Operation::SetAppState {owner:foreign.into(),state}}).is_err());
    }
    let mut stale=owner.clone();stale.process_start_ticks+=1;
    assert!(ready.validate_operation(&Op::ExecuteAndroidHost { operation:host::Operation::Restart { previous:stale.into(), url:None } }).is_err());
    let operation = host::Operation::Observe { input_basis:false };
    let op = Op::ExecuteAndroidHost { operation:operation.clone() };
    assert!(ready.validate_operation(&op).is_ok());
    assert!(ready.validate_operation(&Op::ExecuteAndroidHost { operation:host::Operation::Observe { input_basis:true } }).is_err());
    assert!(ready.validate_operation(&Op::ExecuteAndroidWebview { operation:worker::Operation::Read }).is_err());
    let worker_ready = host::Ready { owner:owner.clone(), contacts:None };
    config.validate_host_ready(&worker_ready).unwrap();
    let mut wrong_profile = config.clone(); wrong_profile.entry_url = "http://127.0.0.1/page".into();
    assert!(wrong_profile.validate_host_ready(&worker_ready).is_err());
    save_atomic(&session.join("host-ready.json"), &worker_ready).unwrap();
    let id = RequestId::new("native-host-capture").unwrap();
    let mut journal = Journal::open(&runtime::journal_path(root)).unwrap();
    journal.begin(&id,&args_sha256(&serde_json::to_value((&op,5000_u64)).unwrap())).unwrap();
    record(root,&id,&op,5000).unwrap(); journal.dispatched(&id).unwrap();
    let history = std::fs::read(runtime::journal_path(root)).unwrap();
    let directory = session.join("requests").join(id.as_str()); std::fs::create_dir(&directory).unwrap();
    save_atomic(&directory.join("operation.json"), &worker::Operation::Host { request:operation.clone() }).unwrap();
    assert_eq!(saved_result(root,&f.launch,&id).unwrap(),RequestResult::Pending { dispatched:true });
    let completion = host::Completion::Observed { owner:Box::new(owner.clone()), screen:directory.join("capture/screen.png"), bytes:128, sha256:"c".repeat(64), basis:None };
    let result = directory.join("result.json");
    save_atomic(&result,&worker::Reply::Completed { completion:Box::new(W::Host { completion:completion.clone() }) }).unwrap();
    let original = std::fs::read(&result).unwrap();
    let expected = RequestResult::Completed { answer:Box::new(Answer::Ok(Reply::Native { body:NativeReply::Done {
        request_id:id.clone(), outcome:Box::new(Outcome::AndroidHostCompleted { completion:completion.clone() }) } })) };
    assert_eq!(saved_result(root,&f.launch,&id).unwrap(),expected);
    assert_eq!(saved_result(root,&f.launch,&id).unwrap(),expected);
    assert_eq!(std::fs::read(&result).unwrap(),original);
    assert_eq!(std::fs::read(runtime::journal_path(root)).unwrap(),history);
    let mut foreign = completion;
    let host::Completion::Observed { owner: foreign_owner, .. } = &mut foreign else { panic!() };
    foreign_owner.process_start_ticks+=1;
    save_atomic(&result,&worker::Reply::Completed { completion:Box::new(W::Host { completion:foreign }) }).unwrap();
    assert!(saved_result(root,&f.launch,&id).is_err());
    assert!(worker::read_result(&session,&id,&worker::Operation::Read).is_err());
    std::fs::write(&result,&original).unwrap();
    let mut changed=config.clone();changed.device.host_app.as_mut().unwrap().local_service=Some(frontcap_native::android_webview::LocalService {device_port:1234,host_port:1235});
    save_atomic(&session.join("launch.json"),&serde_json::json!({"config":changed,"deadline_ns":1})).unwrap();
    assert!(saved_result(root,&f.launch,&id).is_err(),"retained readiness cannot hide a changed frozen launch");
    save_atomic(&session.join("launch.json"),&serde_json::json!({"config":config,"deadline_ns":1})).unwrap();
    let id = RequestId::new("host-url").unwrap();
    let url = "example://item/unknown";
    let operation = host::Operation::DispatchUrl { owner:Box::new(owner.clone()), url:url.into() };
    let op = Op::ExecuteAndroidHost { operation:operation.clone() };
    assert!(ready.validate_operation(&op).is_ok());
    let mut stale = owner.clone(); stale.process_start_ticks += 1;
    assert!(ready.validate_operation(&Op::ExecuteAndroidHost { operation:host::Operation::DispatchUrl {
        owner:Box::new(stale),url:url.into() } }).is_err());
    journal.begin(&id,&args_sha256(&serde_json::to_value((&op,5000_u64)).unwrap())).unwrap();
    record(root,&id,&op,5000).unwrap(); journal.dispatched(&id).unwrap();
    let request = session.join("requests").join(id.as_str());std::fs::create_dir(&request).unwrap();
    save_atomic(&request.join("operation.json"),&worker::Operation::Host {request:operation}).unwrap();
    let proof = session.join("target/host-urls/host-url");std::fs::create_dir_all(&proof).unwrap();
    save_atomic(&proof.join("intent.json"),&serde_json::json!({"owner":owner,"url":url})).unwrap();
    assert_eq!(saved_result(root,&f.launch,&id).unwrap(),RequestResult::Pending {dispatched:true});
    let command = session.join("target/command-1/command.json");std::fs::create_dir(command.parent().unwrap()).unwrap();
    std::fs::write(&command,b"original exchange").unwrap();
    let acceptance = host::UrlAcceptance {owner:owner.clone(),url:url.into(),
        confirmation:frontcap_core::native::external_entry::Observation::Unobserved,
        destination:frontcap_core::native::external_entry::Observation::Unobserved};
    save_atomic(&proof.join("receipt.json"),&serde_json::json!({"acceptance":acceptance,
        "commands":{"command-1/command.json":frontcap_core::canonical::sha256_bytes_hex(b"original exchange")}})).unwrap();
    let expected = RequestResult::Completed {answer:Box::new(Answer::Ok(Reply::Native {body:NativeReply::Done {
        request_id:id.clone(),outcome:Box::new(Outcome::AndroidHostCompleted {completion:host::Completion::UrlAccepted {acceptance:Box::new(acceptance)}}) }}))};
    let history = std::fs::read(runtime::journal_path(root)).unwrap();
    assert_eq!(saved_result(root,&f.launch,&id).unwrap(),expected);
    assert_eq!(saved_result(root,&f.launch,&id).unwrap(),expected);
    assert!(!request.join("result.json").exists(),"lookup cannot publish a missing worker result");
    assert_eq!(std::fs::read(runtime::journal_path(root)).unwrap(),history);
    std::fs::write(command,b"changed exchange").unwrap();
    assert!(saved_result(root,&f.launch,&id).is_err());
}

#[test]
fn ios_host_recovery_requires_original_owner_operation_and_capture_bytes() {
    use frontcap_core::native::ios_host as host;
    let f=Fixture::new();let root=f.root.path();let session=root.join("native-session");
    let config:frontcap_native::ios_host::Config=serde_json::from_value(serde_json::json!({
        "simulator":{"xcrun":"/unused","runtime":"ios","device_type":"phone","source_sha256":"a".repeat(64)},
        "app":{"app":"/unused/app","bundle":"dev.example.game","environment":{},"mute_audio":true,"library":{"path":"/unused/lib","sha256":"b".repeat(64)}},"contacts":false
    })).unwrap();
    let mut spec:LaunchSpec=required(&super::super::spec_path(root)).unwrap();spec.target=crate::native::target::Launch::IosHost(config.clone());save_atomic(&super::super::spec_path(root),&spec).unwrap();
    let decoded:LaunchSpec=required(&super::super::spec_path(root)).unwrap();assert!(matches!(decoded.target,crate::native::target::Launch::IosHost(_)));
    let worker_config=frontcap_native::webview_session::Config {lifetime_ms: 900_000, simulator:config.simulator.clone(),app:config.app.clone(),contacts:false,system_ui:None,lifecycle:None,service_url_environment:None};
    save_atomic(&session.join("launch.json"),&serde_json::json!({"config":worker_config,"host":true,"deadline_ns":1})).unwrap();
    let owner=host::Owner {simulator:"abcd0000-0000-0000-0000-000000000001".into(),process:ProcessIdentity {pid:42,started:"original".into()},attachment_sha256:frontcap_core::canonical::sha256_hex(&frontcap_core::canonical::canonical_of(&(&config.app.library.sha256,Option::<&str>::None,true)))};
    let ready=frontcap_recorder_protocol::native::Ready::IosHost {owner:owner.clone(),contacts:false,pages:false,system_ui:None,cold_entry:false,entry:None,app_lifecycle:false,visibility:None};
    let operation=host::Operation::Observe {input_basis:false};let op=Op::ExecuteIosHost {operation:operation.clone()};
    assert!(ready.validate_operation(&op).is_ok());assert!(ready.validate_operation(&Op::ExecuteWebview {operation:frontcap_core::native::webview::Operation::Capture}).is_err());
    assert!(ready.validate_operation(&Op::ExecuteIosHost {operation:host::Operation::Observe {input_basis:true}}).is_err());
    for p in ["target/app","target/simulator","target/host/capture-catalog","host-requests/catalog"] {std::fs::create_dir_all(session.join(p)).unwrap();}
    save_atomic(&session.join("target/app/process.json"),&owner.process).unwrap();save_atomic(&session.join("target/simulator/simulator.json"),&owner.simulator).unwrap();
    save_atomic(&session.join("host-ready.json"),&host::Ready {owner:owner.clone(),contacts:None,pages:false,system_ui:None,app_lifecycle:false}).unwrap();
    let id=RequestId::new("catalog").unwrap();let mut journal=Journal::open(&runtime::journal_path(root)).unwrap();journal.begin(&id,&args_sha256(&serde_json::to_value((&op,5000_u64)).unwrap())).unwrap();record(root,&id,&op,5000).unwrap();journal.dispatched(&id).unwrap();
    let lookup=||saved_result(root,&f.launch,&id);assert_eq!(lookup().unwrap(),RequestResult::Pending {dispatched:true});
    let binding=serde_json::json!({"owner":owner,"operation":operation});let request=session.join("host-requests/catalog/request.json");save_atomic(&request,&binding).unwrap();
    let image=session.join("target/host/capture-catalog/screen.png");let bytes=b"\x89PNG\r\n\x1a\noriginal";std::fs::write(&image,bytes).unwrap();let image=image.canonicalize().unwrap();
    let completion=host::Completion::Observed {capture:host::Capture {owner:owner.clone(),path:image.clone(),bytes:bytes.len() as u64,sha256:frontcap_core::canonical::sha256_bytes_hex(bytes),input:None}};
    save_atomic(&session.join("host-requests/catalog/result.json"),&serde_json::json!({"result":"completed","completion":completion})).unwrap();
    let history=std::fs::read(runtime::journal_path(root)).unwrap();let expected=RequestResult::Completed {answer:Box::new(Answer::Ok(Reply::Native {body:NativeReply::Done {request_id:id.clone(),outcome:Box::new(Outcome::IosHostCompleted {completion})}}))};
    assert_eq!(lookup().unwrap(),expected);assert_eq!(lookup().unwrap(),expected);assert_eq!(std::fs::read(runtime::journal_path(root)).unwrap(),history);
    let mut wrong=binding.clone();wrong["operation"]["input_basis"]=true.into();save_atomic(&request,&wrong).unwrap();assert!(lookup().is_err());save_atomic(&request,&binding).unwrap();
    save_atomic(&session.join("target/app/process.json"),&ProcessIdentity {pid:43,started:"replacement".into()}).unwrap();assert!(lookup().is_err());save_atomic(&session.join("target/app/process.json"),&owner.process).unwrap();
    std::fs::write(image,b"changed").unwrap();assert!(lookup().is_err());
}

#[test]
fn ios_host_profiles_admit_only_declared_page_registry() {
    let config:frontcap_native::ios_host::Config=serde_json::from_value(serde_json::json!({
        "simulator":{"xcrun":"/usr/bin/xcrun","runtime":"runtime","device_type":"phone","source_sha256":"a".repeat(64)},
        "app":{"app":"/tmp/consumer.app","bundle":"dev.consumer","environment":{},"library":{"path":"/tmp/input.dylib","sha256":"b".repeat(64)},"mute_audio":true},"contacts":true
    })).unwrap();
    for script in [false,true] { for registry in [false,true] {
        let mut candidate=config.clone();candidate.app.page_registry=registry;
        candidate.app.script=script.then(||frontcap_native::simulator_app::AttachmentFile {path:"/tmp/timeline.js".into(),sha256:"c".repeat(64)});
        let encoded=serde_json::to_value(crate::native::target::Launch::IosHost(candidate.clone())).unwrap();
        let decoded=serde_json::from_value::<crate::native::target::Launch>(encoded);
        assert_eq!(decoded.is_ok(),script==registry);
        if let Ok(crate::native::target::Launch::IosHost(accepted))=decoded { assert_eq!(serde_json::to_value(accepted).unwrap(),serde_json::to_value(candidate).unwrap()); }
    }}
}

#[test]
fn android_resource_retirement_resolves_only_stop_without_fabricating_worker_or_app_results() {
    use frontcap_native::{android_webview_session as worker, resource_group};
    for webview in [false, true] {
        let f = Fixture::new(); let root = f.root.path();
        let session = root.join("native-session").canonicalize().unwrap();
        std::fs::remove_file(session.join("view-ready.json")).unwrap();
        let mut spec: LaunchSpec = required(&super::super::spec_path(root)).unwrap();
        let crate::native::target::Launch::Android(mut device) = spec.target else { panic!() };
        let profile = frontcap_native::android_webview::Profile { package: "dev.example.consumer".into(),
            activity: "dev.example.consumer.MainActivity".into(), local_service: None };
        device.view_app = None;
        if webview { device.webview_app = Some(profile); } else { device.host_app = Some(profile); }
        let config = worker::Config { device, entry_url: if webview { "http://127.0.0.1/game".into() } else { String::new() },
            lifetime_ms: 30000, input: None };
        spec.target = if webview { crate::native::target::Launch::AndroidWebview(config.clone()) }
            else { crate::native::target::Launch::AndroidHost(config.clone()) };
        save_atomic(&super::super::spec_path(root), &spec).unwrap();
        save_atomic(&session.join("launch.json"), &serde_json::json!({"config":config,"deadline_ns":1})).unwrap();
        std::fs::File::create_new(session.join("session.lock")).unwrap();
        let mut child = std::process::Command::new("/bin/sleep").arg("300").spawn().unwrap();
        let identity = ProcessIdentity::of(child.id()).unwrap().unwrap();
        child.kill().unwrap(); child.wait().unwrap();
        save_atomic(&session.join("worker.json"), &identity).unwrap();
        let group = session.join("target-resources"); std::fs::create_dir(&group).unwrap();
        let target = session.join("target"); std::fs::create_dir(&target).unwrap();
        let group_config = resource_group::Config { host: root.join("host"), target: target.clone(), evidence_root: Some(session.clone()), artifact_budget_bytes: None, temporary_storage_root: None, scratch_volume: None, scratch_volume_root: None,
            resource_executable: "/unused".into(), resource_arguments: vec![],
            ports: vec![config.device.port, config.device.port+1], deadline_ns: 1 };
        save_atomic(&group.join("config.json"), &group_config).unwrap();
        save_atomic(&group.join("ready.json"), &resource_group::Ready { target: target.clone(), evidence_root: session.clone(), temporary_disks: target.join("emulator-temporary") }).unwrap();
        let lock = std::fs::File::create_new(group.join("group.lock")).unwrap(); lock.lock().unwrap();
        let stop = RequestId::new("stop").unwrap(); let app = RequestId::new("unpublished-observation").unwrap();
        let mut journal = Journal::open(&runtime::journal_path(root)).unwrap();
        for (id, op) in [(&stop, Op::Stop), (&app, Op::Observe)] {
            journal.begin(id, &args_sha256(&serde_json::to_value((&op,5000_u64)).unwrap())).unwrap();
            record(root,id,&op,5000).unwrap(); journal.dispatched(id).unwrap();
        }
        let history = std::fs::read(runtime::journal_path(root)).unwrap();
        assert_eq!(saved_result(root,&f.launch,&stop).unwrap(), RequestResult::Pending { dispatched:true });
        lock.unlock().unwrap();
        assert_eq!(saved_result(root,&f.launch,&stop).unwrap(), RequestResult::Pending { dispatched:true });
        // Reader fixtures exercise recorder interpretation; actual group cleanup is tested in the adapter.
        let mut proof = resource_group::Retirement { adb_server: None, simulator: None, scratch_volume: None, operation_error: Some("worker result absent".into()),
            passed:false, errors:vec!["cleanup unresolved".into()], resources:Default::default(), helper_evidence:Default::default(), forwarding:Default::default() };
        save_atomic(&group.join("retirement.json"), &proof).unwrap();
        assert_eq!(saved_result(root,&f.launch,&stop).unwrap(), RequestResult::Pending { dispatched:true });
        proof.passed=true; proof.errors.clear();
        save_atomic(&group.join("cleanup.json"), &proof).unwrap(); save_atomic(&group.join("retirement.json"), &proof).unwrap();
        for _ in 0..2 {
            let result=saved_result(root,&f.launch,&stop).unwrap();
            let RequestResult::Completed { answer } = result else { panic!("stop remained unresolved") };
            let Answer::Ok(Reply::Native { body: NativeReply::Done { outcome, .. } }) = *answer else { panic!("stop did not succeed") };
            assert!(matches!(*outcome, Outcome::Stopped { .. }));
            assert_eq!(saved_result(root,&f.launch,&app).unwrap(),RequestResult::Pending { dispatched:true });
        }
        assert!(!session.join("result.json").exists());
        assert_eq!(std::fs::read(runtime::journal_path(root)).unwrap(),history);
    }
}

#[test]
fn ios_resource_retirement_resolves_only_stop_without_fabricating_worker_or_app_results() {
    use frontcap_native::{webview_session as worker, resource_group};
    for route in ["host", "webview", "uikit"] {
        let webview = route == "webview";
        let f = Fixture::new(); let root = f.root.path();
        let session = root.join("native-session").canonicalize().unwrap();
        std::fs::remove_file(session.join("view-ready.json")).unwrap();
        let mut spec: LaunchSpec = required(&super::super::spec_path(root)).unwrap();
        let config: worker::Config = serde_json::from_value(serde_json::json!({
            "simulator":{"xcrun":"/unused", "runtime":"ios", "device_type":"phone", "source_sha256":"a".repeat(64)},
            "app":{"app":"/unused/app", "bundle":"dev.example.game", "environment":{}, "mute_audio":true,
                "library":{"path":"/unused/library", "sha256":"b".repeat(64)}}
        })).unwrap();
        spec.target = if webview { crate::native::target::Launch::Webview(Box::new(config.clone())) }
            else { crate::native::target::Launch::IosHost(frontcap_native::ios_host::Config {
                lifetime_ms:config.lifetime_ms, simulator:config.simulator.clone(), app:config.app.clone(), contacts:false, system_ui:None }) };
        save_atomic(&super::super::spec_path(root), &spec).unwrap();
        save_atomic(&session.join("launch.json"), &serde_json::json!({"host":!webview,"config":config,"deadline_ns":1})).unwrap();
        std::fs::File::create_new(session.join("session.lock")).unwrap();
        let mut child = std::process::Command::new("/bin/sleep").arg("300").spawn().unwrap();
        let identity = ProcessIdentity::of(child.id()).unwrap().unwrap();
        child.kill().unwrap(); child.wait().unwrap();
        save_atomic(&session.join("worker.json"), &identity).unwrap();
        let group = session.join("target-resources"); std::fs::create_dir(&group).unwrap();
        let target = session.join("target"); std::fs::create_dir(&target).unwrap();
        if route == "uikit" {
            let mut config: frontcap_native::ios_target::IosLaunch = serde_json::from_str(
                include_str!("../../tests/fixtures/native/ios-launch.json")).unwrap();
            config.resource_lifetime_ms = Some(30_000);
            let owner = serde_json::json!({
                "runner": {"simulator":"abcd0000-0000-0000-0000-000000000001", "generation":"abcd0000-0000-0000-0000-000000000002", "pid":41},
                "app": {"simulator":"abcd0000-0000-0000-0000-000000000001", "generation":"abcd0000-0000-0000-0000-000000000003", "pid":42},
                "controller_id":config.controller_id, "source_sha256":config.xctest.simulator.source_sha256 });
            save_atomic(&session.join("ready.json"), &owner).unwrap();
            save_atomic(&target.join("request-owner.json"), &owner).unwrap();
            save_atomic(&target.join("launch.json"), &config.xctest).unwrap();
            save_atomic(&session.join("launch.json"), &serde_json::json!({"config":config,"deadline_ns":1})).unwrap();
            spec.target = crate::native::target::Launch::Ios(config);
            save_atomic(&super::super::spec_path(root), &spec).unwrap();
        }
        let group_config = resource_group::Config { host: root.join("host"), target: target.clone(), evidence_root: Some(session.clone()), artifact_budget_bytes: None, temporary_storage_root: None, scratch_volume: None, scratch_volume_root: None,
            resource_executable: "/unused".into(), resource_arguments: vec![],
            ports: vec![], deadline_ns: 1 };
        save_atomic(&group.join("config.json"), &group_config).unwrap();
        save_atomic(&group.join("ready.json"), &resource_group::Ready { target: target.clone(), evidence_root: session.clone(), temporary_disks: target.join("emulator-temporary") }).unwrap();
        let lock = std::fs::File::create_new(group.join("group.lock")).unwrap(); lock.lock().unwrap();
        let stop = RequestId::new("stop").unwrap(); let app = RequestId::new("unpublished-observation").unwrap();
        let mut journal = Journal::open(&runtime::journal_path(root)).unwrap();
        for (id, op) in [(&stop, Op::Stop), (&app, Op::Observe)] {
            journal.begin(id, &args_sha256(&serde_json::to_value((&op,5000_u64)).unwrap())).unwrap();
            record(root,id,&op,5000).unwrap(); journal.dispatched(id).unwrap();
        }
        let history = std::fs::read(runtime::journal_path(root)).unwrap();
        assert_eq!(saved_result(root,&f.launch,&stop).unwrap(), RequestResult::Pending { dispatched:true });
        lock.unlock().unwrap();
        assert_eq!(saved_result(root,&f.launch,&stop).unwrap(), RequestResult::Pending { dispatched:true });
        // Reader fixtures exercise recorder interpretation; actual group cleanup is tested in the adapter.
        let mut proof = resource_group::Retirement { adb_server: None, simulator: None, scratch_volume: None, operation_error: Some("worker result absent".into()),
            passed:false, errors:vec!["cleanup unresolved".into()], resources:Default::default(), helper_evidence:Default::default(), forwarding:Default::default() };
        save_atomic(&group.join("retirement.json"), &proof).unwrap();
        assert_eq!(saved_result(root,&f.launch,&stop).unwrap(), RequestResult::Pending { dispatched:true });
        proof.passed=true; proof.errors.clear();
        save_atomic(&group.join("cleanup.json"), &proof).unwrap(); save_atomic(&group.join("retirement.json"), &proof).unwrap();
        for _ in 0..2 {
            let result=saved_result(root,&f.launch,&stop).unwrap();
            let RequestResult::Completed { answer } = result else { panic!("stop remained unresolved") };
            let Answer::Ok(Reply::Native { body: NativeReply::Done { outcome, .. } }) = *answer else { panic!("stop did not succeed") };
            assert!(matches!(*outcome, Outcome::Stopped { .. }));
            assert_eq!(saved_result(root,&f.launch,&app).unwrap(),RequestResult::Pending { dispatched:true });
        }
        assert!(!session.join("result.json").exists());
        assert_eq!(std::fs::read(runtime::journal_path(root)).unwrap(),history);
    }
}


#[test]
fn external_session_recovers_original_receipt_and_refuses_lost_location() {
    let external = tempfile::tempdir().unwrap();
    let mut f = Fixture::with_session_root(Some(external.path()));
    let (completion, receipt) = f.complete();
    assert!(receipt.starts_with(external.path()));
    let journal = std::fs::read(runtime::journal_path(f.root.path())).unwrap();
    let expected = RequestResult::Completed { answer: Box::new(Answer::Ok(Reply::Native {
        body: NativeReply::Done { request_id: f.id.clone(), outcome: Box::new(Outcome::Completed { completion }) },
    })) };
    assert_eq!(f.result().unwrap(), expected);
    std::fs::remove_file(f.root.path().join("native-session-location.json")).unwrap();
    assert!(f.result().unwrap_err().to_string().contains("location missing"));
    assert_eq!(std::fs::read(runtime::journal_path(f.root.path())).unwrap(), journal);
    assert!(receipt.exists());
}
