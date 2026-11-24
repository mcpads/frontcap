use super::*;
use crate::request::{ClockOperation, ClockRequest};
use crate::simulator_app::tests::setup;
use crate::webview::{Endpoint, Reply as ClockReply, State as PageState, Status};
use std::net::TcpListener;
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(15)
}

#[test]
fn worker_entry() {
    let Some(path) = std::env::var_os("FRONTCAP_WEBVIEW_SESSION_TEST_DIRECTORY") else {
        return;
    };
    let directory = PathBuf::from(path);
    let host = directory.parent().unwrap().join("host");
    run_with(&directory, |config, target, end, cancelled, host_mode| {
        std::fs::create_dir(target)?;
        let simulator = Simulator::acquire_at(
            config.simulator,
            &target.join("simulator"),
            end,
            cancelled,
            &host,
        )?;
        let app = App::launch(simulator, config.app, &target.join("app"), end, cancelled)?;
        if host_mode {
            crate::ios_host::Target::connect(app,&target.join("host"),config.contacts,end,cancelled).map(|t|Owned::Host(Box::new(t)))
        } else { Target::connect(app, &target.join("webview"), end, cancelled).map(|t|Owned::Page(Box::new(t))) }
    })
    .unwrap();
}
fn spawn(directory: &Path, config: Config) -> Child { spawn_mode(directory,config,false) }
fn spawn_mode(directory: &Path, config: Config, host: bool) -> Child {
    spawn_mode_budgeted(directory, config, host, None)
}
fn spawn_mode_budgeted(directory: &Path, config: Config, host: bool,
    budget: Option<&frontcap_core::write_budget::WriteBudget>) -> Child {
    std::fs::create_dir(directory).unwrap();
    File::create_new(directory.join("session.lock")).unwrap();
    save_atomic(
        &directory.join("launch.json"),
        &Launch {
            host,
            config,
            deadline_ns: deadline_ns(deadline()).unwrap(),
        },
    )
    .unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
            "--exact",
            "webview_session::tests::worker_entry",
            "--nocapture",
        ])
        .env("FRONTCAP_WEBVIEW_SESSION_TEST_DIRECTORY", directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::null());
    frontcap_core::write_budget::WriteBudget::configure_child(&mut command, budget).unwrap();
    let mut child = command.spawn().unwrap();
    send(&mut child, &Message::Start);
    child
}
fn send(child: &mut Child, message: &Message) {
    let mut bytes = serde_json::to_vec(message).unwrap();
    bytes.push(b'\n');
    child.stdin.as_mut().unwrap().write_all(&bytes).unwrap();
}
fn wait_for(child: &mut Child, path: &Path) {
    let end = deadline();
    while !path.exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "worker exited before {}",
            path.display()
        );
        assert!(Instant::now() < end, "missing {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn finish(child: &mut Child, directory: &Path) -> SessionResult {
    child.stdin.take();
    let end = deadline();
    while child.try_wait().unwrap().is_none() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
    let Recovery::Finished { result, .. } = inspect(directory).unwrap() else {
        panic!("missing terminal proof")
    };
    assert!(result.retirement.as_ref().unwrap().passed);
    result
}
fn ready_session(disconnect: bool) {
    let (fixture, app) = setup("normal");
    let root = fixture.root.path();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = Endpoint {
        generation: "23357f73-a68f-4271-a765-c8fb5538acc3".into(),
        pid: std::process::id(),
        port: listener.local_addr().unwrap().port(),
    };
    std::fs::create_dir(root.join("Documents")).unwrap();
    save_atomic(&root.join("Documents/clock-control.json"), &endpoint).unwrap();
    let peer = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut header = [0; 4];
            stream.read_exact(&mut header).unwrap();
            let mut bytes = vec![0; u32::from_be_bytes(header) as usize];
            stream.read_exact(&mut bytes).unwrap();
            let request: ClockRequest = serde_json::from_slice(&bytes).unwrap();
            let reply = ClockReply {
                observation: Default::default(),
                generation: endpoint.generation.clone(),
                pid: endpoint.pid,
                port: endpoint.port,
                last_id: request.request_id,
                request,
                now_ns: 0,
                replayed: false,
                status: Status::Ok,
                applied: Some(false),
                document: Some("87c86105-02ad-4d07-bc0c-58c922960225".into()),
                state: Some(PageState {
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
    let directory = root.join("session");
    let mut child = spawn(
        &directory,
        Config {
            lifetime_ms: 900_000,
            lifecycle: None,
            system_ui: None,
            contacts: false,
            service_url_environment: None,
            simulator: fixture.config.clone(),
            app,
        },
    );
    wait_for(&mut child, &directory.join("ready.json"));
    assert!(matches!(inspect(&directory).unwrap(), Recovery::Active));
    for (number, operation) in [
        (
            1,
            Operation::Clock {
                kind: ClockOperation::Read,
                delta_ns: 0,
            },
        ),
        (
            2,
            Operation::Reconcile {
                request_id: RequestId::new("inspect").unwrap(),
            },
        ),
    ] {
        send(
            &mut child,
            &Message::Execute {
                number,
                id: RequestId::new("inspect").unwrap(),
                operation,
                deadline_ns: deadline_ns(deadline()).unwrap(),
            },
        );
        let path = directory.join(format!("commands/{number}/result.json"));
        wait_for(&mut child, &path);
        assert!(matches!(
            load::<Reply>(&path).unwrap().unwrap(),
            Reply::Completed { .. }
        ));
    }
    if !disconnect {
        send(&mut child, &Message::Stop);
    }
    let result = finish(&mut child, &directory);
    if disconnect {
        assert!(result.error.unwrap().contains("disconnected"));
    } else {
        assert!(result.error.is_none());
    }
    assert!(
        directory
            .join("target/webview/last-native-receipt.json")
            .exists()
    );
    assert!(root.join("deleted").exists());
    peer.join().unwrap();
}
#[test]
fn controller_eof_during_boot_cancels_acquisition_and_retires_created_device() {
    let (fixture, app) = setup("normal");
    let root = fixture.root.path();
    std::fs::write(root.join("mode"), "slow-boot").unwrap();
    let directory = root.join("session");
    let mut child = spawn(
        &directory,
        Config {
            lifetime_ms: 900_000,
            lifecycle: None,
            system_ui: None,
            contacts: false,
            service_url_environment: None,
            simulator: fixture.config.clone(),
            app,
        },
    );
    wait_for(&mut child, &root.join("boot-started"));
    let result = finish(&mut child, &directory);
    assert!(result.error.is_some());
    assert!(root.join("deleted").exists());
    assert!(!directory.join("ready.json").exists());
}

#[test]
fn controller_eof_retires_ready_target_after_clock_and_recovery_commands() {
    ready_session(true);
}
#[test]
fn explicit_stop_retires_ready_target_without_disconnect_error() {
    ready_session(false);
}

#[test]
fn host_worker_binds_original_capture_without_page_readiness_and_retires_on_eof() {
    use frontcap_core::native::ios_host as host;
    let (fixture,mut app)=setup("normal");app.script=None;
    let root=fixture.root.path();
    let file=File::create(root.join("pixel.png")).unwrap();let mut encoder=png::Encoder::new(file,1,1);encoder.set_color(png::ColorType::Rgb);encoder.write_header().unwrap().write_image_data(&[0,0,0]).unwrap();
    let script=std::fs::read_to_string(&fixture.config.xcrun).unwrap().replace("elif args[1]=='bootstatus':pass","elif args[1]=='io':pathlib.Path(args[-1]).write_bytes((p/'pixel.png').read_bytes())\nelif args[1]=='bootstatus':pass");std::fs::write(&fixture.config.xcrun,script).unwrap();
    let config=Config {lifetime_ms: 900_000, simulator:fixture.config.clone(),app:app.clone(),contacts:false,system_ui:None,lifecycle:None,service_url_environment:None};
    let host_config=crate::ios_host::Config {lifetime_ms: 900_000, simulator:fixture.config.clone(),app,contacts:false,system_ui:None};
    let directory=root.join("session");let mut child=spawn_mode(&directory,config,true);
    wait_for(&mut child,&directory.join("host-ready.json"));assert!(!directory.join("ready.json").exists());
    let id=RequestId::new("catalog").unwrap();let operation=host::Operation::Observe {input_basis:false};
    let mut first=None;
    for number in 1..=3 {
        let request=if number==3 {host::Operation::Observe {input_basis:true}} else {operation.clone()};
        send(&mut child,&Message::Execute {number,id:id.clone(),operation:Operation::Host {request},deadline_ns:deadline_ns(deadline()).unwrap()});
        let p=directory.join(format!("commands/{number}/result.json"));wait_for(&mut child,&p);
        let reply:serde_json::Value=load(&p).unwrap().unwrap();
        if number==1 {first=Some(reply)} else if number==2 {assert_eq!(Some(reply),first)} else {assert_eq!(reply["state"],"failed")}
    }
    let recovered=crate::ios_host_session::read_result(&directory,&host_config,&id,&operation).unwrap().unwrap();assert_eq!(serde_json::to_value(recovered).unwrap(),first.unwrap());
    let image=directory.join("target/host/capture-catalog/screen.png");std::fs::write(image,b"changed").unwrap();assert!(crate::ios_host_session::read_result(&directory,&host_config,&id,&operation).is_err());
    let result=finish(&mut child,&directory);assert!(result.error.unwrap().contains("controller disconnected"));
}

fn finish_during_idle_health_probe(disconnect: bool) {
    let (fixture, mut app) = setup("normal");
    app.script = None;
    let root = fixture.root.path();
    let script = std::fs::read_to_string(&fixture.config.xcrun).unwrap().replace(
        "    print(json.dumps({'devices':",
        "    if (p/'probe-armed').exists():\n        (p/'probe-armed').unlink()\n        (p/'probe-started').write_text('yes')\n        end=time.monotonic()+5\n        while not (p/'probe-release').exists() and time.monotonic()<end:time.sleep(0.01)\n    print(json.dumps({'devices':",
    );
    std::fs::write(&fixture.config.xcrun, script).unwrap();
    let directory = root.join("session");
    let mut child = spawn_mode(&directory, Config {
        lifetime_ms: 900_000,
        simulator: fixture.config.clone(), app, contacts: false,
        system_ui: None, lifecycle: None, service_url_environment: None,
    }, true);
    wait_for(&mut child, &directory.join("host-ready.json"));
    std::fs::write(root.join("probe-armed"), b"yes").unwrap();
    wait_for(&mut child, &root.join("probe-started"));
    if disconnect { child.stdin.take(); } else { send(&mut child, &Message::Stop); }
    // Hold the platform command across delivery of Stop/EOF to the worker's reader.
    std::thread::sleep(Duration::from_millis(200));
    std::fs::write(root.join("probe-release"), b"yes").unwrap();
    let result = finish(&mut child, &directory);
    if disconnect { assert!(result.error.is_some()); }
    else { assert!(result.error.is_none(), "{:?}", result.error); }
    assert!(root.join("deleted").exists());
}

#[test]
fn graceful_stop_during_idle_health_probe_retires_without_error() {
    finish_during_idle_health_probe(false);
}

#[test]
fn controller_eof_during_idle_health_probe_cancels_and_retires() {
    finish_during_idle_health_probe(true);
}

#[test]
fn independent_retirement_recovers_failed_stop_without_rewriting_worker_evidence() {
    use crate::resource_group::{self, Group};
    let (fixture, app) = setup("normal");
    let root = fixture.root.path().canonicalize().unwrap();
    let config = Config { lifetime_ms: 30000, simulator: fixture.config.clone(), app,
        lifecycle: None, system_ui: None, contacts: false, service_url_environment: None };
    let group_config = resource_group::Config { host: root.join("host"), target: root.join("target"),
        evidence_root: Some(root.clone()), artifact_budget_bytes: None, temporary_storage_root: None, scratch_volume: None, scratch_volume_root: None, resource_executable: "/bin/sh".into(),
        resource_arguments: group_arguments("resource_entry"), ports: vec![],
        deadline_ns: monotonic_ns().unwrap()+30_000_000_000 };
    File::create_new(root.join("session.lock")).unwrap();
    save_atomic(&root.join("launch.json"), &Launch { config, host: true, deadline_ns: group_config.deadline_ns }).unwrap();
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
    let failure = SessionResult { error: Some("resource group command cancelled".into()),
        retirement: Some(Retirement { passed: false, errors: vec!["authority revoked".into()], signal_error: None }) };
    save_atomic(&root.join("result.json"), &failure).unwrap();
    let worker_result = std::fs::read(root.join("result.json")).unwrap();
    assert!(matches!(inspect(&root).unwrap(), Recovery::Finished { .. }));
    assert!(!inspect(&root).unwrap().retirement().unwrap().passed);
    worker.kill().unwrap(); worker.wait().unwrap();
    assert!(matches!(inspect(&root).unwrap(), Recovery::ResourceCleanup { .. }));
    assert!(group.stop(Instant::now()+Duration::from_secs(15)).unwrap().passed);
    // Independent cleanup must not rewrite the original failed worker result.
    let original = std::fs::read(root.join("target-resources/retirement.json")).unwrap();
    for _ in 0..2 {
        let outcome = inspect(&root).unwrap();
        assert!(matches!(&outcome, Recovery::RetirementRecovered { result, .. } if result.error == failure.error));
        assert!(outcome.retirement().unwrap().passed);
    }
    assert_eq!(std::fs::read(root.join("target-resources/retirement.json")).unwrap(), original);
    assert_eq!(std::fs::read(root.join("result.json")).unwrap(), worker_result);
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
    assert_eq!(std::fs::read(root.join("result.json")).unwrap(), worker_result);
}
fn group_arguments(entry: &str) -> Vec<String> {
    vec!["-c".into(), format!("export FRONTCAP_RESOURCE_GROUP_DIRECTORY=\"$2\"; exec \"$1\" --exact resource_group::tests::{entry} --nocapture"),
        "retirement-reader".into(), std::env::current_exe().unwrap().to_str().unwrap().into()]
}

#[test]
fn ordinary_worker_exhaustion_refuses_capture_and_retains_terminal_cleanup() {
    use frontcap_core::{ids::LaunchId, write_budget::{Limits, WriteBudget}};
    let (fixture, mut app) = setup("normal"); app.script = None;
    let root = fixture.root.path();
    let limit = 16 * 1024 * 1024;
    let budget = WriteBudget::create(&root.join("budget"), LaunchId::generate(),
        Limits { ordinary: limit, retirement: 1024 * 1024 }).unwrap();
    let directory = root.join("session");
    let config = Config { lifetime_ms: 30_000, simulator: fixture.config.clone(), app,
        contacts: false, system_ui: None, lifecycle: None, service_url_environment: None };
    let mut child = spawn_mode_budgeted(&directory, config, true, Some(&budget));
    wait_for(&mut child, &directory.join("host-ready.json"));
    assert!(budget.charged().unwrap().ordinary > 0);
    budget.charge(Class::Ordinary, limit - budget.charged().unwrap().ordinary).unwrap();
    send(&mut child, &Message::Execute { number: 1, id: RequestId::new("capture").unwrap(),
        operation: Operation::Host { request: frontcap_core::native::ios_host::Operation::Observe { input_basis: false } },
        deadline_ns: deadline_ns(deadline()).unwrap() });
    wait_for(&mut child, &directory.join("result.json"));
    let result = finish(&mut child, &directory);
    assert!(result.error.unwrap().contains("write allowance exhausted"));
    assert!(result.retirement.unwrap().passed);
    assert!(!directory.join("commands/1/command.json").exists());
    assert!(!directory.join("host-requests/capture").exists());
    assert!(!std::fs::read_to_string(root.join("calls")).unwrap().contains("\"io\""));
    let charged = budget.charged().unwrap();
    assert_eq!(charged.ordinary, limit);
    assert!(charged.retirement > 0);
    let original = std::fs::read(directory.join("result.json")).unwrap();
    assert!(matches!(inspect(&directory).unwrap(), Recovery::Finished { .. }));
    assert_eq!(std::fs::read(directory.join("result.json")).unwrap(), original);
    assert_eq!(budget.charged().unwrap(), charged);
}

#[test]
fn finished_session_inspection_releases_lock_while_child_retains_descriptor() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path();
    File::create_new(directory.join("session.lock")).unwrap();
    let lease = inspection_lock(directory).unwrap().unwrap();
    assert!(matches!(inspect(directory).unwrap(), Recovery::Active));
    crate::resource_group::tests::release_while_child_retains_descriptor(|| {
        drop(lease);
        match inspect(directory).unwrap() {
            Recovery::Unresolved => (),
            Recovery::Active => panic!("finished session inspection retained its lock in an unrelated child"),
            _ => panic!("absent worker evidence was accepted as retirement"),
        }
    });
}
