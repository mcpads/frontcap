use super::*;
use frontcap_core::native::request::Operation;
use frontcap_recorder_protocol::client::{ClientError, RecorderClient};
use std::os::unix::fs::PermissionsExt;

fn id(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}
struct Fixture {
    root: tempfile::TempDir,
    task: tokio::task::JoinHandle<Result<(), RecorderError>>,
    client: RecorderClient,
    lease: LeaseToken,
}
impl Fixture {
    async fn start() -> Self {
        Self::platform(false).await
    }
    async fn platform(ios: bool) -> Self {
        Self::platform_service(ios, None).await
    }
    async fn platform_service(ios: bool, service: Option<ServiceMode>) -> Self {
        Self::configured(ios, false, service).await
    }
    async fn configured(ios: bool, webview: bool, service: Option<ServiceMode>) -> Self {
        Self::configured_host(ios, webview, false, service).await
    }
    async fn configured_host(ios: bool, webview: bool, host: bool, service: Option<ServiceMode>) -> Self {
        Self::configured_storage(ios, webview, host, service, None).await
    }
    async fn configured_storage(ios: bool, webview: bool, host: bool, service: Option<ServiceMode>, session_root: Option<PathBuf>) -> Self {
        let budget = service.as_ref().map(|_| frontcap_core::write_budget::Limits { ordinary: 64 << 20, retirement: 16 << 20 });
        Self::configured_budget(ios, webview, host, service, session_root, budget).await
    }
    async fn configured_budget(ios: bool, webview: bool, host: bool, service: Option<ServiceMode>, session_root: Option<PathBuf>, write_budget: Option<frontcap_core::write_budget::Limits>) -> Self {
        let root = tempfile::tempdir().unwrap();
        let exe = root.path().join("session.py");
        std::fs::write(
            &exe,
            if host { include_str!("../../tests/fixtures/native/ios-host-session.py") }
            else if webview { include_str!("../../tests/fixtures/native/webview-session.py") }
            else if ios {
                include_str!("../../tests/fixtures/native/ios-session.py")
            } else {
                include_str!("../../tests/fixtures/native/session.py")
            },
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut spec = LaunchSpec {
            request_id: id("launch"),
            launch_id: LaunchId::generate(),
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
                    python: "python3".into(),
                    bridge_script: "unused".into(),
                    input_registration: Default::default(),
                    clock_transport: Default::default(),
                    source_sha256: "a".repeat(64),
                },
            ),
            session_executable: exe,
            acquisition_budget_ms: 5000,
            min_free_bytes: None,
            write_budget,
            session_root,
            session_volume: None,            service,
        };
        if ios {
            spec.target = target::Launch::Ios(
                serde_json::from_str(include_str!("../../tests/fixtures/native/ios-launch.json"))
                    .unwrap(),
            );
        }
        if webview {
            spec.target = target::Launch::Webview(serde_json::from_str(include_str!("../../tests/fixtures/native/webview-launch.json")).unwrap());
        }
        if host {
            let mut config: frontcap_native::webview_session::Config = serde_json::from_str(include_str!("../../tests/fixtures/native/webview-launch.json")).unwrap();
            config.app.script=None;
            spec.target=target::Launch::IosHost(frontcap_native::ios_host::Config {lifetime_ms:config.lifetime_ms,simulator:config.simulator,app:config.app,contacts:false,system_ui:None});
        }
        if write_budget.is_some() {
            launcher::publish_spec(root.path(), &spec).unwrap();
        } else {
            save_atomic(&spec_path(root.path()), &spec).unwrap();
        }
        let dir = root.path().to_path_buf();
        let task = tokio::spawn(async move { crate::run_launch(&dir).await });
        crate::launcher::wait_ready(root.path(), Duration::from_secs(8))
            .await
            .unwrap();
        let (mut client, hello) = RecorderClient::connect(&runtime::socket_path(root.path()))
            .await
            .unwrap();
        assert_eq!(
            hello,
            Reply::Hello {
                launch_id: spec.launch_id
            }
        );
        let Reply::Native {
            body: NativeReply::Attached { lease, status, .. },
        } = client
            .call(Call::Reattach {
                request_id: id("attach"),
            })
            .await
            .unwrap()
        else {
            panic!("missing native attach")
        };
        assert!(
            matches!(
                status.ready,
                frontcap_recorder_protocol::native::Ready::AndroidViews { .. }
            ) == (!ios && !webview && !host)
        );
        Self {
            root,
            task,
            client,
            lease,
        }
    }
    fn call(&self, name: &str, op: Op) -> Call {
        Call::Native {
            lease: self.lease.clone(),
            request_id: id(name),
            op,
            budget_ms: 5000,
        }
    }
    async fn stop(mut self) {
        let call = self.call("stop", Op::Stop);
        assert!(matches!(
            self.client.call(call).await.unwrap(),
            Reply::Native {
                body: NativeReply::Done { .. }
            }
        ));
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let status: Status = load(&self.root.path().join("native-status.json"))
            .unwrap()
            .unwrap();
        assert!(status.stopped && !status.lease_held && status.running.is_none());
        assert!(matches!(
            Journal::read_state(&runtime::journal_path(self.root.path()), &id("stop")).unwrap(),
            Some(RequestState::Completed(_))
        ));
    }
}
async fn until_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
fn assert_code(result: Result<Reply, ClientError>, expected: ErrorCode) {
    assert!(
        matches!(result, Err(ClientError::Recorder(ref e)) if e.code == expected),
        "{result:?}"
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lost_socket_reply_keeps_request_owned_and_recovers_without_another_dispatch() {
    let mut f = Fixture::start().await;
    std::fs::write(f.root.path().join("hold"), b"").unwrap();
    let call = f.call(
        "advance",
        Op::Execute {
            operation: Operation::Advance { by_us: 1000 },
        },
    );
    f.client.send_without_waiting(call).await.unwrap();
    until_file(&f.root.path().join("native-session/dispatched")).await;
    let (mut observer, _) = RecorderClient::connect(&runtime::socket_path(f.root.path()))
        .await
        .unwrap();
    assert_code(
        observer.call(f.call("busy", Op::Observe)).await,
        ErrorCode::Busy,
    );
    let (replacement, _) = RecorderClient::connect(&runtime::socket_path(f.root.path()))
        .await
        .unwrap();
    drop(std::mem::replace(&mut f.client, replacement));
    let attached = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match f
                .client
                .call(Call::Reattach {
                    request_id: id("reattach"),
                })
                .await
            {
                Ok(reply) => break reply,
                Err(ClientError::Recorder(e)) if e.code == ErrorCode::LeaseHeld => {
                    tokio::time::sleep(Duration::from_millis(5)).await
                }
                other => panic!("{other:?}"),
            }
        }
    })
    .await
    .unwrap();
    let Reply::Native {
        body: NativeReply::Attached { lease, .. },
    } = attached
    else {
        panic!("attach")
    };
    f.lease = lease;
    let pending = f
        .client
        .call(Call::Result {
            request_id: id("advance"),
        })
        .await
        .unwrap();
    assert!(matches!(
        pending,
        Reply::Result {
            result: RequestResult::Pending { dispatched: true },
            ..
        }
    ));
    std::fs::remove_file(f.root.path().join("hold")).unwrap();
    let saved = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let r = observer
                .call(Call::Result {
                    request_id: id("advance"),
                })
                .await
                .unwrap();
            if let Reply::Result {
                result: RequestResult::Completed { answer },
                ..
            } = r
            {
                break *answer;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let retry = f.call(
        "advance",
        Op::Execute {
            operation: Operation::Advance { by_us: 1000 },
        },
    );
    assert_eq!(Answer::Ok(f.client.call(retry).await.unwrap()), saved);
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("native-session/calls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let changed = f.call(
        "advance",
        Op::Execute {
            operation: Operation::Advance { by_us: 2000 },
        },
    );
    assert_code(f.client.call(changed).await, ErrorCode::RequestIdConflict);
    assert_code(
        f.client
            .call(Call::Observe {
                op: frontcap_recorder_protocol::ObserveOp::Screenshot {},
                context: None,
            })
            .await,
        ErrorCode::UnsupportedCapability,
    );
    f.stop().await;
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uncertain_session_failure_remains_pending_and_fences_new_effects() {
    let mut f = Fixture::start().await;
    let invalid = f.call(
        "overflow",
        Op::Execute {
            operation: Operation::Advance { by_us: u64::MAX },
        },
    );
    assert_code(f.client.call(invalid).await, ErrorCode::InvalidArguments);
    assert!(!f.root.path().join("native-session/calls.jsonl").exists());
    std::fs::write(f.root.path().join("fail"), b"").unwrap();
    let failed = f.call(
        "lost",
        Op::Execute {
            operation: Operation::ReadClock {},
        },
    );
    assert_code(
        f.client.call(failed.clone()).await,
        ErrorCode::RecorderFault,
    );
    let again = f.client.call(failed).await.unwrap();
    assert!(matches!(
        again,
        Reply::Result {
            result: RequestResult::Pending { dispatched: true },
            ..
        }
    ));
    let fresh = f.call("fresh", Op::Observe);
    assert_code(f.client.call(fresh).await, ErrorCode::RecorderFault);
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("native-session/calls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    assert!(matches!(
        Journal::read_state(&runtime::journal_path(f.root.path()), &id("lost")).unwrap(),
        Some(RequestState::Dispatched)
    ));
    f.stop().await;
}
#[test]
fn damaged_native_launch_registry_never_allocates_a_replacement() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = crate::launcher::Workspace {
        root: tmp.path().to_path_buf(),
    };
    std::fs::create_dir_all(ws.launches()).unwrap();
    let entry = ws.launches().join("launch.json");
    std::fs::write(&entry, b"partial").unwrap();
    assert_eq!(
        crate::launcher::register_native(&ws, &id("launch"), "hash")
            .unwrap_err()
            .0
            .code,
        ErrorCode::RecorderFault
    );
    assert_eq!(std::fs::read(&entry).unwrap(), b"partial");
}

