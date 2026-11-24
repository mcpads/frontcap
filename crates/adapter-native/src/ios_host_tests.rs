use super::*;
#[test]
fn native_host_capture_and_input_readiness_do_not_wait_for_a_page() {
    let (fixture, mut config) = crate::simulator_app::tests::setup("normal");
    config.script = None;
    let root = fixture.root.path();
    let cancelled = AtomicBool::new(false);
    let deadline = Instant::now() + Duration::from_secs(10);
    let file = std::fs::File::create(root.join("pixel.png")).unwrap();
    let mut encoder = png::Encoder::new(file, 1, 1);
    encoder.set_color(png::ColorType::Rgb);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(&[0, 0, 0])
        .unwrap();
    let script=std::fs::read_to_string(&fixture.config.xcrun).unwrap().replace("elif args[1]=='bootstatus':pass", "elif args[1]=='io':pathlib.Path(args[-1]).write_bytes((p/'pixel.png').read_bytes())\nelif args[1]=='bootstatus':pass");
    std::fs::write(&fixture.config.xcrun, script).unwrap();
    // Device deletion must begin after the host closes its mounted native input lock.
    let script = std::fs::read_to_string(&fixture.config.xcrun).unwrap().replace(
        "elif args[1]=='delete':", "elif args[1]=='delete':\n    import fcntl\n    with (p/'Documents/frontcap-hid/requests.lock').open('r+') as lock:\n        fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)");
    std::fs::write(&fixture.config.xcrun, script).unwrap();
    let app = App::launch(
        crate::simulator_app::tests::acquire(&fixture),
        config,
        &root.join("app"),
        deadline,
        &cancelled,
    )
    .unwrap();
    let native = root.join("Documents/frontcap-hid");
    std::fs::create_dir_all(&native).unwrap();
    let endpoint = contacts::Endpoint {
        generation: "550e8400-e29b-41d4-a716-446655440000".into(),
        pid: app.process().pid,
        available: true,
        short_tap: false, observed_contacts: true,
        request_capacity: 24,
    };
    save_atomic(&native.join("endpoint.json"), &endpoint).unwrap();
    save_atomic(
        &native.join("window.json"),
        &contacts::Window {
            generation: endpoint.generation.clone(),
            pid: endpoint.pid,
            token: "window".into(),
            width: 1.0,
            height: 1.0,
            scale: 1.0,
            fullscreen: true,
        },
    )
    .unwrap();
    let mut target =
        Target::connect(app, &root.join("native-host"), true, deadline, &cancelled).unwrap();
    let capture = target
        .capture(RequestId::new("catalog").unwrap(), deadline, &cancelled)
        .unwrap();
    assert_eq!(capture.input.unwrap().expected_revision, 0);
    assert!(!root.join("Documents/clock-control.json").exists());
    assert!(crate::store::lock(&native).is_err(), "live input must retain exclusive ownership");
    assert!(target.stop().passed);
    assert!(crate::store::lock(&native).is_ok(), "Stop must release the device-data handle");
    assert!(
        target
            .capture(RequestId::new("after-stop").unwrap(), deadline, &cancelled)
            .is_err()
    );
}

