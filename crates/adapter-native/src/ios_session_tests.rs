use super::*;
use crate::{simulator::tests::Fixture, xctest::Xctest};
use std::os::unix::fs::PermissionsExt;
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(15)
}
fn wait_for(path: &Path) {
    let end = deadline();
    while !path.exists() {
        assert!(Instant::now() < end, "missing {}", path.display());
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn setup(f: &Fixture) -> IosLaunch {
    let root = f.root.path();
    let config = crate::xctest::tests::config(f);
    let device = root.join("ABCD0000-0000-0000-0000-000000000001");
    std::fs::create_dir_all(device.join("container/Documents/ui-control")).unwrap();
    let executable = device.join("SampleAppTests-Runner");
    std::fs::copy("/bin/sleep", &executable).unwrap();
    #[cfg(target_os = "macos")]
    assert!(
        Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&executable)
            .output()
            .unwrap()
            .status
            .success()
    );
    std::fs::write(&config.xcodebuild, r#"#!/usr/bin/env python3
import pathlib,subprocess,json,time
root=pathlib.Path(__file__).parent
device=root/'ABCD0000-0000-0000-0000-000000000001'
child=subprocess.Popen([str(device/'SampleAppTests-Runner'),'120'])
endpoint=device/'container/Documents/ui-control/ui-control.json'
endpoint.write_text(json.dumps(dict(generation='abcd0000-0000-0000-0000-000000000002',pid=child.pid,port=1234,status='ready',last_id=0,app_restart=(root/'restart-enabled').exists(),app_entry=(root/'entry-enabled').exists())))
(device/'container/Documents/clock-control.json').write_text('{}')
(root/'container-path').write_text(str(device/'container'))
try:
 while not (root/'retire-fake').exists():time.sleep(.01)
finally:child.terminate();child.wait()
"#).unwrap();
    let script = root.join("bridge.py");
    std::fs::write(&script,r#"import sys,json,pathlib,hashlib,time,argparse,uuid,errno
p=argparse.ArgumentParser();p.add_argument('--descriptor');p.add_argument('--directory');p.add_argument('--timeout',type=float);a=p.parse_args()
root=pathlib.Path(__file__).parent;client=pathlib.Path(a.directory);client.mkdir(exist_ok=True)
call=json.load(sys.stdin);identity=call['identity'];ui=client.name=='ui-client';channel='ui' if ui else 'clock'
if (root/'require-budget').exists():
 assert a.timeout is not None and 0 < a.timeout <= 15, a.timeout
 with (root/'budgets').open('a') as out:out.write(json.dumps(dict(channel=channel,call=call['call'],timeout=a.timeout))+'\n')
if (root/'unresolved-phase').exists() and (root/'unresolved-phase').read_text()==channel+':'+call['call']:
 print(json.dumps(dict(reply='unresolved',error='TimeoutError: retained acquisition cause')));sys.exit()
state=client/channel;state.mkdir(exist_ok=True);cursor=state/(channel+'-client.json')
if call['call']=='initialize':
 (client/'owner.json').write_text(json.dumps(dict(surface='uikit' if ui else None,identity=identity,source_sha256=call['source_sha256'],descriptor=str(pathlib.Path(a.descriptor).resolve()))))
 cursor.write_text(json.dumps(dict(generation=identity['generation'],next_id=1,pending=None)))
 print(json.dumps(dict(reply='ready',identity=identity,source_sha256=call['source_sha256'])));sys.exit()
assert call['call']=='execute'
r=call['request'];op=r['operation']
raw=dict(request=r,generation=identity['generation'],pid=identity['pid'],last_id=r['request_id'],status='ok',port=1234)
if ui:
 if op=='launch':
  app=dict(simulator=identity['simulator'],generation='abcd0000-0000-0000-0000-000000000003',pid=identity['pid']+1)
  raw['result']=dict(state='launched',reads=dict(endpoint=dict(generation=app['generation'],pid=app['pid'],port=1235)))
  outcome=dict(outcome='launched',app=app)
 else:
  assert op=='restart' and r['arguments']['app']==call['app']
  previous=call['app'];app=dict(previous,generation=str(uuid.uuid4()),pid=previous['pid']+1)
  with (root/'restart-effects').open('a') as out:out.write(json.dumps(r)+'\n')
  raw['result']=dict(state='restarted',retirement=dict(endpoint=dict(generation=previous['generation'],pid=previous['pid'],port=1235),
   process_absent=True,port_closed=True,process_errno=errno.ESRCH,port_errno=errno.ECONNREFUSED),
   reads=dict(endpoint=dict(generation=app['generation'],pid=app['pid'],port=1236),now_ns=0))
  if 'url' in r['arguments']:raw['result']['url']=r['arguments']['url']
  outcome=dict(outcome='restarted',previous=previous,app=app,process_absent=True,port_closed=True,now_ns=0)
 (client/'app.json').write_text(json.dumps(app))
else:
 with (root/'effects').open('a') as out:out.write(op+'\n')
 if (root/'block').exists():
  (root/'dispatched').write_text('yes');time.sleep(60)
 raw.update(before_ns=0,now_ns=r['delta_ns'],applied=op=='advance',advance_result=0 if op=='advance' else None,drain_result=0 if op=='advance' else None)
data=json.dumps(raw).encode();digest=hashlib.sha256(data).hexdigest();receipts=client/'receipts';receipts.mkdir(exist_ok=True);(receipts/(digest+'.json')).write_bytes(data)
cursor.write_text(json.dumps(dict(generation=identity['generation'],next_id=r['request_id']+1,pending=None)))
raw.pop('port');raw['receipt_sha256']=digest
if ui:raw.pop('result');raw.pop('status');raw['outcome']=outcome
if op=='restart':
 (client/'restarts').mkdir(exist_ok=True);(client/'restarts'/f"{r['request_id']}.json").write_text(json.dumps(raw))
if ui and (root/'forge-app').exists():raw['outcome']['app']['pid']+=1
if ui and (root/'late-launch').exists():
 with (root/'launch-effects').open('a') as out:out.write(json.dumps(raw)+'\n')
 time.sleep(a.timeout+.5)
print(json.dumps(dict(reply='completed',completion=raw)))
"#).unwrap();
    let python = Command::new("python3")
        .args(["-c", "import sys;print(sys.executable)"])
        .output()
        .unwrap();
    let wrapper = root.join("worker");
    std::fs::write(&wrapper, format!("#!/usr/bin/env python3\nimport os,sys\nos.environ['FRONTCAP_IOS_TEST_DIRECTORY']=sys.argv[1]\nexe={}\nos.execv(exe,[exe,'--exact','ios_session::tests::worker_entry','--nocapture'])\n", serde_json::to_string(&std::env::current_exe().unwrap()).unwrap())).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    IosLaunch {
        resource_lifetime_ms: None,
        app_environment: Default::default(), service_url_environment: None,
        surface: crate::ios_execution::UiSurface::Uikit,
        xctest: config,
        python: PathBuf::from(String::from_utf8(python.stdout).unwrap().trim()),
        ui_script: script.clone(),
        clock_script: script,
        controller_id: "abcd0000-0000-0000-0000-000000000004".into(),
    }
}
#[test]
fn worker_entry() {
    let Ok(directory) = std::env::var("FRONTCAP_IOS_TEST_DIRECTORY") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let exhaustion = directory.parent().unwrap().join("exhaust-before-worker-identity");
    if exhaustion.exists() {
        let limit: u64 = std::fs::read_to_string(exhaustion).unwrap().parse().unwrap();
        let budget = frontcap_core::write_budget::WriteBudget::from_environment().unwrap().unwrap();
        budget.charge(Class::Ordinary, limit - budget.charged().unwrap().ordinary).unwrap();
    }
    run_with(&directory, |config, path, deadline, cancel| {
        Target::acquire_with(
            config,
            path,
            deadline,
            cancel,
            |config, path, deadline, cancel| {
                Xctest::start_at(
                    config,
                    path,
                    deadline,
                    cancel,
                    &directory.parent().unwrap().join("host"),
                )
            },
        )
    })
    .unwrap();
}
fn start(f: &Fixture, config: IosLaunch) -> Session {
    Session::start(
        &f.root.path().join("worker"),
        config,
        &f.root.path().join("session"),
        deadline(),
    )
    .unwrap()
}
fn retired(f: &Fixture) -> SessionResult {
    let end = deadline();
    loop {
        if let Recovery::Finished { result, .. } = inspect(&f.root.path().join("session")).unwrap()
        {
            assert!(result.retirement.as_ref().unwrap().passed, "{result:?}");
            let host: crate::host::HostSession =
                load(&f.root.path().join("host/native-session.json"))
                    .unwrap()
                    .unwrap();
            assert!(host.retired);
            if let Some(identity) =
                load::<ProcessIdentity>(&f.root.path().join("session/target/runner-process.json"))
                    .unwrap()
            {
                assert!(identity.has_exited().unwrap());
            }
            assert!(f.root.path().join("deleted").exists());
            return result;
        }
        assert!(Instant::now() < end, "worker failed to retire");
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn serial_requests_cache_results_and_stop_retires_the_original_target() {
    let f = Fixture::new("normal");
    let config = setup(&f);
    let mut session = start(&f, config);
    let cancel = AtomicBool::new(false);
    let owner = session.wait_ready(deadline(), &cancel).unwrap();
    for _ in 0..2 {
        assert!(matches!(
            session
                .execute(
                    RequestId::new("read").unwrap(),
                    Operation::ReadClock,
                    deadline(),
                    &cancel
                )
                .unwrap(),
            Reply::Completed { .. }
        ));
    }
    assert_eq!(
        std::fs::read_to_string(f.root.path().join("effects")).unwrap(),
        "read\n"
    );
    let result = session.stop(deadline()).unwrap();
    assert!(matches!(
        result,
        Recovery::Finished {
            result: SessionResult { error: None, .. },
            ..
        }
    ));
    retired(&f);
    assert!(
        crate::ios_execution::Executor::read_completion(
            &f.root.path().join("session/target"),
            &owner,
            &RequestId::new("read").unwrap(),
            &Operation::ReadClock
        )
        .unwrap()
        .is_some()
    );
    assert!(
        run_with(&f.root.path().join("session"), |_, _, _, _| panic!(
            "must not reacquire"
        ))
        .is_err()
    );
}

#[test]
fn uikit_url_session_owns_delivery_caches_acceptance_and_retires_uncertainty() {
    use frontcap_core::native::external_entry::{Request, Target as EntryTarget};
    use crate::external_entry::{Reply as EntryReply, read_accepted};
    for lose_reply in [false, true] {
        let f = Fixture::new("normal");
        let mut config = setup(&f);
        config.surface = crate::ios_execution::UiSurface::Uikit;
        let bundle = config.xctest.app.bundle.clone();
        std::fs::write(f.root.path().join("uikit"), b"").unwrap();
        let app = f.root.path().join("Installed.app");
        std::fs::create_dir(&app).unwrap();
        plist::to_file_binary(app.join("Info.plist"), &serde_json::json!({
            "CFBundleIdentifier": bundle, "CFBundleURLTypes": [{"CFBundleURLSchemes": ["owned-client-test"]}]
        })).unwrap();
        std::fs::write(f.root.path().join("apps.plist"), format!(
            r#"{{ "{bundle}" = {{ CFBundleIdentifier = "{bundle}"; Path = "{}"; }}; }}"#, app.display())).unwrap();
        if lose_reply { std::fs::write(f.root.path().join("lose-url"), b"").unwrap(); }
        let mut session = start(&f, config);
        let cancel = AtomicBool::new(false);
        let owner = session.wait_ready(deadline(), &cancel).unwrap();
        let request = Request { target: EntryTarget::Ios {
            simulator: owner.app.simulator.to_ascii_lowercase(), bundle,
        }, generation: owner.app.generation.clone(), url: "owned-client-test://item/0?x=';literal#fragment".into() };
        let id = RequestId::new("entry").unwrap();
        let reply = session.dispatch_url(id.clone(), request.clone(), deadline(), &cancel).unwrap();
        let directory = f.root.path().join("session/target/external-entry");
        let effects = std::fs::read(f.root.path().join("url-effects")).unwrap();
        let args: Vec<String> = serde_json::from_slice(&effects).unwrap();
        assert_eq!(args, ["simctl", "openurl", "ABCD0000-0000-0000-0000-000000000001", &request.url]);
        if lose_reply {
            assert!(matches!(reply, EntryReply::Failed { .. }));
            assert!(read_accepted(&directory, &id, &request).unwrap().is_none());
            assert!(retired(&f).error.is_some());
        } else {
            let EntryReply::Accepted { acceptance } = reply else { panic!("{reply:?}") };
            assert_eq!(acceptance.request, request);
            assert!(directory.join("admission-entry.json").exists());
            assert!(matches!(session.execute(RequestId::new("read").unwrap(), Operation::ReadClock,
                deadline(), &cancel).unwrap(), Reply::Completed { .. }));
            let state = f.root.path().join("session/target/requests/requests.json");
            let before = std::fs::read(&state).unwrap();
            let EntryReply::Accepted { acceptance: cached } = session.dispatch_url(id.clone(), request.clone(),
                deadline(), &cancel).unwrap() else { panic!() };
            assert_eq!(cached, acceptance);
            assert_eq!(std::fs::read(state).unwrap(), before);
            // An ordinary request cannot steal the accepted URL ID.
            assert!(matches!(session.execute(id.clone(), Operation::ReadClock, deadline(), &cancel).unwrap(), Reply::Failed { .. }));
            assert!(retired(&f).error.is_some());
            assert_eq!(read_accepted(&directory, &id, &request).unwrap(), Some(acceptance));
        }
        assert_eq!(std::fs::read(f.root.path().join("url-effects")).unwrap(), effects);
        let journal = directory.join("requests.jsonl");
        let before = std::fs::read(&journal).unwrap();
        let _ = read_accepted(&directory, &id, &request).unwrap();
        assert_eq!(std::fs::read(journal).unwrap(), before);
    }
}
#[test]
fn controller_entry() {
    let Ok(root) = std::env::var("FRONTCAP_IOS_CONTROLLER_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let phase = std::env::var("FRONTCAP_IOS_CONTROLLER_PHASE").unwrap();
    let config = load(&root.join("config.json")).unwrap().unwrap();
    let mut session = Session::start(
        &root.join("worker"),
        config,
        &root.join("session"),
        deadline(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    if phase != "acquisition" {
        session.wait_ready(deadline(), &cancel).unwrap();
        if phase == "dispatch" {
            std::fs::write(root.join("block"), b"yes").unwrap();
            let _ = session.execute(
                RequestId::new("advance").unwrap(),
                Operation::AdvanceClock { delta_ns: 1000000 },
                deadline(),
                &cancel,
            );
            panic!("expected controller death");
        }
        std::fs::write(root.join("idle-ready"), b"yes").unwrap();
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
#[test]
fn controller_group_death_during_acquisition_idle_and_dispatch_preserves_cleanup_and_pending() {
    for phase in ["acquisition", "idle", "dispatch"] {
        let f = Fixture::new(if phase == "acquisition" {
            "slow-boot"
        } else {
            "normal"
        });
        let config = setup(&f);
        save_atomic(&f.root.path().join("config.json"), &config).unwrap();
        let mut controller = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ios_session::tests::controller_entry",
                "--nocapture",
            ])
            .env("FRONTCAP_IOS_CONTROLLER_ROOT", f.root.path())
            .env("FRONTCAP_IOS_CONTROLLER_PHASE", phase)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for(&f.root.path().join(match phase {
            "acquisition" => "boot-started",
            "idle" => "idle-ready",
            _ => "dispatched",
        }));
        assert_eq!(
            unsafe { libc::kill(-(controller.id() as i32), libc::SIGKILL) },
            0
        );
        controller.wait().unwrap();
        assert!(retired(&f).error.is_some());
        if phase == "dispatch" {
            let owner = load(&f.root.path().join("session/ready.json"))
                .unwrap()
                .unwrap();
            let store = crate::ios_store::RequestStore::open(
                &f.root.path().join("session/target/requests"),
                &owner,
            )
            .unwrap();
            assert!(matches!(
                store.recovery(&RequestId::new("advance").unwrap()).unwrap(),
                crate::ios_store::Recovery::Reconcile(_)
            ));
            assert_eq!(
                std::fs::read_to_string(f.root.path().join("effects")).unwrap(),
                "advance\n"
            );
        }
    }
}
#[test]
fn worker_death_is_unresolved_and_fences_replacement_without_adoption() {
    let f = Fixture::new("normal");
    let config = setup(&f);
    let mut session = start(&f, config.clone());
    session
        .wait_ready(deadline(), &AtomicBool::new(false))
        .unwrap();
    let runner: ProcessIdentity = load(&f.root.path().join("session/target/runner-process.json"))
        .unwrap()
        .unwrap();
    session.child.as_mut().unwrap().kill().unwrap();
    session.child.as_mut().unwrap().wait().unwrap();
    assert!(matches!(
        inspect(&f.root.path().join("session")).unwrap(),
        Recovery::Unresolved
    ));
    assert!(!runner.has_exited().unwrap());
    let mut replacement = Session::start(
        &f.root.path().join("worker"),
        config,
        &f.root.path().join("replacement"),
        deadline(),
    )
    .unwrap();
    assert!(
        replacement
            .wait_ready(deadline(), &AtomicBool::new(false))
            .unwrap_err()
            .to_string()
            .contains("unretired")
    );
    replacement.stop(deadline()).unwrap();
    // Fixture-owned shutdown protocol, not a signal to a saved PID.
    std::fs::write(f.root.path().join("retire-fake"), b"yes").unwrap();
    let end = deadline();
    while !runner.has_exited().unwrap() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(crate::host::acquire(&f.root.path().join("host")).is_err());
}

#[test]
fn forged_launch_receipt_and_ready_publication_failure_retire_acquired_target() {
    for mode in ["forge-app", "publication"] {
        let f = Fixture::new("normal");
        let config = setup(&f);
        if mode == "forge-app" {
            std::fs::write(f.root.path().join(mode), b"yes").unwrap();
        }
        let mut session = start(&f, config);
        if mode == "publication" {
            // Acquisition takes place before the worker creates its command directory.
            std::fs::create_dir(f.root.path().join("session/commands")).unwrap();
        }
        let error = session
            .wait_ready(deadline(), &AtomicBool::new(false))
            .unwrap_err();
        if mode == "forge-app" {
            assert!(
                error.to_string().contains("raw launched app differs"),
                "{error}"
            );
        }
        session.stop(deadline()).unwrap();
        assert!(retired(&f).error.is_some());
        assert!(!f.root.path().join("session/ready.json").exists());
    }
}

#[test]
fn initial_bridges_share_the_remaining_acquisition_budget() {
    let f = Fixture::new("normal");
    let config = setup(&f);
    std::fs::write(f.root.path().join("require-budget"), b"yes").unwrap();
    let mut session = start(&f, config);
    let ready = session.wait_ready(deadline(), &AtomicBool::new(false));
    // Retire even if the regression fails, so a failed assertion leaves no target.
    session.stop(deadline()).unwrap();
    assert!(ready.is_ok(), "{ready:?}");
    let rows: Vec<serde_json::Value> = std::fs::read_to_string(f.root.path().join("budgets"))
        .unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0]["channel"], "ui");
    assert_eq!(rows[0]["call"], "initialize");
    assert_eq!(rows[1]["call"], "execute");
    assert_eq!(rows[2]["channel"], "clock");
    for pair in rows.windows(2) {
        assert!(pair[1]["timeout"].as_f64().unwrap() < pair[0]["timeout"].as_f64().unwrap());
    }
}

#[test]
fn late_initial_launch_reply_fails_acquisition_and_retires_without_relaunch() {
    let f = Fixture::new("normal");
    let config = setup(&f);
    std::fs::write(f.root.path().join("late-launch"), b"yes").unwrap();
    let mut session = Session::start(
        &f.root.path().join("worker"), config, &f.root.path().join("session"),
        Instant::now() + Duration::from_secs(5),
    ).unwrap();
    let ready = session.wait_ready(deadline(), &AtomicBool::new(false));
    session.stop(deadline()).unwrap();
    assert!(ready.is_err(), "late acquisition became ready: {ready:?}");
    let result = retired(&f);
    assert!(result.error.is_some());
    assert!(!f.root.path().join("session/ready.json").exists());
    let effects = std::fs::read_to_string(f.root.path().join("launch-effects")).unwrap();
    assert_eq!(effects.lines().count(), 1);
    let target = f.root.path().join("session/target");
    assert!(!target.join("request-owner.json").exists());
    let exchange: serde_json::Value = load(&target.join("ui-launch/exchange.json")).unwrap().unwrap();
    assert!(exchange["failure"].as_str().unwrap().contains("deadline"), "{exchange}");
    assert!(exchange["reply"].is_null());
    assert!(exchange["cleanup_failure"].is_null());
}

#[test]
fn initial_unresolved_bridge_reply_preserves_cause_through_retirement() {
    for phase in ["ui:initialize", "ui:execute", "clock:initialize"] {
        let f = Fixture::new("normal");
        let config = setup(&f);
        std::fs::write(f.root.path().join("unresolved-phase"), phase).unwrap();
        let mut session = start(&f, config);
        let error = session.wait_ready(deadline(), &AtomicBool::new(false)).unwrap_err();
        session.stop(deadline()).unwrap();
        let result = retired(&f);
        assert!(error.to_string().contains("TimeoutError: retained acquisition cause"), "{phase}: {error}");
        assert!(result.error.unwrap().contains("TimeoutError: retained acquisition cause"));
        assert!(!f.root.path().join("session/ready.json").exists());
    }
}

#[test]
fn uikit_replacement_preserves_old_epochs_and_resets_only_the_app_clock_sequence() {
    replacement_history(None, false);
}
#[test]
fn uikit_cold_entry_admits_before_retirement_and_recovers_the_original_url() {
    replacement_history(Some("another-app://note/1"), true);
    replacement_history(Some("client-test://item/1"), false);
}
fn replacement_history(url: Option<&str>, denied: bool) {
    let f=Fixture::new("normal");let mut config=setup(&f);let root=f.root.path();
    config.surface=crate::ios_execution::UiSurface::Uikit;
    std::fs::write(root.join("uikit"),b"").unwrap();std::fs::write(root.join("restart-enabled"),b"").unwrap();
    let app=root.join("ABCD0000-0000-0000-0000-000000000001/Installed.app");std::fs::create_dir(&app).unwrap();
    std::fs::write(app.join("fixture"),b"unchanged installed product").unwrap();
    let xcrun=std::fs::read_to_string(&f.config.xcrun).unwrap().replace("pointer=p/'container-path'",
        "pointer=p/('app-container-path' if args[-1]=='app' else 'container-path')");
    std::fs::write(&f.config.xcrun,xcrun).unwrap();std::fs::write(root.join("app-container-path"),app.to_str().unwrap()).unwrap();
    let bundle=config.xctest.app.bundle.clone();
    if url.is_some() {
        std::fs::write(root.join("entry-enabled"),b"").unwrap();
        std::fs::write(app.join("Info.plist"),format!(r#"<?xml version="1.0"?><plist version="1.0"><dict><key>CFBundleIdentifier</key><string>{bundle}</string><key>CFBundleURLTypes</key><array><dict><key>CFBundleURLSchemes</key><array><string>client-test</string></array></dict></array></dict></plist>"#)).unwrap();
        std::fs::write(root.join("apps.plist"),format!(r#"{{ "{bundle}" = {{ CFBundleIdentifier = "{bundle}"; Path = "{}"; }}; }}"#,app.display())).unwrap();
    }
    let mut session=start(&f,config);let cancel=AtomicBool::new(false);
    let initial=session.wait_ready(deadline(),&cancel).unwrap();assert!(session.restart_available().unwrap());
    assert_eq!(session.entry_available().unwrap(),url.is_some());
    if denied {
        let reply=session.restart(RequestId::new("unowned-scheme").unwrap(),initial.app.generation.clone(),url.map(str::to_owned),deadline(),&cancel).unwrap();
        assert!(matches!(reply,crate::ios_restarts::Reply::Failed {..}));
        assert!(!root.join("restart-effects").exists());
        assert_eq!(session.current_epoch().unwrap().0,initial);
        assert!(!root.join("session/target/restarts/unowned-scheme").exists());
        session.stop(deadline()).unwrap();assert!(retired(&f).error.is_some());
        return;
    }
    let old_id=RequestId::new("old-clock").unwrap();
    assert!(matches!(session.execute(old_id.clone(),Operation::AdvanceClock {delta_ns:100},deadline(),&cancel).unwrap(),Reply::Completed {..}));
    let mut previous=initial.clone();let target=root.join("session/target");let mut results=vec![];
    for number in 1..=2 {
        let id=RequestId::new(format!("restart-{number}")).unwrap();
        let reply=session.restart(id.clone(),previous.app.generation.clone(),url.map(str::to_owned),deadline(),&cancel).unwrap();
        let crate::ios_restarts::Reply::Completed {replacement}=reply else {panic!("{reply:?}")};
        assert_eq!(replacement.entry.as_ref().map(|e|e.request.url.as_str()),url);
        if let Some(url)=url {
            assert!(target.join("restarts").join(id.as_str()).join("admission.json").exists());
            assert!(crate::ios_restarts::read_completion(&target,&id,&previous.app.generation,Some(&format!("{url}0"))).is_err());
        }
        assert_eq!(replacement.previous,previous);assert_eq!(replacement.owner.runner,initial.runner);
        assert_ne!(replacement.owner.app,previous.app);assert_eq!(session.current_epoch().unwrap().0,replacement.owner);
        let state=std::fs::read(target.join("restarts/history.json")).unwrap();
        assert_eq!(crate::ios_restarts::read_completion(&target,&id,&previous.app.generation,url).unwrap(),Some(*replacement.clone()));
        assert_eq!(std::fs::read(target.join("restarts/history.json")).unwrap(),state);
        assert!(session.validate_restart(&RequestId::new("stale").unwrap(),&previous.app.generation,url).is_err());
        let Reply::Completed {completion:Completion::Clock(clock)}=session.execute(RequestId::new(format!("new-clock-{number}")).unwrap(),Operation::ReadClock,deadline(),&cancel).unwrap() else {panic!()};
        assert_eq!(clock.request.request_id,1);assert_eq!(clock.now_ns,0);assert_eq!(clock.generation,replacement.owner.app.generation);
        previous=replacement.owner.clone();results.push(replacement);
    }
    assert_eq!(results[0].installation,results[1].installation);
    assert_eq!(load::<Owner>(&target.join("request-owner.json")).unwrap(),Some(initial.clone()));
    assert!(crate::ios_execution::Executor::read_completion(&target,&initial,&old_id,&Operation::AdvanceClock {delta_ns:100}).unwrap().is_some());
    session.stop(deadline()).unwrap();assert!(retired(&f).error.is_none());
    assert_eq!(std::fs::read_to_string(root.join("restart-effects")).unwrap().lines().count(),2);
    assert_eq!(crate::ios_restarts::epochs(&target).unwrap().len(),3);
    assert_eq!(crate::ios_restarts::read_completion(&target,&RequestId::new("restart-1").unwrap(),&initial.app.generation,url).unwrap(),Some(*results[0].clone()));
    // Model owner loss after the verified bridge/installation publication but before history commit.
    let path=target.join("restarts/history.json");
    let mut pending:serde_json::Value=load(&path).unwrap().unwrap();pending["entries"][1]["completion"]=serde_json::Value::Null;
    save_atomic(&path,&pending).unwrap();let original=std::fs::read(&path).unwrap();
    let second=RequestId::new("restart-2").unwrap();
    if url.is_some() {
        let admission=target.join("restarts/restart-2/admission.json");let bytes=std::fs::read(&admission).unwrap();
        std::fs::write(&admission,b"changed admission").unwrap();
        assert!(crate::ios_restarts::read_completion(&target,&second,&results[0].owner.app.generation,url).is_err());
        std::fs::write(&admission,bytes).unwrap();
        let epoch=target.join("epochs").join(&results[0].owner.app.generation);
        let original=&results[1].completion;
        let receipt=epoch.join("ui-client/receipts").join(format!("{}.json",original.receipt_sha256));
        let mut raw:serde_json::Value=load(&receipt).unwrap().unwrap();raw["result"]["url"]=serde_json::json!("client-test://item/2");
        let bytes=serde_json::to_vec(&raw).unwrap();let digest=frontcap_core::canonical::sha256_bytes_hex(&bytes);
        std::fs::write(epoch.join("ui-client/receipts").join(format!("{digest}.json")),bytes).unwrap();
        let completion_path=epoch.join("ui-client/restarts").join(format!("{}.json",original.request.request_id));
        let mut changed=original.clone();changed.receipt_sha256=digest;save_atomic(&completion_path,&changed).unwrap();
        assert!(crate::ios_restarts::read_completion(&target,&second,&results[0].owner.app.generation,url).is_err());
        save_atomic(&completion_path,original).unwrap();
    }
    assert_eq!(crate::ios_restarts::read_completion(&target,&second,&results[0].owner.app.generation,url).unwrap(),Some(*results[1].clone()));
    assert_eq!(crate::ios_restarts::epochs(&target).unwrap().len(),2);
    assert_eq!(std::fs::read(&path).unwrap(),original);
    std::fs::remove_file(target.join("restarts/restart-2/installation-after.json")).unwrap();
    assert!(crate::ios_restarts::read_completion(&target,&second,&results[0].owner.app.generation,url).unwrap().is_none());
    assert_eq!(std::fs::read(&path).unwrap(),original);
}

#[test]
fn expired_initial_dispatch_reaps_real_worker_without_claiming_target_retirement() {
    let fixture = Fixture::new("normal");
    let config = setup(&fixture);
    let directory = fixture.root.path().join("session");
    let startup = Session::start_with_budget_outcome(&fixture.root.path().join("worker"), config,
        &directory, Instant::now(), None);
    let error = startup.result.err().expect("expired initial dispatch must fail");
    assert!(error.to_string().contains("native session deadline exceeded"), "{error}");
    assert!(matches!(startup.cleanup, StartupCleanup::AcquisitionNotStarted { .. }));
    let Recovery::Finished { worker, result } = inspect(&directory).unwrap() else {
        panic!("the original worker must be retired before startup returns");
    };
    assert!(worker.has_exited().unwrap());
    assert!(result.acquisition_not_started);
    assert!(result.error.unwrap().contains("session not authorized before acquisition deadline"));
    assert!(result.retirement.is_none());
    assert!(!directory.join("target").exists());
    assert!(!fixture.root.path().join("host/native-session.json").exists());
}

#[test]
fn acquisition_observer_runs_before_start_and_refusal_reaps_original_worker() {
    let fixture = Fixture::new("normal");let config = setup(&fixture);
    let directory = fixture.root.path().join("session");let mut called = false;
    let startup = Session::start_with_observer(&fixture.root.path().join("worker"), config,
        &directory, deadline(), None, Some(&mut |session| {
            called = true;
            assert!(!directory.join("target").exists());
            assert!(!session.worker_identity()?.has_exited()?);
            Err(io::Error::other("acquisition watch refused"))
        }));
    assert!(called);
    assert!(startup.result.err().unwrap().to_string().contains("acquisition watch refused"));
    assert!(matches!(startup.cleanup, StartupCleanup::AcquisitionNotStarted { .. }));
    let Recovery::Finished { worker, result } = inspect(&directory).unwrap() else { panic!() };
    assert!(worker.has_exited().unwrap());assert!(result.acquisition_not_started);
    assert!(result.retirement.is_none());assert!(!directory.join("target").exists());
}


#[test]
fn worker_metadata_refusal_prevents_effects_but_preserves_terminal_retirement() {
    use frontcap_core::{ids::LaunchId, write_budget::{Limits, WriteBudget}};
    let fixture = Fixture::new("normal");let config = setup(&fixture);
    let directory = fixture.root.path().join("session");
    let limits = Limits { ordinary: 1024 * 1024, retirement: 1024 * 1024 };
    let budget = WriteBudget::create(&fixture.root.path().join("budget"), LaunchId::generate(), limits).unwrap();
    let mut session = Session::start_with_budget(&fixture.root.path().join("worker"), config,
        &directory, deadline(), Some(&budget)).unwrap();
    session.wait_ready(deadline(), &AtomicBool::new(false)).unwrap();
    let prior = std::fs::read(fixture.root.path().join("effects")).unwrap_or_default();
    let remaining = limits.ordinary - budget.charged().unwrap().ordinary;
    budget.charge(Class::Ordinary, remaining).unwrap();
    let retired_before = budget.charged().unwrap().retirement;
    assert!(session.execute(RequestId::new("refused").unwrap(), Operation::AdvanceClock { delta_ns: 100 },
        deadline(), &AtomicBool::new(false)).is_err());
    let Recovery::Finished { worker, result } = session.stop(deadline()).unwrap() else { panic!() };
    assert!(worker.has_exited().unwrap());assert!(result.retirement.unwrap().passed);
    assert!(result.error.unwrap().contains("write allowance exhausted"));
    assert_eq!(std::fs::read(fixture.root.path().join("effects")).unwrap_or_default(), prior);
    assert!(!directory.join("commands/1/command.json").exists());
    assert!(!directory.join("commands/1/result.json").exists());
    assert_eq!(budget.charged().unwrap().ordinary, limits.ordinary);
    assert!(budget.charged().unwrap().retirement > retired_before);
    retired(&fixture);
}


#[test]
fn exhausted_ordinary_admission_preserves_initial_owner_and_pre_start_cleanup() {
    use frontcap_core::{ids::LaunchId, write_budget::{Limits, WriteBudget}};
    let fixture = Fixture::new("normal");let config = setup(&fixture);
    let directory = fixture.root.path().join("session");
    let limits = Limits { ordinary: 1024 * 1024, retirement: 1024 * 1024 };
    let budget = WriteBudget::create(&fixture.root.path().join("budget"), LaunchId::generate(), limits).unwrap();
    std::fs::write(fixture.root.path().join("exhaust-before-worker-identity"), limits.ordinary.to_string()).unwrap();
    let mut observed = false;
    let startup = Session::start_with_observer(&fixture.root.path().join("worker"), config,
        &directory, deadline(), Some(&budget), Some(&mut |session| {
            observed = true;
            assert!(!session.worker_identity()?.has_exited()?);
            assert_eq!(budget.charged()?.ordinary, limits.ordinary);
            assert!(!directory.join("target").exists());
            budget.charge(Class::Ordinary, 1)
        }));
    assert!(observed, "initial identity must survive ordinary exhaustion");
    assert!(startup.result.err().unwrap().to_string().contains("write allowance exhausted"));
    let StartupCleanup::AcquisitionNotStarted { worker } = startup.cleanup else { panic!() };
    assert!(worker.has_exited().unwrap());
    let Recovery::Finished { result, .. } = inspect(&directory).unwrap() else { panic!() };
    assert!(result.acquisition_not_started && result.retirement.is_none());
    assert!(!directory.join("target").exists());
    assert_eq!(budget.charged().unwrap().ordinary, limits.ordinary);
    assert!(budget.charged().unwrap().retirement >=
        std::fs::metadata(directory.join("worker.json")).unwrap().len()
        + std::fs::metadata(directory.join("result.json")).unwrap().len());
}

#[test]
fn insufficient_retirement_allowance_cannot_bypass_initial_identity_admission() {
    use frontcap_core::{ids::LaunchId, write_budget::{Limits, WriteBudget}};
    let fixture = Fixture::new("normal");let config = setup(&fixture);
    let directory = fixture.root.path().join("session");
    let budget = WriteBudget::create(&fixture.root.path().join("budget"), LaunchId::generate(),
        Limits { ordinary: 1024 * 1024, retirement: 1 }).unwrap();
    let mut observed = false;
    let startup = Session::start_with_observer(&fixture.root.path().join("worker"), config,
        &directory, deadline(), Some(&budget), Some(&mut |_| { observed = true; Ok(()) }));
    assert!(startup.result.is_err());assert!(!observed);
    assert!(matches!(startup.cleanup, StartupCleanup::Unproven));
    assert!(!directory.join("worker.json").exists());assert!(!directory.join("target").exists());
    assert_eq!(budget.charged().unwrap().retirement, 0);
}

#[test]
fn simulator_data_volume_requires_explicit_bounded_resource_ownership() {
    let fixture = Fixture::new("normal");
    let mut launch = setup(&fixture);
    launch.xctest.simulator.data_volume = Some(crate::storage_volume::Config {
        python: "/unused/python".into(), worker: "/unused/volume-worker".into(), size_mib: 8192, reserve_mib: 8,
    });
    assert!(launch.validate_resources().is_err());
    for lifetime in [0, 900_001] {
        launch.resource_lifetime_ms = Some(lifetime);
        assert!(launch.validate_resources().is_err());
    }
    launch.resource_lifetime_ms = Some(60_000);
    launch.validate_resources().unwrap();
    let target = fixture.root.path().join("must-not-start");
    let error = Target::acquire(launch, &target, deadline(), &AtomicBool::new(false)).err().unwrap();
    assert!(error.cause.contains("independent session worker"));
    assert!(!target.exists());
}

#[test]
fn independent_group_retirement_preserves_lost_and_failed_worker_results() {
    use crate::resource_group::{self, Group};
    let fixture = Fixture::new("normal");
    let root = fixture.root.path().canonicalize().unwrap();
    let mut config = setup(&fixture);
    config.resource_lifetime_ms = Some(30_000);
    let arguments = |entry: &str| vec!["-c".into(),
        format!("export FRONTCAP_RESOURCE_GROUP_DIRECTORY=\"$2\"; exec \"$1\" --exact resource_group::tests::{entry} --nocapture"),
        "uikit-retirement".into(), std::env::current_exe().unwrap().to_str().unwrap().into()];
    let group_config = resource_group::Config { host: root.join("host"), target: root.join("target"),
        evidence_root: Some(root.clone()), artifact_budget_bytes: None, temporary_storage_root: None, scratch_volume: None, scratch_volume_root: None,
        resource_executable: "/bin/sh".into(), resource_arguments: arguments("resource_entry"), ports: vec![],
        deadline_ns: monotonic_ns().unwrap() + 30_000_000_000 };
    File::create_new(root.join("session.lock")).unwrap();
    save_atomic(&root.join("launch.json"), &Launch { config: config.clone(), deadline_ns: group_config.deadline_ns }).unwrap();
    let mut command = Command::new("/bin/sh"); command.args(arguments("group_entry"));
    let mut group = Group::start(&mut command, &group_config, &root.join("target-resources"), deadline()).unwrap();
    let resource = group.start_resource(RequestId::new("runner").unwrap(), Command::new("/bin/sleep").arg("300"),
        deadline(), &AtomicBool::new(false)).unwrap();
    let mut worker = Command::new("/bin/sleep").arg("300").spawn().unwrap();
    save_atomic(&root.join("worker.json"), &ProcessIdentity::of(worker.id()).unwrap().unwrap()).unwrap();
    assert!(matches!(inspect(&root).unwrap(), Recovery::Unresolved));
    worker.kill().unwrap(); worker.wait().unwrap();
    assert!(matches!(inspect(&root).unwrap(), Recovery::ResourceCleanup { .. }));
    assert!(group.stop(deadline()).unwrap().passed);
    assert!(resource.has_exited().unwrap());
    let original = std::fs::read(root.join("target-resources/retirement.json")).unwrap();
    assert!(matches!(inspect(&root).unwrap(), Recovery::WorkerLost { retirement, .. } if retirement.passed));
    assert!(!root.join("result.json").exists());
    let failure = SessionResult { error: Some("worker lost reply".into()),
        retirement: Some(Retirement { passed: false, errors: vec!["group pending".into()], signal_error: None }),
        acquisition_not_started: false };
    save_atomic(&root.join("result.json"), &failure).unwrap();
    let failed_bytes = std::fs::read(root.join("result.json")).unwrap();
    assert!(matches!(inspect(&root).unwrap(), Recovery::RetirementRecovered { result, retirement, .. }
        if retirement.passed && result.error == failure.error && !result.retirement.as_ref().unwrap().passed));
    assert_eq!(std::fs::read(root.join("result.json")).unwrap(), failed_bytes);
    assert_eq!(std::fs::read(root.join("target-resources/retirement.json")).unwrap(), original);
    // Only the explicitly selected group route may recover; old direct sessions stay unresolved.
    std::fs::remove_file(root.join("result.json")).unwrap();
    config.resource_lifetime_ms = None;
    save_atomic(&root.join("launch.json"), &Launch { config: config.clone(), deadline_ns: group_config.deadline_ns }).unwrap();
    assert!(matches!(inspect(&root).unwrap(), Recovery::Unresolved));
    config.resource_lifetime_ms = Some(30_000);
    save_atomic(&root.join("launch.json"), &Launch { config: config.clone(), deadline_ns: group_config.deadline_ns }).unwrap();
    let mut different = config.xctest.clone(); different.simulator.source_sha256 = "b".repeat(64);
    save_atomic(&root.join("target/launch.json"), &different).unwrap();
    assert!(inspect(&root).is_err());
    save_atomic(&root.join("target/launch.json"), &config.xctest).unwrap();
    assert!(inspect(&root).unwrap().retirement().unwrap().passed);
    // A corrupt passing group receipt must not substitute for original cleanup proof.
    std::fs::write(root.join("target-resources/cleanup.json"), b"{}").unwrap();
    assert!(inspect(&root).is_err());
}