#[tokio::test]
async fn ios_recorder_rejects_cross_platform_work_before_admission_and_retains_cached_results() {
    use frontcap_core::native::ios_request::{
        Completion as IosCompletion, Operation as IosOperation,
    };
    let mut f = Fixture::platform(true).await;
    for (name, op) in [
        (
            "wrong-platform",
            Op::Execute {
                operation: Operation::ReadClock {},
            },
        ),
        (
            "finish",
            Op::ExecuteIos {
                operation: IosOperation::Finish,
            },
        ),
    ] {
        let call = f.call(name, op);
        assert!(f.client.call(call).await.is_err());
        assert!(
            Journal::read_state(&runtime::journal_path(f.root.path()), &id(name))
                .unwrap()
                .is_none()
        );
    }
    let call = f.call(
        "advance",
        Op::ExecuteIos {
            operation: IosOperation::AdvanceClock { delta_ns: 5000000 },
        },
    );
    let first = f.client.call(call.clone()).await.unwrap();
    assert!(
        matches!(&first,Reply::Native{body:NativeReply::Done{outcome,..}} if matches!(outcome.as_ref(),Outcome::IosCompleted{completion:IosCompletion::Clock(c)} if c.now_ns==5000000))
    );
    assert_eq!(f.client.call(call).await.unwrap(), first);
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("native-session/calls.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let call = f.call("observe", Op::Observe);
    assert!(
        matches!(f.client.call(call).await.unwrap(),Reply::Native{body:NativeReply::Done{outcome,..}} if matches!(outcome.as_ref(),Outcome::IosCompleted{completion:IosCompletion::Ui(_)}))
    );
    f.stop().await;
}

#[tokio::test]
async fn stopped_recorder_refuses_reattach_without_consuming_detached_results() {
    let mut fixture = Fixture::start().await;
    let Reply::Native {
        body: NativeReply::Status { mut status },
    } = fixture.client.call(Call::Status).await.unwrap()
    else {
        panic!("missing status")
    };
    fixture.stop().await;
    status.stopped = true;
    let root = tempfile::tempdir().unwrap();
    let journal = Journal::open(&runtime::journal_path(root.path())).unwrap();
    let rec = Arc::new(Recorder {
        dir: root.path().into(),
        storage_roots: storage::Roots::runtime(root.path()).unwrap(),
        min_free_bytes: storage::DEFAULT_MIN_FREE_BYTES,
        write_budget: None,
        state: Mutex::new(State {
            journal,
            status: *status,
            lease: None,
            detached: vec![id("completed")],
            retiring: false,
        }),
        session: Mutex::new(None),
        session_volume: Mutex::new(None),
        service: None,
        shutdown: tokio::sync::Notify::new(),
    });
    for lease in [None, Some((LeaseToken::generate(), 1))] {
        rec.state.lock().unwrap().lease = lease.clone();
        let answer = rec
            .handle(
                2,
                Call::Reattach {
                    request_id: id("after-stop"),
                },
            )
            .await;
        assert!(
            matches!(
                answer,
                Answer::Err(ErrorReply {
                    code: ErrorCode::Stopped,
                    ..
                })
            ),
            "{answer:?}"
        );
        let state = rec.state.lock().unwrap();
        assert_eq!(state.lease, lease);
        assert_eq!(state.detached, vec![id("completed")]);
        assert!(state.journal.state(&id("after-stop")).is_none());
    }
}


#[tokio::test]
async fn owned_service_follows_recorder_launch_and_terminal_runtime() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = frontcap_core::native::service::RequestEndpoint {
        port: reserved.local_addr().unwrap().port(), request_path: "/shared/".into(),
    };
    drop(reserved);
    for mode in [ServiceMode::ControlledHttp, ServiceMode::ControlledHttpAt(endpoint)] {
    let mut fixture = Fixture::platform_service(false, Some(mode)).await;
    let dir = fixture.root.path();
    let spec: LaunchSpec = load(&spec_path(dir)).unwrap().unwrap();
    let binding: frontcap_service_fixture::owned::Binding = load(&dir.join("service/owner.json")).unwrap().unwrap();
    let initial: frontcap_service_fixture::Record = load(&dir.join("service/state.json")).unwrap().unwrap();
    assert_eq!(binding.owner, spec.launch_id);
    assert_eq!(binding.service, initial.generation);
    let mut stream = tokio::net::TcpStream::connect(initial.address).await.unwrap();
    stream.write_all(format!("GET {}load HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", initial.requests_path()).as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let record: frontcap_service_fixture::Record = load(&dir.join("service/state.json")).unwrap().unwrap();
            if record.requests.contains_key(&id("load")) { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let limits = spec.write_budget.unwrap();
    let budget = frontcap_core::write_budget::WriteBudget::open(
        &dir.join("native-write-budget"), spec.launch_id.clone(), limits,
    ).unwrap();
    let before = budget.charged().unwrap();
    assert!(before.ordinary > 0);
    budget.charge(frontcap_core::write_budget::Class::Ordinary, limits.ordinary - before.ordinary).unwrap();
    let stop = fixture.call("stop", Op::Stop);
    fixture.client.call(stop).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut fixture.task).await.unwrap().unwrap().unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut bytes)).await.unwrap().unwrap();
    assert!(String::from_utf8(bytes).unwrap().starts_with("HTTP/1.1 503"));
    assert!(tokio::net::TcpStream::connect(initial.address).await.is_err());
    let final_record: frontcap_service_fixture::Record = load(&dir.join("service/state.json")).unwrap().unwrap();
    assert!(budget.charged().unwrap().retirement > before.retirement);
    assert!(final_record.stopping);
    assert!(final_record.releases.is_empty());
    assert_eq!(final_record.requests[&id("load")].transport, frontcap_service_fixture::Transport::Stopped);
    assert!(matches!(runtime::load_runtime(dir).unwrap().unwrap().state, RuntimeState::Stopped));
    }
}

#[tokio::test]
async fn failed_target_acquisition_does_not_leave_service_listener_running() {
    let fixture = Fixture::start().await;
    let mut spec: LaunchSpec = load(&spec_path(fixture.root.path())).unwrap().unwrap();
    fixture.stop().await;
    for resources in [false, true] {
        let root = tempfile::tempdir().unwrap();
        spec.launch_id = LaunchId::generate();
        spec.write_budget = Some(frontcap_core::write_budget::Limits { ordinary: 8 << 20, retirement: 8 << 20 });
        spec.service = Some(if resources {
            let source = root.path().join("app.js");
            std::fs::write(&source, "export const value = 1;").unwrap();
            ServiceMode::ControlledAssets { endpoint: None, assets:vec![frontcap_service_fixture::assets::AssetInput {
                path:"/app.js".into(), source, content_type:"text/javascript".into(), content_security_policy:None,
            }] }
        } else { ServiceMode::ControlledHttp });
        spec.session_executable = root.path().join("missing-session");
        save_atomic(&spec_path(root.path()), &spec).unwrap();
        let error = crate::run_launch(root.path()).await.unwrap_err();
        assert!(!error.to_string().is_empty());
        let retained: frontcap_service_fixture::Record = load(&root.path().join("service/state.json")).unwrap().unwrap();
        assert!(retained.stopping);
        assert!(retained.releases.is_empty());
        assert!(tokio::net::TcpStream::connect(retained.address).await.is_err());
        assert!(matches!(runtime::load_runtime(root.path()).unwrap().unwrap().state, RuntimeState::Failed { .. }));
        let budget = WriteBudget::open(&root.path().join("native-write-budget"), spec.launch_id.clone(), spec.write_budget.unwrap()).unwrap();
        let spent = budget.charged().unwrap();
        assert!(spent.ordinary > 0 && spent.retirement > 0);
        let original = std::fs::read(runtime::runtime_path(root.path())).unwrap();
        assert!(crate::run_launch(root.path()).await.is_err());
        assert_eq!(std::fs::read(runtime::runtime_path(root.path())).unwrap(), original);
        assert_eq!(budget.charged().unwrap(), spent);
    }
}