#[test]
fn restart_preserves_consumer_data_and_replaces_input_and_page_authority() {
    struct Consumer {
        identity: frontcap_core::process::ProcessIdentity,
        reaper: Option<std::thread::JoinHandle<()>>,
    }
    impl Consumer {
        fn new() -> Self {
            let mut child = std::process::Command::new("/bin/sleep").arg("60").spawn().unwrap();
            let identity = frontcap_core::process::ProcessIdentity::of(child.id()).unwrap().unwrap();
            Self { identity, reaper: Some(std::thread::spawn(move || { child.wait().unwrap(); })) }
        }
    }
    impl Drop for Consumer {
        fn drop(&mut self) {
            if !self.identity.has_exited().unwrap() {
                unsafe { libc::kill(self.identity.pid as i32, libc::SIGTERM); }
            }
            self.reaper.take().unwrap().join().unwrap();
        }
    }
    for mode in ["normal", "reused-contact-generation", "foreign-registry"] {
        let first = Consumer::new();
        let second = Consumer::new();
        let (fixture, mut config) = crate::simulator_app::tests::setup("normal");
        config.page_registry = true;
        let root = fixture.root.path();
        std::fs::write(root.join("app-pid"), first.identity.pid.to_string()).unwrap();
        std::fs::write(root.join("next-pid"), second.identity.pid.to_string()).unwrap();
        std::fs::write(root.join("restart-mode"), mode).unwrap();
        let file = std::fs::File::create(root.join("pixel.png")).unwrap();
        let mut encoder = png::Encoder::new(file, 1, 1);
        encoder.set_color(png::ColorType::Rgb);
        encoder.write_header().unwrap().write_image_data(&[0, 0, 0]).unwrap();
        let script = std::fs::read_to_string(&fixture.config.xcrun).unwrap().replace(
            "    print(args[3]+': '+(p/'app-pid').read_text())", r#"    pid=int((p/'app-pid').read_text())
    replaced=(p/'terminated').exists()
    native=p/'Documents/frontcap-hid';native.mkdir(parents=True)
    generation='550e8400-e29b-41d4-a716-44665544000'+str(int(replaced and (p/'restart-mode').read_text()!='reused-contact-generation'))
    (native/'endpoint.json').write_text(json.dumps(dict(generation=generation,pid=pid,available=True,observed_contacts=True,request_capacity=24)))
    (native/'window.json').write_text(json.dumps(dict(generation=generation,pid=pid,token='window',width=1.0,height=1.0,scale=1.0,fullscreen=True)))
    (p/'Documents/webview-pages').mkdir()
    (p/'Documents/webview-pages/original-channel').write_text('new' if replaced else 'old')
    (p/'Documents/webview-pages.json').write_text(json.dumps(dict(pid=pid+int(replaced and (p/'restart-mode').read_text()=='foreign-registry'),revision=1,capacity_exceeded=False,pages=[])))
    print(args[3]+': '+str(pid))"#).replace(
            "elif args[1]=='bootstatus':pass", r#"elif args[1]=='io':pathlib.Path(args[-1]).write_bytes((p/'pixel.png').read_bytes())
elif args[1]=='terminate':
    import signal
    os.kill(int((p/'app-pid').read_text()),signal.SIGTERM)
    (p/'app-pid').write_text((p/'next-pid').read_text());(p/'terminated').touch()
elif args[1]=='bootstatus':pass"#);
        std::fs::write(&fixture.config.xcrun, script).unwrap();
        let end = Instant::now() + Duration::from_secs(20);
        let cancel = AtomicBool::new(false);
        let host_config = Config { lifetime_ms: 900_000, simulator: fixture.config.clone(), app: config.clone(), contacts: true, system_ui: None };
        std::fs::create_dir(root.join("target")).unwrap();
        let simulator = Simulator::acquire_at(fixture.config.clone(), &root.join("target/simulator"), end, &cancel, &root.join("host")).unwrap();
        let app = App::launch(simulator, config.clone(), &root.join("target/app"), end, &cancel).unwrap();
        let mut target = Target::connect(app, &root.join("target/host"), true, end, &cancel).unwrap();
        save_atomic(&root.join("launch.json"), &serde_json::json!({"host":true,"deadline_ns":1,"config":{
            "simulator":fixture.config,"app":config,"contacts":true,"lifecycle":null,"service_url_environment":null
        }})).unwrap();
        save_atomic(&root.join("host-ready.json"), &frontcap_core::native::ios_host::Ready {
            owner:target.owner().clone(),contacts:target.contact_endpoint().cloned(),pages:true,app_lifecycle:false,system_ui:None,
        }).unwrap();
        let old_id = RequestId::new("original").unwrap();
        let observe = frontcap_core::native::ios_host::Operation::Observe {input_basis:false};
        let old = crate::ios_host_session::execute(&mut target,root,&old_id,&observe,end,&cancel).unwrap();
        let previous = target.owner().clone();
        let old_endpoint = target.contact_endpoint().unwrap().clone();
        std::fs::write(root.join("Documents/saved-result.json"), b"consumer result").unwrap();
        let capture = target.capture(RequestId::new("before").unwrap(), end, &cancel).unwrap();
        let mut foreign = previous.clone();foreign.process.started.push_str("wrong");
        assert!(target.restart(&foreign, &root.join("rejected"), end, &cancel).is_err());
        assert!(!root.join("rejected").exists());
        assert!(!first.identity.has_exited().unwrap());
        let restart_id = RequestId::new("replacement").unwrap();
        let operation = frontcap_core::native::ios_host::Operation::Restart {previous:previous.clone()};
        std::fs::create_dir(root.join("host-epochs")).unwrap();
        let epoch = root.join("host-epochs/replacement");
        let answer = if mode == "normal" {
            let frontcap_core::native::ios_host::Completion::Restarted {ready,..} =
                crate::ios_host_session::execute(&mut target,root,&restart_id,&operation,end,&cancel).unwrap() else {panic!()};
            Ok(*ready)
        } else { target.restart(&previous, &epoch, end, &cancel) };
        assert!(first.identity.has_exited().unwrap());
        assert_eq!(std::fs::read(root.join("Documents/saved-result.json")).unwrap(), b"consumer result");
        assert_eq!(std::fs::read(epoch.join("app/previous-page-channels/original-channel")).unwrap(), b"old");
        assert_eq!(load::<contacts::Endpoint>(&epoch.join("app/previous-native-contacts/endpoint.json")).unwrap().unwrap(), old_endpoint);
        if mode == "normal" {
            let ready = answer.unwrap();
            assert_eq!(ready.owner.process, second.identity);
            assert!(ready.pages);
            assert_ne!(ready.contacts.unwrap().generation, old_endpoint.generation);
            assert!(target.input(RequestId::new("stale-input").unwrap(), capture.input.unwrap().observation,
                contacts::Operation::Set { expected_revision:0, contacts:vec![] }, end, &cancel).is_err());
            assert!(target.restart(&previous, &root.join("stale-restart"), end, &cancel).is_err());
            assert!(!root.join("stale-restart").exists());
            assert!(target.capture(RequestId::new("after").unwrap(), end, &cancel).unwrap().input.is_some());
            assert_eq!(target.discover_pages(end, &cancel).unwrap().pid, second.identity.pid);
            assert_eq!(crate::ios_host_session::execute(&mut target,root,&old_id,&observe,end,&cancel).unwrap(),old);
            let new_id = RequestId::new("new-capture").unwrap();
            crate::ios_host_session::execute(&mut target,root,&new_id,&observe,end,&cancel).unwrap();
            let restarted = crate::ios_host_session::read_result(root,&host_config,&restart_id,&operation).unwrap().unwrap();
            let saved = root.join("host-requests/replacement/result.json");
            std::fs::remove_file(&saved).unwrap();
            assert_eq!(serde_json::to_value(crate::ios_host_session::read_result(root,&host_config,&restart_id,&operation).unwrap().unwrap()).unwrap(),serde_json::to_value(restarted).unwrap());
            assert!(crate::ios_host_session::execute(&mut target,root,&restart_id,&operation,end,&cancel).is_ok());
            assert!(!saved.exists(), "lookup must not repair evidence");
            assert!(target.stop().passed);
            for id in [&old_id,&new_id] {
                assert!(crate::ios_host_session::read_result(root,&host_config,id,&observe).unwrap().is_some());
            }
            let publication=epoch.join("replacement-ready.json");let bytes=std::fs::read(&publication).unwrap();
            std::fs::remove_file(&publication).unwrap();
            assert!(crate::ios_host_session::read_result(root,&host_config,&restart_id,&operation).unwrap().is_none());
            assert!(crate::ios_host_session::read_result(root,&host_config,&new_id,&observe).unwrap().is_none());
            assert!(crate::ios_host_session::read_result(root,&host_config,&old_id,&observe).unwrap().is_some());
            std::fs::write(&publication,&bytes).unwrap();
            let request=root.join("host-requests/replacement/request.json");let original=std::fs::read(&request).unwrap();
            let mut cycle:serde_json::Value=serde_json::from_slice(&original).unwrap();cycle["epoch"]="replacement".into();save_atomic(&request,&cycle).unwrap();
            assert!(crate::ios_host_session::read_result(root,&host_config,&new_id,&observe).is_err());
            std::fs::write(&request,&original).unwrap();
            let container=epoch.join("app/container.json");let original=std::fs::read(&container).unwrap();
            save_atomic(&container,&root.join("different-container")).unwrap();
            assert!(crate::ios_host_session::read_result(root,&host_config,&new_id,&observe).is_err());
            std::fs::write(&container,&original).unwrap();
            std::fs::write(epoch.join("app/attachment.dylib"),b"changed").unwrap();
            assert!(crate::ios_host_session::read_result(root,&host_config,&restart_id,&operation).is_err());
        } else {
            assert!(answer.unwrap_err().retirement.unwrap().passed, "{mode}");
            assert!(!epoch.join("replacement-ready.json").exists());
            assert!(target.capture(RequestId::new("after-failure").unwrap(), end, &cancel).is_err());
        }
        assert!(target.stop().passed);
        let calls: Vec<Vec<String>> = std::fs::read_to_string(root.join("calls")).unwrap()
            .lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(calls.iter().filter(|args| args[1] == "install").count(), 1);
        assert_eq!(calls.iter().filter(|args| args[1] == "terminate").count(), 1);
        assert_eq!(calls.iter().filter(|args| args[1] == "launch").count(), 2);
    }
}

#[test]
fn exhausted_host_request_admission_prevents_capture_without_spending_retirement() {
    use frontcap_core::{ids::LaunchId, write_budget::{Class, Limits, WriteBudget}};
    const CHILD: &str = "FRONTCAP_IOS_HOST_ADMISSION_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let temporary = tempfile::tempdir().unwrap();
        let budget = WriteBudget::create(&temporary.path().join("budget"), LaunchId::generate(),
            Limits { ordinary: 32 * 1024 * 1024, retirement: 1024 * 1024 }).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "ios_host::tests::exhausted_host_request_admission_prevents_capture_without_spending_retirement", "--nocapture"])
            .env(CHILD, "1");
        WriteBudget::configure_child(&mut command, Some(&budget)).unwrap();
        assert!(command.status().unwrap().success());
        return;
    }
    let (fixture, mut config) = crate::simulator_app::tests::setup("normal");
    config.script = None;
    let directory = fixture.root.path().join("session");
    std::fs::create_dir_all(directory.join("target")).unwrap();
    let cancel = AtomicBool::new(false);
    let end = Instant::now() + Duration::from_secs(20);
    let app = App::launch(crate::simulator_app::tests::acquire(&fixture), config,
        &directory.join("target/app"), end, &cancel).unwrap();
    let mut target = Target::connect(app, &directory.join("target/host"), false, end, &cancel).unwrap();
    let budget = WriteBudget::from_environment().unwrap().unwrap();
    budget.charge(Class::Ordinary, 32 * 1024 * 1024 - budget.charged().unwrap().ordinary).unwrap();
    let before = budget.charged().unwrap();
    let sdk_calls = std::fs::read(fixture.root.path().join("calls")).unwrap();
    let error = crate::ios_host_session::execute(&mut target, &directory, &RequestId::new("capture").unwrap(),
        &frontcap_core::native::ios_host::Operation::Observe { input_basis: false }, end, &cancel).unwrap_err();
    assert!(error.to_string().contains("allowance"), "{error}");
    assert!(!directory.join("host-requests/capture/request.json").exists());
    assert!(!directory.join("host-requests/capture/result.json").exists());
    assert_eq!(budget.charged().unwrap(), before);
    assert_eq!(std::fs::read(fixture.root.path().join("calls")).unwrap(), sdk_calls);
    assert!(target.stop().passed);
}
