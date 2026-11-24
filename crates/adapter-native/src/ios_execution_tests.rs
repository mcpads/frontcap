use super::*;
use frontcap_core::native::{ios::ClockIdentity, ios_ui::UiIdentity};
use std::time::Duration;
fn fixture() -> (tempfile::TempDir, Owner, Config) {
    let root = tempfile::tempdir().unwrap();
    let p = root.path();
    let owner = Owner {
        runner: UiIdentity {
            simulator: "abcd0000-0000-0000-0000-000000000001".into(),
            generation: "abcd0000-0000-0000-0000-000000000002".into(),
            pid: 123,
        },
        app: ClockIdentity {
            simulator: "abcd0000-0000-0000-0000-000000000001".into(),
            generation: "abcd0000-0000-0000-0000-000000000003".into(),
            pid: 124,
        },
        controller_id: "abcd0000-0000-0000-0000-000000000004".into(),
        source_sha256: "a".repeat(64),
    };
    let script = p.join("bridge.py");
    std::fs::write(&script,r#"import sys,json,pathlib,hashlib
call=json.load(sys.stdin);root=pathlib.Path(__file__).parent
with (root/'calls').open('a') as f:f.write(call['call']+'\n')
if call['call']=='recover':
 if not (root/'saved.json').exists():print(json.dumps(dict(reply='unresolved',error='no saved result')));sys.exit()
 result=json.loads((root/'saved.json').read_text())
else:
 request=call['request'];r=dict(request=request,generation=request['generation'],pid=124,last_id=request['request_id'],status='ok',before_ns=0,now_ns=0,applied=False,advance_result=None,drain_result=None,port=12345)
 if request['operation']=='advance':r.update(status='failed',advance_result=45)
 raw=json.dumps(r,indent=2).encode();digest=hashlib.sha256(raw).hexdigest();folder=root/'clock-client/receipts';folder.mkdir(exist_ok=True);(folder/(digest+'.json')).write_bytes(raw)
 r.pop('port');r['receipt_sha256']=digest;result=dict(reply='completed',completion=r);(root/'saved.json').write_text(json.dumps(result))
 if (root/'forge').exists():result['completion']['before_ns']=1;result['completion']['now_ns']=1
print(json.dumps(result))
"#).unwrap();
    let python = std::process::Command::new("python3")
        .args(["-c", "import sys;print(sys.executable)"])
        .output()
        .unwrap();
    assert!(python.status.success());
    let config = Config {
        system_alerts: false,
        surface: UiSurface::Uikit,
        python: PathBuf::from(String::from_utf8(python.stdout).unwrap().trim()),
        ui_script: script.clone(),
        clock_script: script,
        ui_descriptor: p.join("ui.json"),
        clock_descriptor: p.join("clock.json"),
    };
    for (client, identity, generation, next, descriptor) in [
        (
            "ui-client",
            serde_json::to_value(&owner.runner).unwrap(),
            &owner.runner.generation,
            2,
            &config.ui_descriptor,
        ),
        (
            "clock-client",
            serde_json::to_value(&owner.app).unwrap(),
            &owner.app.generation,
            1,
            &config.clock_descriptor,
        ),
    ] {
        let channel = if client == "ui-client" { "ui" } else { "clock" };
        std::fs::create_dir_all(p.join(client).join(channel)).unwrap();
        std::fs::write(descriptor, b"{}").unwrap();
        let mut binding = serde_json::json!({"identity":identity,"source_sha256":owner.source_sha256,"descriptor":descriptor.canonicalize().unwrap()});
        if client == "ui-client" { binding["surface"] = "uikit".into(); }
        save_atomic(&p.join(client).join("owner.json"), &binding).unwrap();
        save_atomic(
            &p.join(client)
                .join(channel)
                .join(format!("{channel}-client.json")),
            &serde_json::json!({"generation":generation,"next_id":next,"pending":null}),
        )
        .unwrap();
    }
    save_atomic(&p.join("ui-client/app.json"), &owner.app).unwrap();
    (root, owner, config)
}
fn id() -> RequestId {
    RequestId::new("read").unwrap()
}
fn execute(executor: &mut Executor) -> Result<Completion, Error> {
    executor.execute(
        id(),
        Operation::ReadClock,
        Instant::now() + Duration::from_secs(5),
        &AtomicBool::new(false),
    )
}

#[test]
fn system_alert_input_requires_fresh_evidence_and_recovers_without_redelivery() {
    use frontcap_core::native::ios_ui::UiOutcome;
    for rejected in [false, true] {
        let (root, owner, mut config) = fixture();
        let path = root.path();
        config.surface = UiSurface::Uikit;
        let ui_owner = path.join("ui-client/owner.json");
        let mut saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&ui_owner).unwrap()).unwrap();
        saved["surface"] = "uikit".into();
        save_atomic(&ui_owner, &saved).unwrap();
        save_atomic(&config.ui_descriptor, &serde_json::json!({"generation":owner.runner.generation,
            "pid":123,"port":12346,"last_id":1,"status":"ready"})).unwrap();
        // System evidence must work without any readable app-clock descriptor.
        std::fs::write(&config.clock_descriptor, b"unavailable app clock").unwrap();
        let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools").canonicalize().unwrap();
        std::fs::write(&config.ui_script, format!("import sys\nsys.path.insert(0,{:?})\n{}", tools.to_str().unwrap(),
            include_str!("../tests/fixtures/ios_system_bridge.py"))).unwrap();
        let mut catalog: serde_json::Value = serde_json::from_slice(include_bytes!("../../core/tests/fixtures/ios-system-ui.json")).unwrap();
        save_atomic(&path.join("catalog.json"), &catalog).unwrap();
        let key = |name| RequestId::new(name).unwrap();
        let deadline = || Instant::now() + Duration::from_secs(10);
        let cancelled = AtomicBool::new(false);
        config.system_alerts = true;
        let mut executor = Executor::create(path, owner.clone(), config.clone()).unwrap();
        executor.config.system_alerts = false;
        assert!(executor.execute(key("unsupported"), Operation::ObserveSystemAlerts, deadline(), &cancelled).is_err());
        assert!(!path.join("deliveries").exists());
        executor.config.system_alerts = true;
        let observed = executor.execute(key("before"), Operation::ObserveSystemAlerts, deadline(), &cancelled).unwrap();
        let input = Operation::TapSystemButton { observation: catalog["observation"].as_str().unwrap().into(),
            reference: catalog["alerts"][0]["buttons"][1]["reference"].as_str().unwrap().into() };
        executor.invalidate_for_external_entry(&key("url")).unwrap();
        drop(executor);
        let mut executor = Executor::open(path, owner.clone(), config.clone()).unwrap();
        assert!(executor.execute(key("stale"), input.clone(), deadline(), &cancelled).is_err());
        assert_eq!(Executor::read_completion(path, &owner, &key("before"), &Operation::ObserveSystemAlerts).unwrap(), Some(observed));
        // A new observation is necessary after an external action, including after reopen.
        executor.execute(key("fresh"), Operation::ObserveSystemAlerts, deadline(), &cancelled).unwrap();
        if rejected { std::fs::write(path.join("reject"), b"yes").unwrap(); }
        std::fs::write(path.join("lose-reply"), b"yes").unwrap();
        assert!(executor.execute(key("tap"), input.clone(), deadline(), &cancelled).is_err());
        assert!(executor.execute(key("new-input"), input.clone(), deadline(), &cancelled).is_err());
        drop(executor);
        let mut executor = Executor::open(path, owner.clone(), config).unwrap();
        let completion = executor.execute(key("tap"), input.clone(), deadline(), &cancelled).unwrap();
        assert!(matches!(&completion, Completion::Ui(c) if match &c.outcome {
            UiOutcome::SystemButtonNotApplied { .. } => rejected,
            UiOutcome::SystemButtonTapped { .. } => !rejected,
            _ => false,
        }));
        assert!(executor.execute(key("consumed"), input.clone(), deadline(), &cancelled).is_err());
        assert_eq!(std::fs::read_to_string(path.join("deliveries")).unwrap(),
            "observe_system_alerts\nobserve_system_alerts\ntap_system_button\n");
        // Cached input lookup must neither deliver again nor consume a later observation.
        let fresh = "abcd0000-0000-0000-0000-000000000009";
        catalog["observation"] = fresh.into();
        for (i, button) in catalog["alerts"][0]["buttons"].as_array_mut().unwrap().iter_mut().enumerate() {
            button["reference"] = format!("{fresh}-{i}").into();
        }
        save_atomic(&path.join("catalog.json"), &catalog).unwrap();
        executor.execute(key("later"), Operation::ObserveSystemAlerts, deadline(), &cancelled).unwrap();
        std::fs::remove_file(path.join("ui.json")).unwrap();
        assert_eq!(executor.execute(key("tap"), input.clone(), deadline(), &cancelled).unwrap(), completion);
        assert_eq!(Executor::read_completion(path, &owner, &key("tap"), &input).unwrap(), Some(completion));
        RequestStore::validate_ui_input(&path.join("requests"), &owner, &key("next"),
            &Operation::TapSystemButton { reference: format!("{fresh}-1"), observation: fresh.into() }).unwrap();
        assert_eq!(std::fs::read_to_string(path.join("deliveries")).unwrap().lines().count(), 4);
        // Retained capture bytes are verified again on read-only lookup.
        std::fs::write(path.join(format!("ui-client/observations/{fresh}/screen.png")), b"changed").unwrap();
        assert!(Executor::read_completion(path, &owner, &key("later"), &Operation::ObserveSystemAlerts).is_err());
    }
}