#[tokio::test]
async fn unexpected_service_exit_fences_new_work_but_preserves_results_and_stop() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut fixture = Fixture::platform_service(false, Some(ServiceMode::ControlledHttp)).await;
    let first = fixture.call("before-service-exit", Op::Execute { operation: Operation::Advance { by_us: 1000 } });
    let original = fixture.client.call(first.clone()).await.unwrap();
    let service: frontcap_service_fixture::Record = load(&fixture.root.path().join("service/state.json")).unwrap().unwrap();
    let body = serde_json::to_string(&frontcap_service_fixture::Control::Stop { generation: service.generation }).unwrap();
    let mut socket = tokio::net::TcpStream::connect(service.address).await.unwrap();
    socket.write_all(format!("POST /control HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), socket.read_to_end(&mut bytes)).await.unwrap().unwrap();
    assert!(String::from_utf8(bytes).unwrap().starts_with("HTTP/1.1 200"));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let Reply::Native { body: NativeReply::Status { status } } = fixture.client.call(Call::Status).await.unwrap() else { panic!() };
            if status.failure.is_some() { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert_eq!(serde_json::to_value(fixture.client.call(first).await.unwrap()).unwrap(), serde_json::to_value(original).unwrap());
    let later = fixture.call("after-service-exit", Op::Observe);
    assert_code(fixture.client.call(later).await, ErrorCode::RecorderFault);
    assert!(Journal::read_state(&runtime::journal_path(fixture.root.path()), &id("after-service-exit")).unwrap().is_none());
    let stop = fixture.call("stop", Op::Stop);
    assert!(matches!(fixture.client.call(stop).await.unwrap(), Reply::Native { body: NativeReply::Done { .. } }));
    tokio::time::timeout(Duration::from_secs(5), &mut fixture.task).await.unwrap().unwrap().unwrap();
    assert!(matches!(runtime::load_runtime(fixture.root.path()).unwrap().unwrap().state, RuntimeState::Failed { .. }));
    let status: Status = load(&fixture.root.path().join("native-status.json")).unwrap().unwrap();
    assert!(status.stopped);
    assert!(status.failure.unwrap().contains("controlled service exited"));
}

#[tokio::test]
async fn service_release_recovers_unread_result_without_another_response() {
    check_service_release_recovery(false, None).await;
}

#[tokio::test]
async fn asset_release_recovers_unread_result_without_another_response() {
    check_service_release_recovery(true, None).await;
}

#[tokio::test]
async fn fixed_origin_asset_release_preserves_recovery_and_retirement() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    check_service_release_recovery(true, Some(frontcap_core::native::service::RequestEndpoint {
        port, request_path: "/content/".into(),
    })).await;
}

async fn check_service_release_recovery(resources: bool, endpoint: Option<frontcap_core::native::service::RequestEndpoint>) {
    use frontcap_core::native::service::{Reply as ServiceReply, ResponseSpec, Transport};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let asset_root = tempfile::tempdir().unwrap();
    let source = asset_root.path().join("app.js");
    let body = if resources { "export const value = 1;" } else { "[\"한글\"]" };
    std::fs::write(&source, body).unwrap();
    let mode = if resources { ServiceMode::ControlledAssets { endpoint: endpoint.clone(), assets: vec![frontcap_service_fixture::assets::AssetInput {
        path: "/app.js".into(), source, content_type: "text/javascript".into(), content_security_policy: None,
    }] } } else { ServiceMode::ControlledHttp };
    let mut fixture = Fixture::configured(false, true, Some(mode)).await;
    let inspect = fixture.call("inspect-service", Op::InspectService);
    let initial = fixture.client.call(inspect).await.unwrap();
    let Reply::Native { body: NativeReply::Done { outcome, .. } } = initial else { panic!() };
    let Outcome::Service { reply: ServiceReply::Snapshot { record } } = *outcome else { panic!() };
    if let Some(endpoint) = endpoint {
        assert_eq!(record.address.port(), endpoint.port);
        assert_eq!(record.request_path.as_deref(), Some(endpoint.request_path.as_str()));
    }
    assert!(record.requests.is_empty());
    let mut stream = tokio::net::TcpStream::connect(record.address).await.unwrap();
    let path = if resources { "/app.js".into() } else { format!("/{}/requests/load", record.generation) };
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let admitted = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let saved: frontcap_service_fixture::Record = load(&fixture.root.path().join("service/state.json")).unwrap().unwrap();
            if let Some((id, row)) = saved.requests.into_iter().next() { break (id, row); }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let inspection = fixture.client.call(fixture.call("held-progress", Op::ExecuteWebview {
        operation: frontcap_core::native::webview::Operation::InspectProgress })).await.unwrap();
    assert!(matches!(inspection, Reply::Native { body: NativeReply::Done { outcome, .. } }
        if matches!(outcome.as_ref(), Outcome::WebviewCompleted { completion }
            if matches!(completion.as_ref(), frontcap_core::native::webview::Completion::Progress { observation }
                if matches!(observation.service, frontcap_core::native::webview_progress::Service::Observed { requests: 1, held: 1, releases: 0, .. })))));
    let release = if resources {
        Op::ReleaseAsset { generation:record.generation, request:admitted.0.clone(), sha256:admitted.1.resource.unwrap().asset.sha256 }
    } else {
        Op::ReleaseService { generation:record.generation, request:admitted.0.clone(), response:ResponseSpec {status:200, body:body.into()} }
    };
    let command = fixture.call("send-load", release.clone());
    fixture.client.send_without_waiting(command).await.unwrap();
    let (replacement, _) = RecorderClient::connect(&runtime::socket_path(fixture.root.path())).await.unwrap();
    drop(std::mem::replace(&mut fixture.client, replacement));
    fixture.lease = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match fixture.client.call(Call::Reattach { request_id: id("reattach-service") }).await {
                Ok(Reply::Native { body: NativeReply::Attached { lease, .. } }) => break lease,
                Err(ClientError::Recorder(e)) if e.code == ErrorCode::LeaseHeld => tokio::task::yield_now().await,
                other => panic!("{other:?}"),
            }
        }
    }).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut bytes)).await.unwrap().unwrap();
    assert!(bytes.ends_with(body.as_bytes()));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(Journal::read_state(&runtime::journal_path(fixture.root.path()), &id("send-load")).unwrap(), Some(RequestState::Completed(_))) { break; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let retry = fixture.call("send-load", release.clone());
    let recovered = fixture.client.call(retry).await.unwrap();
    assert!(matches!(recovered, Reply::Native { body: NativeReply::Done { outcome, .. } } if matches!(*outcome, Outcome::Service { reply: ServiceReply::Released { .. } })));
    let second_id = fixture.call("send-load-again", release);
    assert!(matches!(fixture.client.call(second_id).await.unwrap(), Reply::Native { body: NativeReply::Done { outcome, .. } } if matches!(*outcome, Outcome::Service { reply: ServiceReply::Rejected { .. } })));
    let final_record: frontcap_service_fixture::Record = load(&fixture.root.path().join("service/state.json")).unwrap().unwrap();
    assert_eq!(final_record.releases.len(), 1);
    assert_eq!(final_record.requests[&admitted.0].transport, Transport::Closed);
    let inspection = fixture.client.call(fixture.call("released-progress", Op::ExecuteWebview {
        operation: frontcap_core::native::webview::Operation::InspectProgress })).await.unwrap();
    assert!(matches!(inspection, Reply::Native { body: NativeReply::Done { outcome, .. } }
        if matches!(outcome.as_ref(), Outcome::WebviewCompleted { completion }
            if matches!(completion.as_ref(), frontcap_core::native::webview::Completion::Progress { observation }
                if matches!(observation.service, frontcap_core::native::webview_progress::Service::Observed { requests: 1, held: 0, closed: 1, releases: 1, queued_for_http: 1, .. })))));
    let comparison = Op::CompareWebviewProgress { before: id("held-progress"), after: id("released-progress"), before_capture: None, after_capture: None };
    let answer = fixture.client.call(fixture.call("compare-service", comparison)).await.unwrap();
    let Reply::Native { body: NativeReply::Done { outcome, .. } } = answer else { panic!() };
    let Outcome::WebviewCompared { comparison } = *outcome else { panic!() };
    let held = comparison.changes.iter().find(|c| c.path == "/service/held").unwrap();
    assert_eq!(held.before.value, Some(serde_json::json!(1)));
    assert_eq!(held.after.value, Some(serde_json::json!(0)));
    assert_eq!(comparison.omitted_changes, 0);
    assert!(comparison.capture_content_changed.is_none());
    // Rejected release is a completed no-effect result, not a failed native session.
    fixture.stop().await;
}

#[tokio::test]
async fn service_commands_without_configuration_reject_before_journal_admission() {
    let mut fixture = Fixture::start().await;
    let command = fixture.call("no-service", Op::InspectService);
    assert_code(fixture.client.call(command).await, ErrorCode::UnsupportedCapability);
    assert!(Journal::read_state(&runtime::journal_path(fixture.root.path()), &id("no-service")).unwrap().is_none());
    let command = fixture.call("no-resources", Op::ReleaseAsset { generation:LaunchId::new("service").unwrap(), request:id("resource"), sha256:"a".repeat(64) });
    assert_code(fixture.client.call(command).await, ErrorCode::UnsupportedCapability);
    assert!(Journal::read_state(&runtime::journal_path(fixture.root.path()), &id("no-resources")).unwrap().is_none());
    fixture.stop().await;
}

#[tokio::test]
async fn preparation_observes_owned_pending_service_without_claiming_target_ready() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fixture = Fixture::start().await;
    let mut spec: LaunchSpec = load(&spec_path(fixture.root.path())).unwrap().unwrap();
    fixture.stop().await;
    let root = tempfile::tempdir().unwrap();
    let worker = root.path().join("waiting.py");
    std::fs::write(&worker, "#!/usr/bin/env python3\nimport pathlib,time,sys\nrelease=pathlib.Path(__file__).with_suffix('.release')\ndeadline=time.monotonic()+5\nwhile not release.exists() and time.monotonic()<deadline: time.sleep(.01)\nsys.exit(1)\n").unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    spec.launch_id = LaunchId::generate();
    spec.service = Some(ServiceMode::ControlledHttp);
    spec.session_executable = worker.clone();
    spec.acquisition_budget_ms = 6000;
    save_atomic(&spec_path(root.path()), &spec).unwrap();
    let unpublished = preparation::inspect(root.path(), &spec.launch_id).unwrap();
    assert!(unpublished.runtime.is_none());
    assert!(unpublished.recorder_alive.is_none());
    let directory = root.path().to_path_buf();
    let task = tokio::spawn(async move { crate::run_launch(&directory).await });
    let snapshot = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = preparation::inspect(root.path(), &spec.launch_id).unwrap();
            if snapshot.service.is_some() {break snapshot;}
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert_eq!(snapshot.recorder_alive, Some(true));
    assert_eq!(snapshot.runtime.unwrap().state, RuntimeState::Starting);
    assert!(!runtime::socket_path(root.path()).exists());
    let service = snapshot.service.unwrap();
    let mut stream = tokio::net::TcpStream::connect(service.address).await.unwrap();
    stream.write_all(format!("GET /{}/requests/pending HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", service.generation).as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = preparation::inspect(root.path(), &spec.launch_id).unwrap();
            if let Some(row) = snapshot.service.unwrap().requests.get(&id("pending")) {
                assert!(row.release.is_none());
                assert_eq!(row.transport, frontcap_service_fixture::Transport::Open);
                break;
            }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let state_path = root.path().join("service/state.json");
    let before = std::fs::read(&state_path).unwrap();
    preparation::inspect(root.path(), &spec.launch_id).unwrap();
    assert_eq!(std::fs::read(&state_path).unwrap(), before);
    let owner_path = root.path().join("service/owner.json");
    let owner = std::fs::read(&owner_path).unwrap();
    save_atomic(&owner_path, &frontcap_service_fixture::owned::Binding {
        owner:LaunchId::new("foreign").unwrap(), service:service.generation,
    }).unwrap();
    assert!(preparation::inspect(root.path(), &spec.launch_id).is_err());
    std::fs::write(owner_path, owner).unwrap();
    std::fs::write(worker.with_extension("release"), "finish").unwrap();
    assert!(tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap().is_err());
    let terminal = preparation::inspect(root.path(), &spec.launch_id).unwrap();
    assert!(matches!(terminal.runtime.unwrap().state, RuntimeState::Failed {..}));
    assert!(terminal.service.unwrap().stopping);
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut reply)).await.unwrap().unwrap();
    assert!(reply.starts_with(b"HTTP/1.1 503"));
}