#[test]
fn composite_press_recovers_each_bridge_boundary_without_second_delivery() {
    for (fault,rejected) in [("reserve_ui",false),("before_ui",false),("press_target",false),("recover",false),("release_ui",true)] {
        let (root,owner,mut config)=fixture();let path=root.path();
        config.surface=UiSurface::Uikit;
        let ui_owner=path.join("ui-client/owner.json");
        let mut saved:serde_json::Value=serde_json::from_slice(&std::fs::read(&ui_owner).unwrap()).unwrap();
        saved["surface"]="uikit".into();save_atomic(&ui_owner,&saved).unwrap();
        for (descriptor,identity,port,last) in [(&config.ui_descriptor,serde_json::to_value(&owner.runner).unwrap(),12346,1),
            (&config.clock_descriptor,serde_json::to_value(&owner.app).unwrap(),12345,0)] {
            save_atomic(descriptor,&serde_json::json!({"generation":identity["generation"],"pid":identity["pid"],
                "port":port,"last_id":last,"status":"ready"})).unwrap();
        }
        let tools=Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools").canonicalize().unwrap();
        std::fs::write(&config.ui_script,format!("import sys\nsys.path.insert(0,{:?})\n{}",tools.to_str().unwrap(),
            include_str!("../tests/fixtures/ios_press_bridge.py"))).unwrap();
        std::fs::write(path.join("press.json"),include_bytes!("../../core/tests/fixtures/uikit-press.json")).unwrap();
        if rejected { std::fs::write(path.join("reject"),b"yes").unwrap(); }
        let mut executor=Executor::create(path,owner.clone(),config.clone()).unwrap();
        let deadline=|| Instant::now()+Duration::from_secs(10);
        let cancelled=AtomicBool::new(false);
        executor.execute(RequestId::new("observe").unwrap(),Operation::ObserveTargets,deadline(),&cancelled).unwrap();
        let observation="abcd0000-0000-0000-0000-000000000009".to_owned();
        let op=Operation::PressTarget { reference:format!("{observation}-0"),observation,delta_ns:70_000_000 };
        let key=RequestId::new("press").unwrap();
        std::fs::write(path.join("fault"),fault).unwrap();
        assert!(executor.execute(key.clone(),op.clone(),deadline(),&cancelled).is_err(),"{fault}");
        drop(executor);
        let mut executor=Executor::open(path,owner.clone(),config.clone()).unwrap();
        if fault=="before_ui" {
            assert!(executor.execute(key.clone(),op,deadline(),&cancelled).is_err());
            assert!(matches!(executor.store.recovery(&key).unwrap(),Recovery::Reconcile(Request::Press { .. })));
            assert_eq!(std::fs::read_to_string(path.join("deliveries")).unwrap(),"observe_targets\n");
            assert!(executor.execute(RequestId::new("later").unwrap(),Operation::ReadClock,deadline(),&cancelled).is_err());
            continue;
        }
        let completion=executor.execute(key.clone(),op.clone(),deadline(),&cancelled).unwrap();
        assert!(matches!(&completion,Completion::Press { clock:frontcap_core::native::ios_request::PressClock::Released { .. },.. })==rejected);
        assert_eq!(std::fs::read_to_string(path.join("deliveries")).unwrap(),"observe_targets\npress_target\n");
        std::fs::remove_file(&config.ui_descriptor).unwrap();std::fs::remove_file(&config.clock_descriptor).unwrap();
        assert_eq!(executor.execute(key.clone(),op.clone(),deadline(),&cancelled).unwrap(),completion);
        assert_eq!(Executor::read_completion(path,&owner,&key,&op).unwrap(),Some(completion.clone()));
        assert_eq!(std::fs::read_to_string(path.join("deliveries")).unwrap(),"observe_targets\npress_target\n");
        if let Completion::Press { clock:frontcap_core::native::ios_request::PressClock::Released { ui_receipt_sha256,.. },.. }=completion {
            std::fs::write(path.join("clock-client/releases").join(format!("{ui_receipt_sha256}.json")),b"{}").unwrap();
            assert!(Executor::read_completion(path,&owner,&key,&op).is_err());
        }
    }
}
#[test]
fn committed_results_survive_endpoint_loss_and_corrupt_receipts_are_not_returned() {
    let (root, owner, config) = fixture();
    let p = root.path();
    let mut executor = Executor::create(p, owner.clone(), config.clone()).unwrap();
    let completion = execute(&mut executor).unwrap();
    assert_eq!(execute(&mut executor).unwrap(), completion);
    assert_eq!(
        std::fs::read_to_string(p.join("calls")).unwrap(),
        "execute\n"
    );
    drop(executor);
    std::fs::remove_file(&config.ui_descriptor).unwrap();
    std::fs::remove_file(&config.clock_descriptor).unwrap();
    let mut executor = Executor::open(p, owner.clone(), config).unwrap();
    assert_eq!(execute(&mut executor).unwrap(), completion);
    assert_eq!(
        Executor::read_completion(p, &owner, &id(), &Operation::ReadClock).unwrap(),
        Some(completion.clone())
    );
    let Completion::Clock(c) = completion else {
        panic!()
    };
    let path = p
        .join("clock-client/receipts")
        .join(format!("{}.json", c.receipt_sha256));
    std::fs::write(path, b"changed").unwrap();
    assert!(Executor::read_completion(p, &owner, &id(), &Operation::ReadClock).is_err());
    assert!(execute(&mut executor).is_err());
    assert_eq!(
        std::fs::read_to_string(p.join("calls")).unwrap(),
        "execute\n"
    );
}
#[test]
fn a_dispatched_store_without_bridge_effect_never_authorizes_a_late_dispatch() {
    let (root, owner, config) = fixture();
    let mut executor = Executor::create(root.path(), owner.clone(), config.clone()).unwrap();
    executor.store.prepare(id(), Operation::ReadClock).unwrap();
    executor.store.dispatch(&id()).unwrap();
    drop(executor);
    let mut executor = Executor::open(root.path(), owner, config).unwrap();
    assert!(execute(&mut executor).is_err());
    assert_eq!(
        std::fs::read_to_string(root.path().join("calls")).unwrap(),
        "recover\n"
    );
    assert!(matches!(
        executor.store.recovery(&id()).unwrap(),
        Recovery::Reconcile(_)
    ));
}
#[test]
fn mismatched_projection_stays_pending_until_saved_raw_result_is_reconciled() {
    let (root, owner, config) = fixture();
    std::fs::write(root.path().join("forge"), b"yes").unwrap();
    let mut executor = Executor::create(root.path(), owner, config).unwrap();
    assert!(execute(&mut executor).is_err());
    assert!(matches!(
        executor.store.recovery(&id()).unwrap(),
        Recovery::Reconcile(_)
    ));
    let completion = execute(&mut executor).unwrap();
    let Completion::Clock(c) = completion else {
        panic!()
    };
    assert_eq!(c.now_ns, 0);
    assert_eq!(
        std::fs::read_to_string(root.path().join("calls")).unwrap(),
        "execute\nrecover\n"
    );
}

#[test]
fn terminal_native_clock_failure_is_verified_and_consumes_the_stored_sequence() {
    let (root, owner, config) = fixture();
    let mut executor = Executor::create(root.path(), owner, config).unwrap();
    let failed = executor
        .execute(
            RequestId::new("advance").unwrap(),
            Operation::AdvanceClock { delta_ns: 1 },
            Instant::now() + Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap();
    let Completion::Clock(failed) = failed else {
        panic!()
    };
    assert_eq!(
        failed.status,
        frontcap_core::native::ios::ClockStatus::Failed
    );
    assert!(!failed.applied);
    let Completion::Clock(read) = execute(&mut executor).unwrap() else {
        panic!()
    };
    assert_eq!(read.request.request_id, 2);
}