#[tokio::test]
async fn preparation_release_unblocks_acquisition_and_closes_new_admission_at_ready() {
    use preparation_control::Release;
    use frontcap_service_fixture::Reply as ServiceReply;
    let fixture = Fixture::start().await;
    let mut spec: LaunchSpec = load(&spec_path(fixture.root.path())).unwrap().unwrap();
    fixture.stop().await;
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session.py");
    std::fs::write(&session, include_str!("../../tests/fixtures/native/session.py")).unwrap();
    let worker = root.path().join("resource-worker.py");
    std::fs::write(&worker, "#!/usr/bin/env python3\nimport pathlib,sys,json,urllib.request,runpy\nroot=pathlib.Path(sys.argv[1]).parent\nstate=json.loads((root/'service/state.json').read_text())\nassert urllib.request.urlopen('http://'+state['address']+'/app.js',timeout=10).read()==b'export const value=1;'\nrunpy.run_path(str(root/'session.py'),run_name='__main__')\n").unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    let source = root.path().join("app.js");
    std::fs::write(&source, "export const value=1;").unwrap();
    spec.launch_id = LaunchId::generate();
    spec.session_executable = worker;
    spec.acquisition_budget_ms = 15000;
    spec.service = Some(ServiceMode::ControlledAssets {endpoint: None, assets:vec![frontcap_service_fixture::assets::AssetInput {
        path:"/app.js".into(), source, content_type:"text/javascript".into(), content_security_policy:None,
    }]});
    save_atomic(&spec_path(root.path()), &spec).unwrap();
    let directory = root.path().to_path_buf();
    let task = tokio::spawn(async move {crate::run_launch(&directory).await});
    let service = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(service) = preparation::inspect(root.path(), &spec.launch_id).unwrap().service
                && !service.requests.is_empty() {break service;}
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    assert_eq!(runtime::load_runtime(root.path()).unwrap().unwrap().state, RuntimeState::Starting);
    let (request, row) = service.requests.iter().next().unwrap();
    let command = Release {launch_id:spec.launch_id.clone(),generation:service.generation,
        release_id:id("prepare-script"),request:request.clone(),sha256:row.resource.as_ref().unwrap().asset.sha256.clone()};
    let first = preparation_control::release(root.path(), command.clone()).await.unwrap();
    assert!(matches!(first, ServiceReply::Released {..}));
    crate::launcher::wait_ready(root.path(), Duration::from_secs(5)).await.unwrap();
    assert_eq!(preparation_control::release(root.path(), command.clone()).await.unwrap(), first);
    let mut late = command.clone(); late.release_id = id("late-preparation");
    assert!(matches!(preparation_control::release(root.path(), late).await.unwrap(), ServiceReply::Rejected {..}));
    // Reproduce the observed publication gap: the endpoint is gone while a reader
    // still sees Starting. Existing receipts remain readable; a new attempt was not sent.
    let mut record = runtime::load_runtime(root.path()).unwrap().unwrap();
    record.state = RuntimeState::Starting;
    runtime::save_runtime(root.path(), &record).unwrap();
    let mut gap = command.clone(); gap.release_id = id("publication-gap");
    let ServiceReply::Rejected { reason } = preparation_control::release(root.path(), gap).await.unwrap() else { panic!("closed connection must reject before dispatch") };
    assert!(reason.contains("this attempt was not sent"));
    assert_eq!(preparation_control::release(root.path(), command.clone()).await.unwrap(), first);
    record.state = RuntimeState::Ready;
    runtime::save_runtime(root.path(), &record).unwrap();
    let mut conflict = command; conflict.sha256 = "different".into();
    assert!(matches!(preparation_control::release(root.path(), conflict).await.unwrap(), ServiceReply::Rejected {..}));
    let (mut client, _) = RecorderClient::connect(&runtime::socket_path(root.path())).await.unwrap();
    let Reply::Native {body:NativeReply::Attached {lease,..}} = client.call(Call::Reattach {request_id:id("attach")}).await.unwrap() else {panic!()};
    client.call(Call::Native {lease, request_id:id("stop"), op:Op::Stop, budget_ms:5000}).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5),task).await.unwrap().unwrap().unwrap();
    let saved: frontcap_service_fixture::Record = load(&root.path().join("service/state.json")).unwrap().unwrap();
    assert_eq!(saved.releases.len(),1);
    assert!(saved.stopping);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webview_requests_keep_uncertainty_recover_without_replay_and_retire() {
    use frontcap_core::native::{webview::{Operation as WebOp, Completion as WebCompletion}, request::ClockOperation};
    let mut f = Fixture::configured(false, true, Some(ServiceMode::ControlledHttp)).await;
    let status: Status = load(&f.root.path().join("native-status.json")).unwrap().unwrap();
    assert!(status.capabilities.contains(&Capability::WebviewClock));
    assert!(!status.capabilities.contains(&Capability::ObservedWebviewContacts));
    assert!(!status.capabilities.contains(&Capability::WebviewLifecycle));
    assert!(status.ready.validate_operation(&Op::ExecuteWebview { operation: WebOp::Background }).is_err());
    assert!(status.capabilities.contains(&Capability::NativeRasterCapture));
    assert!(status.capabilities.contains(&Capability::WebviewProgress));
    let inspected = f.client.call(f.call("progress", Op::ExecuteWebview { operation: WebOp::InspectProgress })).await.unwrap();
    assert!(matches!(inspected, Reply::Native { body: NativeReply::Done { outcome, .. } }
        if matches!(outcome.as_ref(), Outcome::WebviewCompleted { completion }
            if matches!(completion.as_ref(), WebCompletion::Progress { observation }
                if matches!(observation.service, frontcap_core::native::webview_progress::Service::Observed { requests: 0, held: 0, .. })))));
    let call = f.call("uncertain", Op::ExecuteWebview { operation: WebOp::Clock { kind: ClockOperation::Advance, delta_ns: 50_000_000 } });
    let first = f.client.call(call.clone()).await.unwrap();
    assert!(matches!(&first, Reply::Native {body: NativeReply::Done {outcome,..}} if matches!(outcome.as_ref(), Outcome::WebviewUnresolved {..})));
    assert_eq!(f.client.call(call).await.unwrap(), first);
    let result = f.client.call(f.call("recover", Op::ExecuteWebview {operation: WebOp::Reconcile {request_id: id("uncertain")}})).await.unwrap();
    assert!(matches!(result, Reply::Native {body:NativeReply::Done {outcome,..}} if matches!(outcome.as_ref(), Outcome::WebviewCompleted { completion } if matches!(completion.as_ref(), WebCompletion::Reconciled {reply} if reply.now_ns==50_000_000 && reply.observation.canvas_kit.as_ref().is_some_and(|c| c.published && Some(&c.document)==reply.document.as_ref())))));
    let result = f.client.call(f.call("capture",Op::Observe)).await.unwrap();
    assert!(matches!(result, Reply::Native {body:NativeReply::Done {outcome,..}} if matches!(outcome.as_ref(),Outcome::WebviewCompleted { completion } if matches!(completion.as_ref(), WebCompletion::Captured {bytes,..} if *bytes>0))));
    let compare = Op::CompareWebviewProgress { before: id("progress"), after: id("progress"), before_capture: Some(id("capture")), after_capture: Some(id("capture")) };
    let answer = f.client.call(f.call("compare", compare.clone())).await.unwrap();
    assert_eq!(f.client.call(f.call("compare", compare)).await.unwrap(), answer);
    let Reply::Native { body: NativeReply::Done { outcome, .. } } = answer else { panic!() };
    let Outcome::WebviewCompared { comparison } = *outcome else { panic!() };
    assert!(comparison.changes.is_empty());
    assert_eq!(comparison.capture_content_changed, Some(false));
    assert_eq!(comparison.before.capture.unwrap().request_id, id("capture"));
    for (request, before, capture) in [("missing-evidence", "missing", "capture"), ("wrong-evidence", "recover", "capture"), ("wrong-capture", "progress", "progress")] {
        assert_code(f.client.call(f.call(request, Op::CompareWebviewProgress { before: id(before), after: id("progress"), before_capture: Some(id(capture)), after_capture: None })).await, ErrorCode::InvalidArguments);
        assert!(Journal::read_state(&runtime::journal_path(f.root.path()), &id(request)).unwrap().is_none());
    }
    let frontcap_recorder_protocol::native::Ready::Webview { owner: previous, .. } = status.ready else { panic!() };
    let restart = f.call("restart", Op::ExecuteWebview { operation: WebOp::Restart { generation: previous.endpoint.generation.clone() } });
    let restarted = f.client.call(restart.clone()).await.unwrap();
    assert!(matches!(&restarted, Reply::Native {body:NativeReply::Done {outcome,..}} if matches!(outcome.as_ref(),Outcome::WebviewCompleted { completion } if matches!(completion.as_ref(), WebCompletion::Restarted {owner,page_ready:false,..} if owner.process != previous.process))));
    assert_eq!(f.client.call(restart).await.unwrap(), restarted, "same ID cannot replace again");
    let Reply::Native { body: NativeReply::Status { status: current } } = f.client.call(Call::Status).await.unwrap() else { panic!() };
    assert!(matches!(current.ready, frontcap_recorder_protocol::native::Ready::Webview {owner,..} if owner.endpoint.generation != previous.endpoint.generation));
    assert_code(f.client.call(f.call("stale-restart", Op::ExecuteWebview { operation: WebOp::Restart { generation: previous.endpoint.generation } })).await, ErrorCode::InvalidArguments);
    let original = f.client.call(f.call("uncertain", Op::ExecuteWebview { operation: WebOp::Clock { kind: ClockOperation::Advance, delta_ns: 50_000_000 } })).await.unwrap();
    assert_eq!(original, first, "old outcome remains unchanged after replacement");
    let read = f.client.call(f.call("replacement-read", Op::ExecuteWebview { operation: WebOp::Clock { kind: ClockOperation::Read, delta_ns: 0 } })).await.unwrap();
    assert!(matches!(read, Reply::Native {body:NativeReply::Done {outcome,..}} if matches!(outcome.as_ref(),Outcome::WebviewCompleted { completion } if matches!(completion.as_ref(), WebCompletion::Clock {reply} if reply.now_ns==0 && reply.pid==44))));
    let calls=std::fs::read_to_string(f.root.path().join("native-session/calls.jsonl")).unwrap();
    assert_eq!(calls.lines().count(),6,"same-ID lookup must not reach the worker twice");
    f.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ios_host_restart_publishes_new_owner_and_does_not_replay_original_requests() {
    use frontcap_core::native::ios_host::{Operation as H, Completion as C};
    let mut f=Fixture::configured_host(false,false,true,None).await;
    let status:Status=load(&f.root.path().join("native-status.json")).unwrap().unwrap();
    assert!(status.capabilities.contains(&Capability::IosHostRestart));
    let frontcap_recorder_protocol::native::Ready::IosHost {owner:previous,..}=status.ready else {panic!()};
    let original=f.call("before",Op::ExecuteIosHost {operation:H::Observe {input_basis:false}});
    let before=f.client.call(original.clone()).await.unwrap();
    let restart=f.call("restart",Op::ExecuteIosHost {operation:H::Restart {previous:previous.clone()}});
    let answer=f.client.call(restart.clone()).await.unwrap();
    let Reply::Native {body:NativeReply::Done {outcome,..}}=&answer else {panic!()};
    let Outcome::IosHostCompleted {completion:C::Restarted {previous:old,ready}}=outcome.as_ref() else {panic!()};
    assert_eq!(old.as_ref(),&previous);assert_ne!(ready.owner.process,previous.process);
    let replacement=ready.owner.clone();
    assert_eq!(f.client.call(restart).await.unwrap(),answer);
    assert_eq!(f.client.call(original).await.unwrap(),before);
    assert_code(f.client.call(f.call("stale",Op::ExecuteIosHost {operation:H::Restart {previous}})).await,ErrorCode::InvalidArguments);
    let Reply::Native {body:NativeReply::Status {status}}=f.client.call(Call::Status).await.unwrap() else {panic!()};
    assert!(matches!(status.ready,frontcap_recorder_protocol::native::Ready::IosHost {owner,..} if owner==replacement));
    f.client.call(f.call("after",Op::ExecuteIosHost {operation:H::Observe {input_basis:false}})).await.unwrap();
    let calls=std::fs::read_to_string(f.root.path().join("native-session/calls.jsonl")).unwrap();
    assert_eq!(calls.lines().count(),3,"same-ID reads and stale restart cannot reach worker");
    f.stop().await;
}

#[tokio::test]
async fn storage_admission_preserves_receipts_and_allows_retirement() {
    let mut fixture = Fixture::start().await;
    let observe = fixture.call("saved-read", Op::Execute { operation: Operation::ReadClock {} });
    let original = fixture.client.call(observe).await.unwrap();
    let Reply::Native { body: NativeReply::Status { mut status } } =
        fixture.client.call(Call::Status).await.unwrap() else { panic!("status") };
    fixture.client.call(fixture.call("stop", Op::Stop)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), fixture.task).await.unwrap().unwrap().unwrap();
    status.stopped = false;
    let lease = LeaseToken::generate();
    let rec = Arc::new(Recorder {
        dir: fixture.root.path().into(),
        storage_roots: storage::Roots::runtime(fixture.root.path()).unwrap(),
        min_free_bytes: u64::MAX,
        write_budget: None,
        state: Mutex::new(State {
            journal: Journal::open(&runtime::journal_path(fixture.root.path())).unwrap(),
            status: *status, lease: Some((lease.clone(), 1)), detached: vec![], retiring: false,
        }),
        session: Mutex::new(None),
        session_volume: Mutex::new(None), service: None,
        shutdown: tokio::sync::Notify::new(),
    });
    let saved = rec.mutate(lease.clone(), id("saved-read"), Op::Execute { operation: Operation::ReadClock {} }, 5000).await;
    assert_eq!(serde_json::to_value(saved).unwrap(), serde_json::to_value(Answer::Ok(original)).unwrap());
    let refused = rec.mutate(lease.clone(), id("no-space"), Op::Observe, 5000).await;
    assert!(matches!(refused, Answer::Err(ref e) if e.message.contains("storage admission refused before dispatch")), "{refused:?}");
    assert!(rec.state.lock().unwrap().journal.state(&id("no-space")).is_none());
    let retired = rec.mutate(lease, id("retire-low-space"), Op::Stop, 5000).await;
    assert!(matches!(retired, Answer::Ok(Reply::Native { body: NativeReply::Done { outcome, .. } }) if matches!(*outcome, Outcome::Stopped { .. })));
    assert!(storage::admit(fixture.root.path(), 0).is_ok());
    assert!(storage::admit(&fixture.root.path().join("missing"), 0).is_err());
    let fresh = tempfile::tempdir().unwrap();
    let mut spec: LaunchSpec = load(&spec_path(fixture.root.path())).unwrap().unwrap();
    let external = tempfile::tempdir().unwrap();
    let avd = external.path().join("avd");
    let scratch = external.path().join("scratch");
    std::fs::create_dir(&avd).unwrap();
    std::fs::create_dir(&scratch).unwrap();
    let target::Launch::Android(config) = &mut spec.target else { panic!("Android fixture") };
    config.avd_home = Some(avd.clone());
    config.temporary_storage_root = Some(scratch.clone());
    spec.min_free_bytes = Some(u64::MAX);
    save_atomic(&spec_path(fresh.path()), &spec).unwrap();
    let failed = crate::run_launch(fresh.path()).await.unwrap_err().to_string();
    assert!(failed.contains("available bytes"), "{failed}");
    assert!(!fresh.path().join("native-session").exists());
    let roots: serde_json::Value = load(&fresh.path().join("native-storage-roots.json")).unwrap().unwrap();
    let registered = roots["roots"].as_array().unwrap();
    for (role, path) in [("runtime", fresh.path()), ("android_temporary_storage", scratch.as_path()),
                         ("android_avd_home", avd.as_path())] {
        let matching: Vec<_> = registered.iter().filter(|root| root["role"] == role).collect();
        assert_eq!(matching.len(), 1);
        assert_eq!(matching[0]["path"], serde_json::to_value(path).unwrap());
    }
    let record = runtime::load_runtime(fresh.path()).unwrap().unwrap();
    assert!(matches!(record.state, RuntimeState::Failed { ref reason, .. } if reason == &failed));
    let observed = crate::launcher::wait_ready(fresh.path(), Duration::ZERO).await.unwrap_err();
    assert_eq!(observed.0.message, failed);
    // Reusing the terminal generation must preserve the original refusal, not reacquire it.
    let before = std::fs::read(runtime::runtime_path(fresh.path())).unwrap();
    assert!(crate::run_launch(fresh.path()).await.is_err());
    assert_eq!(std::fs::read(runtime::runtime_path(fresh.path())).unwrap(), before);
}

#[tokio::test]
async fn storage_watch_does_not_reclassify_an_explicit_stop_while_it_retires() {
    let mut fixture = Fixture::start().await;
    let spec: LaunchSpec = load(&spec_path(fixture.root.path())).unwrap().unwrap();
    let Reply::Native { body: NativeReply::Status { mut status } } =
        fixture.client.call(Call::Status).await.unwrap() else { panic!("status") };
    let source = std::fs::read(&spec.session_executable).unwrap();
    fixture.stop().await;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("session.py");
    std::fs::write(&executable, source).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (session, ready) = Session::start(&executable, spec.target,
        &root.path().join("native-session"), Instant::now() + Duration::from_secs(5)).unwrap();
    status.ready = ready;
    let lease = LeaseToken::generate();
    let rec = Arc::new(Recorder {
        dir: root.path().into(), storage_roots: storage::Roots::runtime(root.path()).unwrap(), min_free_bytes: 0, write_budget: None,
        state: Mutex::new(State { journal: Journal::open(&runtime::journal_path(root.path())).unwrap(),
            status: *status, lease: Some((lease.clone(), 1)), detached: vec![], retiring: false }),
        session: Mutex::new(Some(session)), session_volume: Mutex::new(None), service: None, shutdown: tokio::sync::Notify::new(),
    });
    std::fs::write(root.path().join("stop-hold"), b"").unwrap();
    let active = rec.clone();
    let stop = tokio::spawn(async move { active.mutate(lease, id("explicit-stop"), Op::Stop, 5000).await });
    until_file(&root.path().join("native-session/stop-dispatched")).await;
    assert!(storage_watch::retire(&rec, "registered volume removed during Stop").unwrap().is_none());
    assert!(rec.state.lock().unwrap().status.failure.is_none());
    assert!(!root.path().join("native-storage-pressure.json").exists());
    std::fs::remove_file(root.path().join("stop-hold")).unwrap();
    let answer = stop.await.unwrap();
    assert!(matches!(answer, Answer::Ok(Reply::Native { body: NativeReply::Done { outcome, .. } })
        if matches!(outcome.as_ref(), Outcome::Stopped { retirement } if retirement.passed)));
    assert!(rec.state.lock().unwrap().status.failure.is_none());
}

#[tokio::test]
async fn storage_watch_waits_for_original_effect_then_retires_without_a_controller_request() {
    let mut fixture = Fixture::start().await;
    let spec: LaunchSpec = load(&spec_path(fixture.root.path())).unwrap().unwrap();
    let Reply::Native { body: NativeReply::Status { mut status } } =
        fixture.client.call(Call::Status).await.unwrap() else { panic!("status") };
    let source = std::fs::read(&spec.session_executable).unwrap();
    fixture.stop().await;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("session.py");
    std::fs::write(&executable, source).unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (session, ready) = Session::start(&executable, spec.target,
        &root.path().join("native-session"), Instant::now() + Duration::from_secs(5)).unwrap();
    status.ready = ready;
    status.stopped = false;
    status.failure = None;
    let lease = LeaseToken::generate();
    let limits = frontcap_core::write_budget::Limits { ordinary: 8 << 20, retirement: 8 << 20 };
    let budget = WriteBudget::create(&root.path().join("budget"), status.launch_id.clone(), limits).unwrap();
    let mut rec = Arc::new(Recorder {
        dir: root.path().into(),
        storage_roots: storage::Roots::runtime(root.path()).unwrap(), min_free_bytes: 0, write_budget: Some(budget.clone()),
        state: Mutex::new(State { journal: Journal::open(&runtime::journal_path(root.path())).unwrap(),
            status: *status, lease: Some((lease.clone(), 1)), detached: vec![], retiring: false }),
        session: Mutex::new(Some(session)), session_volume: Mutex::new(None), service: None, shutdown: tokio::sync::Notify::new(),
    });
    std::fs::write(root.path().join("hold"), b"").unwrap();
    let running = rec.clone();
    let active_lease = lease.clone();
    let active = tokio::spawn(async move {
        running.mutate(active_lease, id("original-effect"), Op::Execute {
            operation: Operation::Advance { by_us: 1000 },
        }, 5000).await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while !root.path().join("native-session/dispatched").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.unwrap();
    assert!(storage_watch::retire(&rec, "fixture storage pressure").unwrap().is_none());
    assert!(matches!(rec.state.lock().unwrap().journal.state(&id("original-effect")), Some(RequestState::Dispatched)));
    let denied = rec.mutate(lease, id("later-work"), Op::Observe, 5000).await;
    assert!(matches!(denied, Answer::Err(_)));
    assert!(rec.state.lock().unwrap().journal.state(&id("later-work")).is_none());
    assert!(!root.path().join("native-storage-pressure.json").exists());
    std::fs::remove_file(root.path().join("hold")).unwrap();
    let original = active.await.unwrap();
    assert!(matches!(original, Answer::Ok(Reply::Native { body: NativeReply::Done { .. } })));
    budget.charge(Class::Ordinary, limits.ordinary - budget.charged().unwrap().ordinary).unwrap();
    // Drive the actual server timer with an impossible floor, without exhausting the test host.
    Arc::get_mut(&mut rec).unwrap().min_free_bytes = u64::MAX;
    let listener = UnixListener::bind(root.path().join("watch.sock")).unwrap();
    tokio::time::timeout(Duration::from_secs(5), serve(rec.clone(), listener, None)).await.unwrap().unwrap();
    assert!(rec.state.lock().unwrap().status.stopped);
    assert!(rec.session.lock().unwrap().is_none());
    assert!(root.path().join("native-session/result.json").exists());
    let stored: Answer = load(&root.path().join("native-storage-retirement.json")).unwrap().unwrap();
    assert!(matches!(stored, Answer::Ok(Reply::Native { body: NativeReply::Done { outcome, .. } })
        if matches!(outcome.as_ref(), Outcome::Stopped { retirement } if retirement.passed)));
    let retained = result(rec.state.lock().unwrap().journal.state(&id("original-effect"))).unwrap();
    assert!(matches!(retained, RequestResult::Completed { answer }
        if serde_json::to_value(&answer).unwrap() == serde_json::to_value(&original).unwrap()));
}


#[tokio::test]
async fn external_session_storage_preserves_control_results_and_rejects_replaced_evidence() {
    let external = tempfile::tempdir().unwrap();
    let mut fixture = Fixture::configured_storage(false, false, false, None, Some(external.path().into())).await;
    let dir = fixture.root.path();
    let evidence = session_directory(dir).unwrap();
    assert!(evidence.starts_with(external.path()));
    assert!(!dir.join("native-session").exists());
    assert!(runtime::journal_path(dir).exists());
    let reply = fixture.client.call(Call::Native {
        lease: fixture.lease.clone(), request_id: id("external-advance"), budget_ms: 5000, op: Op::Execute { operation: Operation::Advance { by_us: 1000 } },
    }).await.unwrap();
    assert!(matches!(reply, Reply::Native { .. }));
    let stopped = fixture.client.call(Call::Native {
        lease: fixture.lease.clone(), request_id: id("external-stop"), budget_ms: 5000, op: Op::Stop,
    }).await.unwrap();
    assert!(matches!(stopped, Reply::Native { .. }));
    fixture.task.await.unwrap().unwrap();
    let original_journal = std::fs::read(runtime::journal_path(dir)).unwrap();
    let read_original = || {
        let Some(RequestState::Completed(value)) = Journal::read_state(&runtime::journal_path(dir), &id("external-advance")).unwrap() else { panic!("missing completed result") };
        let answer: Answer = serde_json::from_value(value).unwrap();
        assert_eq!(serde_json::to_value(answer).unwrap(), serde_json::to_value(Answer::Ok(reply.clone())).unwrap());
    };
    read_original();
    assert!(session_directory(dir).is_ok());
    let original = external.path().join("original-evidence");
    std::fs::rename(&evidence, &original).unwrap();
    std::fs::create_dir(&evidence).unwrap();
    std::fs::write(evidence.join("foreign"), b"preserve").unwrap();
    assert!(session_directory(dir).unwrap_err().to_string().contains("identity changed"));
    assert_eq!(std::fs::read(evidence.join("foreign")).unwrap(), b"preserve");
    read_original();
    assert_eq!(std::fs::read(runtime::journal_path(dir)).unwrap(), original_journal);
    assert!(runtime::journal_path(dir).exists());
}

#[tokio::test]
async fn write_budget_refuses_before_intent_but_preserves_original_result_and_stop() {
    let limits = frontcap_core::write_budget::Limits { ordinary: 8 << 20, retirement: 8 << 20 };
    let mut fixture = Fixture::configured_budget(false, false, false, None, None, Some(limits)).await;
    let original = fixture.client.call(fixture.call("original", Op::Execute { operation: Operation::ReadClock {} })).await.unwrap();
    let spec: LaunchSpec = load(&spec_path(fixture.root.path())).unwrap().unwrap();
    let budget = WriteBudget::open(&fixture.root.path().join("native-write-budget"), spec.launch_id, limits).unwrap();
    budget.charge(Class::Ordinary, limits.ordinary - budget.charged().unwrap().ordinary).unwrap();
    let journal_before = std::fs::read(runtime::journal_path(fixture.root.path())).unwrap();
    assert!(matches!(fixture.client.call(fixture.call("refused", Op::Execute { operation: Operation::ReadClock {} })).await,
        Err(ClientError::Recorder(e)) if e.message.contains("before dispatch")));
    assert_eq!(std::fs::read(runtime::journal_path(fixture.root.path())).unwrap(), journal_before);
    assert!(!fixture.root.path().join("native-requests/refused.json").exists());
    let retry = fixture.client.call(fixture.call("original", Op::Execute { operation: Operation::ReadClock {} })).await.unwrap();
    assert_eq!(serde_json::to_value(original).unwrap(), serde_json::to_value(retry).unwrap());
    fixture.client.call(fixture.call("stop", Op::Stop)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut fixture.task).await.unwrap().unwrap().unwrap();
    assert!(matches!(Journal::read_state(&runtime::journal_path(fixture.root.path()), &id("stop")).unwrap(), Some(RequestState::Completed(_))));
    assert!(budget.charged().unwrap().retirement > 0);
}

#[tokio::test]
async fn write_budget_completion_exhaustion_preserves_effect_and_uncertainty() {
    use frontcap_core::native::service::{Control, ReleaseReceipt, ReleaseState, ResponseSpec};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut fixture = Fixture::platform_service(false, Some(ServiceMode::ControlledHttp)).await;
    let directory = fixture.root.path().to_owned();
    let spec: LaunchSpec = load(&spec_path(&directory)).unwrap().unwrap();
    let limits = spec.write_budget.unwrap();
    let budget = WriteBudget::open(&directory.join("native-write-budget"), spec.launch_id.clone(), limits).unwrap();
    let initial: frontcap_service_fixture::Record = load(&directory.join("service/state.json")).unwrap().unwrap();
    let mut stream = tokio::net::TcpStream::connect(initial.address).await.unwrap();
    stream.write_all(format!("GET /{}/requests/load HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n", initial.generation).as_bytes()).await.unwrap();
    let mut prepared = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let record: frontcap_service_fixture::Record = load(&directory.join("service/state.json")).unwrap().unwrap();
            if record.requests.contains_key(&id("load")) { break record; }
            tokio::task::yield_now().await;
        }
    }).await.unwrap();
    let response = ResponseSpec { status: 200, body: "original response".into() };
    let op = Op::ReleaseService { generation: initial.generation.clone(), request: id("load"), response: response.clone() };
    let request = id("release");
    let command = Control::Release { generation: initial.generation, id: request.clone(), request: id("load"), response };
    prepared.releases.insert(request.clone(), ReleaseReceipt { command, state: ReleaseState::Prepared });
    prepared.requests.get_mut(&id("load")).unwrap().release = Some(request.clone());
    let prepared_bytes = frontcap_core::canonical::canonical_of(&prepared).len() as u64;
    prepared.releases.get_mut(&request).unwrap().state = ReleaseState::QueuedForHttp;
    let queued_bytes = frontcap_core::canonical::canonical_of(&prepared).len() as u64;
    prepared.releases.get_mut(&request).unwrap().state = ReleaseState::ReceiverGone;
    let final_bytes = queued_bytes.max(frontcap_core::canonical::canonical_of(&prepared).len() as u64);
    // Admit exactly the recorder intent/dispatch/plan and both service publications, but no
    // recorder result. The actual HTTP effect must survive this separate publication refusal.
    let hash = args_sha256(&serde_json::to_value((&op, 5000_u64)).unwrap());
    let allowed = Journal::admission_bytes(&request, &hash) + recovery::record_bytes(&op, 5000)
        + prepared_bytes + final_bytes;
    budget.charge(Class::Ordinary, limits.ordinary - budget.charged().unwrap().ordinary - allowed).unwrap();
    assert!(matches!(fixture.client.call(fixture.call("release", op)).await, Err(ClientError::Recorder(_))));
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut bytes)).await.unwrap().unwrap();
    assert!(bytes.ends_with(b"original response"));
    assert_eq!(Journal::read_state(&runtime::journal_path(&directory), &request).unwrap(), Some(RequestState::Dispatched));
    assert!(matches!(recovery::saved_result(&directory, &spec.launch_id, &request).unwrap(), RequestResult::Pending { dispatched: true }));
    let service_before: frontcap_service_fixture::Record = load(&directory.join("service/state.json")).unwrap().unwrap();
    assert_eq!(service_before.releases[&request].state, ReleaseState::QueuedForHttp);
    fixture.client.call(fixture.call("stop", Op::Stop)).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), &mut fixture.task).await.unwrap().unwrap().unwrap();
    assert!(matches!(Journal::read_state(&runtime::journal_path(fixture.root.path()), &id("stop")).unwrap(), Some(RequestState::Completed(_))));
    let service_after: frontcap_service_fixture::Record = load(&directory.join("service/state.json")).unwrap().unwrap();
    assert_eq!(serde_json::to_value(&service_after.releases[&request]).unwrap(), serde_json::to_value(&service_before.releases[&request]).unwrap());
    assert!(matches!(recovery::saved_result(&directory, &spec.launch_id, &request).unwrap(), RequestResult::Pending { dispatched: true }));
    assert_eq!(Journal::read_state(&runtime::journal_path(&directory), &request).unwrap(), Some(RequestState::Dispatched));
}

#[tokio::test]
async fn write_budget_bootstrap_refusal_retains_failure_without_starting_or_resetting() {
    let template = Fixture::start().await;
    let mut spec: LaunchSpec = load(&spec_path(template.root.path())).unwrap().unwrap();
    template.stop().await;
    let root = tempfile::tempdir().unwrap();
    spec.launch_id = LaunchId::generate();
    spec.session_executable = root.path().join("must-not-start");
    spec.service = Some(ServiceMode::ControlledHttp);
    let limits = frontcap_core::write_budget::Limits { ordinary: 1, retirement: 65536 };
    spec.write_budget = Some(limits);
    save_atomic(&spec_path(root.path()), &spec).unwrap();
    let failure = crate::run_launch(root.path()).await.unwrap_err().to_string();
    assert!(failure.contains("write allowance exhausted"));
    assert!(!root.path().join("service").exists());
    assert!(!root.path().join("native-session").exists());
    assert!(!root.path().join("native-storage-roots.json").exists());
    assert!(!root.path().join("native-session-location.json").exists());
    let runtime = runtime::load_runtime(root.path()).unwrap().unwrap();
    assert!(matches!(runtime.state, RuntimeState::Failed { ref reason, .. } if reason == &failure));
    let budget = WriteBudget::open(&root.path().join("native-write-budget"), spec.launch_id.clone(), limits).unwrap();
    let spent = budget.charged().unwrap();
    assert_eq!(spent.ordinary, 0);
    assert!(spent.retirement > 0);
    let original = std::fs::read(runtime::runtime_path(root.path())).unwrap();
    assert!(crate::run_launch(root.path()).await.unwrap_err().to_string().contains(&failure));
    assert_eq!(budget.charged().unwrap(), spent);
    assert_eq!(std::fs::read(runtime::runtime_path(root.path())).unwrap(), original);
    // Losing allowance evidence cannot trigger unbudgeted fallback or allowance recreation.
    std::fs::rename(root.path().join("native-write-budget/ledger.json"), root.path().join("ledger.saved")).unwrap();
    assert!(mark_failed(root.path(), "replacement reason", None).is_err());
    assert!(!root.path().join("native-write-budget/ledger.json").exists());
    assert_eq!(std::fs::read(runtime::runtime_path(root.path())).unwrap(), original);
}

#[tokio::test]
async fn storage_watch_classifies_worker_exit_before_unavailable_storage() {
    let mut fixture = Fixture::start().await;
    let spec: LaunchSpec = load(&spec_path(fixture.root.path())).unwrap().unwrap();
    let Reply::Native { body: NativeReply::Status { mut status } } =
        fixture.client.call(Call::Status).await.unwrap() else { panic!("status") };
    let source = std::fs::read(&spec.session_executable).unwrap();
    fixture.stop().await;
    for flutter in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("session.py");
        std::fs::write(&executable, &source).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = if flutter {
            let worker = root.path().join("flutter-worker.py");
            std::fs::write(&worker, include_str!("../../tests/fixtures/native/flutter-worker.py")).unwrap();
            target::Launch::Flutter(serde_json::from_value(serde_json::json!({
                "worker": {"python": "/usr/bin/python3", "worker_script": worker, "config": {
                    "flutter": "/unused/flutter", "project": root.path(), "platform": "ios",
                    "source_sha256": "c".repeat(64), "acquisition_timeout_s": 10, "lifetime_s": 30
                }}, "http_script": "/unused/http"
            })).unwrap())
        } else { spec.target.clone() };
        let (session, ready) = Session::start(&executable, target,
            &root.path().join("native-session"), Instant::now() + Duration::from_secs(5)).unwrap();
        status.ready = ready;
        status.stopped = false;
        status.failure = None;
        let lease = LeaseToken::generate();
        let limits = frontcap_core::write_budget::Limits { ordinary: 8 << 20, retirement: 8 << 20 };
        let budget = WriteBudget::create(&root.path().join("budget"), status.launch_id.clone(), limits).unwrap();
        let rec = Arc::new(Recorder {
            dir: root.path().into(),
            storage_roots: storage::Roots::runtime(root.path()).unwrap(), min_free_bytes: u64::MAX, write_budget: Some(budget.clone()),
            state: Mutex::new(State { journal: Journal::open(&runtime::journal_path(root.path())).unwrap(),
                status: (*status).clone(), lease: Some((lease.clone(), 1)), detached: vec![], retiring: false }),
            session: Mutex::new(Some(session)), session_volume: Mutex::new(None), service: None, shutdown: tokio::sync::Notify::new(),
        });
        assert!(!storage_watch::worker_exited(&rec).unwrap());
        let identity = if flutter { "native-session/session/worker-process.json" } else { "native-session/worker.json" };
        let worker: ProcessIdentity = load(&root.path().join(identity)).unwrap().unwrap();
        assert!(worker.kill().unwrap());
        assert!(storage_watch::worker_exited(&rec).unwrap());
        let listener = UnixListener::bind(root.path().join("loss.sock")).unwrap();
        tokio::time::timeout(Duration::from_secs(10), serve(rec.clone(), listener, None)).await.unwrap().unwrap();
        let state = rec.state.lock().unwrap();
        assert!(state.status.failure.as_ref().unwrap().contains("session worker exited"));
        assert!(!root.path().join("native-storage-pressure.json").exists());
        assert!(!root.path().join("native-storage-retirement.json").exists());
        assert!(root.path().join("native-worker-loss.json").exists());
        // The substitute worker has no independent group: lost ownership cannot fabricate cleanup.
        let result: Answer = load(&root.path().join("native-worker-retirement.json")).unwrap().unwrap();
        assert!(matches!(result, Answer::Err(_)));
    }
}


#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preparation_cancellation_retires_unready_worker_and_recovers_original_admission() {
    use preparation_control::{Cancel, Cancellation};
    let fixture = Fixture::start().await;
    let mut spec: LaunchSpec = load(&spec_path(fixture.root.path())).unwrap().unwrap();
    fixture.stop().await;
    let root = tempfile::tempdir().unwrap();
    let worker = root.path().join("waiting.py");
    std::fs::write(&worker, r#"#!/usr/bin/env python3
import pathlib,sys,json,os
root=pathlib.Path(sys.argv[1])
(root/'waiting-pid').write_text(str(os.getpid()))
sys.stdin.buffer.read()
(root/'cancelled-by-eof').touch()
(root/'result.json').write_text(json.dumps(dict(error=None,retirement=dict(passed=True,errors=[],signal_error=None))))
"#).unwrap();
    std::fs::set_permissions(&worker, std::fs::Permissions::from_mode(0o700)).unwrap();
    spec.launch_id = LaunchId::generate();
    spec.session_executable = worker;
    spec.service = None;
    spec.acquisition_budget_ms = 15000;
    save_atomic(&spec_path(root.path()), &spec).unwrap();
    let directory = root.path().to_path_buf();
    let task = tokio::spawn(async move {crate::run_launch(&directory).await});
    tokio::time::timeout(Duration::from_secs(5), async {
        while !root.path().join("native-session/waiting-pid").exists() {tokio::task::yield_now().await;}
    }).await.unwrap();
    let request = Cancel {launch_id:spec.launch_id.clone(), request_id:id("cancel-original")};
    let reply = preparation_control::cancel(root.path(), request.clone()).await.unwrap();
    assert_eq!(reply, Cancellation::Requested { request: request.clone() });
    let result = tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
    assert!(result.is_err());
    assert!(root.path().join("native-session/cancelled-by-eof").exists());
    assert!(!root.path().join("native-status.json").exists());
    assert!(matches!(runtime::load_runtime(root.path()).unwrap().unwrap().state, RuntimeState::Failed {..}));
    assert_eq!(preparation_control::cancel(root.path(), request).await.unwrap(), reply);
    assert!(matches!(preparation_control::cancel(root.path(), Cancel {
        launch_id:spec.launch_id, request_id:id("different-cancel") }).await.unwrap(), Cancellation::NotSent {..}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preparation_cancellation_cannot_preempt_a_ready_lease() {
    let fixture = Fixture::start().await;
    let status: Status = load(&fixture.root.path().join("native-status.json")).unwrap().unwrap();
    let result = preparation_control::cancel(fixture.root.path(), preparation_control::Cancel {
        launch_id: status.launch_id, request_id:id("too-late") }).await.unwrap();
    assert!(matches!(result, preparation_control::Cancellation::NotSent {..}));
    assert!(!fixture.root.path().join("preparation-cancel.json").exists());
    assert_eq!(runtime::load_runtime(fixture.root.path()).unwrap().unwrap().state, RuntimeState::Ready);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preparation_retirement_readback_waits_for_owners_and_never_rewrites_evidence() {
    use preparation::TargetRetirement;
    let fixture = Fixture::start().await;
    let spec: LaunchSpec = load(&spec_path(fixture.root.path())).unwrap().unwrap();
    let mut record = runtime::load_runtime(fixture.root.path()).unwrap().unwrap();
    fixture.stop().await;
    let root = tempfile::tempdir().unwrap();
    save_atomic(&spec_path(root.path()), &spec).unwrap();
    record.state = RuntimeState::Failed {reason:"cancelled".into(), error:None};
    runtime::save_runtime(root.path(), &record).unwrap();
    let session = root.path().join("native-session");
    std::fs::create_dir(&session).unwrap();
    let lock = std::fs::File::create(session.join("session.lock")).unwrap();
    let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let exited = ProcessIdentity::of(child.id()).unwrap().unwrap();
    child.kill().unwrap(); child.wait().unwrap();
    save_atomic(&session.join("worker.json"), &exited).unwrap();
    let result = serde_json::json!({"error":null,"retirement":{"passed":true,"errors":[],"signal_error":null}});
    save_atomic(&session.join("result.json"), &result).unwrap();
    // The recorder is still live, even though the synthetic target already has a terminal record.
    assert!(matches!(preparation::inspect(root.path(), &spec.launch_id).unwrap().target_retirement, TargetRetirement::Pending));
    record.recorder = exited.clone(); runtime::save_runtime(root.path(), &record).unwrap();
    save_atomic(&session.join("worker.json"), &ProcessIdentity::current()).unwrap();
    assert!(matches!(preparation::inspect(root.path(), &spec.launch_id).unwrap().target_retirement, TargetRetirement::Pending));
    save_atomic(&session.join("worker.json"), &exited).unwrap();
    lock.lock().unwrap();
    assert!(matches!(preparation::inspect(root.path(), &spec.launch_id).unwrap().target_retirement, TargetRetirement::Pending));
    drop(lock);
    let before = std::fs::read(session.join("result.json")).unwrap();
    assert!(matches!(preparation::inspect(root.path(), &spec.launch_id).unwrap().target_retirement,
        TargetRetirement::Recorded { retirement } if retirement.passed));
    assert_eq!(std::fs::read(session.join("result.json")).unwrap(), before);
    std::fs::remove_file(session.join("result.json")).unwrap();
    assert!(matches!(preparation::inspect(root.path(), &spec.launch_id).unwrap().target_retirement, TargetRetirement::Pending));
    std::fs::write(session.join("result.json"), b"partial").unwrap();
    assert!(matches!(preparation::inspect(root.path(), &spec.launch_id).unwrap().target_retirement, TargetRetirement::Unavailable {..}));
    assert_eq!(std::fs::read(session.join("result.json")).unwrap(), b"partial");
    let failed = serde_json::json!({"error":"cancelled","retirement":{"passed":false,"errors":["owned resource remains"],"signal_error":null}});
    save_atomic(&session.join("result.json"), &failed).unwrap();
    assert!(matches!(preparation::inspect(root.path(), &spec.launch_id).unwrap().target_retirement,
        TargetRetirement::Recorded { retirement } if !retirement.passed));
}
