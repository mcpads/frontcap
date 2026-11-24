use super::*;
use frontcap_core::state::save_atomic;
use crate::simulator::tests::Fixture;
use std::time::Duration;
pub(crate) fn setup(mode: &str) -> (Fixture, Launch) {
    let fixture = Fixture::new("normal");
    let path = fixture.config.xcrun.clone();
    let script = std::fs::read_to_string(&path).unwrap().replace("import json,sys,pathlib,time", "import json,sys,pathlib,time,os").replace("elif args[1]=='bootstatus':pass", r#"elif args[1]=='install':
    if (p/'app-mode').read_text()=='install-failure':sys.exit(6)
elif args[1]=='launch':
    (p/'environment.json').write_text(json.dumps({k:v for k,v in os.environ.items() if k.startswith('SIMCTL_CHILD_')}))
    print(args[3]+': '+(p/'app-pid').read_text())
elif args[1]=='bootstatus':pass"#);
    std::fs::write(path, script).unwrap();
    let root = fixture.root.path();
    std::fs::write(root.join("app-mode"), mode).unwrap();
    std::fs::write(root.join("app-pid"), std::process::id().to_string()).unwrap();
    std::fs::write(root.join("container-path"), root.to_str().unwrap()).unwrap();
    let app = root.join("Consumer.app");
    std::fs::create_dir(&app).unwrap();
    let library = root.join("injection.dylib");
    std::fs::write(&library, b"frozen library").unwrap();
    let script = root.join("injection.js");
    std::fs::write(&script, b"frozen script").unwrap();
    let config = Launch {
        app,
        bundle: "dev.frontcap.consumer".into(),
        environment: BTreeMap::from([("CONTENT_URL".into(), "http://127.0.0.1:1234".into())]),
        library: AttachmentFile {
            path: library,
            sha256: sha256_bytes_hex(b"frozen library"),
        },
        script: Some(AttachmentFile {
            path: script,
            sha256: sha256_bytes_hex(b"frozen script"),
        }),
        mute_audio: true,
        page_registry: false,
        prepared: None,
        initial_data: None,
    };
    (fixture, config)
}
pub(crate) fn acquire(f: &Fixture) -> Simulator {
    Simulator::acquire_at(
        f.config.clone(),
        &f.root.path().join("simulator"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
        &f.root.path().join("host"),
    )
    .unwrap()
}
fn with_initial_data(f: &Fixture, config: &mut Launch) {
    let root = f.root.path();
    let source = root.join("seed");
    std::fs::create_dir_all(source.join("Documents")).unwrap();
    std::fs::write(source.join("Documents/state.json"), b"initial state").unwrap();
    let product = crate::products::BundleInventory::capture(&config.app).unwrap();
    let recipe = crate::app_data::Recipe { app_id:config.bundle.clone(),
        product_sha256:frontcap_core::canonical::sha256_hex(&frontcap_core::canonical::canonical_of(&product)),
        inventory:crate::products::BundleInventory::capture(&source).unwrap() };
    let prepared = root.join("prepared-data");
    recipe.construct(&source, &prepared, "abcd0000-0000-0000-0000-000000000099",
        &recipe.app_id, &recipe.product_sha256).unwrap();
    config.initial_data = Some(prepared);
    std::fs::write(root.join("apps.plist"), "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict/></plist>").unwrap();
    let script = std::fs::read_to_string(&f.config.xcrun).unwrap()
        .replace("print(pointer.read_text())", "print(str(p/'Consumer.app') if args[-1]=='app' else pointer.read_text())")
        .replace("    print(args[3]+': '+(p/'app-pid').read_text())",
            "    (p/'launch-data').write_bytes((p/'Documents/state.json').read_bytes())\n    print(args[3]+': '+(p/'app-pid').read_text())");
    std::fs::write(&f.config.xcrun, script).unwrap();
}

#[test]
fn initial_data_is_visible_before_first_launch_and_incompatible_products_never_install() {
    for changed in [false, true] {
        let (f, mut config) = setup("normal");
        with_initial_data(&f, &mut config);
        if changed { std::fs::write(config.app.join("new-product-file"), b"different product").unwrap(); }
        let result = App::launch(acquire(&f), config, &f.root.path().join("app"),
            Instant::now()+Duration::from_secs(10), &AtomicBool::new(false));
        if changed {
            assert!(result.err().unwrap().retirement.unwrap().passed);
            assert!(!std::fs::read_to_string(f.root.path().join("calls")).unwrap().contains("\"install\""));
            assert!(!f.root.path().join("Documents/state.json").exists());
        } else {
            let mut app = result.unwrap();
            assert_eq!(std::fs::read(f.root.path().join("launch-data")).unwrap(), b"initial state");
            assert!(app.launch_config.initial_data.is_none());
            assert!(f.root.path().join("app/initial-data-applied.json").exists());
            assert!(app.stop().passed);
        }
    }
}

#[test]
fn existing_installation_or_destination_data_blocks_initial_launch_without_overwrite() {
    for installed in [false, true] {
        let (f, mut config) = setup("normal");
        with_initial_data(&f, &mut config);
        if installed {
            std::fs::write(f.root.path().join("apps.plist"), format!(
                "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict><key>{}</key><dict/></dict></plist>", config.bundle)).unwrap();
        } else {
            std::fs::create_dir(f.root.path().join("Documents")).unwrap();
            std::fs::write(f.root.path().join("Documents/state.json"), b"existing data").unwrap();
        }
        let result = App::launch(acquire(&f), config, &f.root.path().join("app"),
            Instant::now()+Duration::from_secs(10), &AtomicBool::new(false));
        assert!(result.err().unwrap().retirement.unwrap().passed);
        assert!(!f.root.path().join("launch-data").exists());
        assert!(!f.root.path().join("app/initial-data-applied.json").exists());
        if !installed { assert_eq!(std::fs::read(f.root.path().join("Documents/state.json")).unwrap(), b"existing data"); }
    }
}

#[test]
fn launched_app_keeps_simulator_owner_and_frozen_attachment_until_retirement() {
    let (f, config) = setup("normal");
    let simulator = acquire(&f);
    let mut app = App::launch(
        simulator,
        config.clone(),
        &f.root.path().join("app"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(app.process.pid, std::process::id());
    app.ensure_running(
        Instant::now() + Duration::from_secs(5),
        &AtomicBool::new(false),
    )
    .unwrap();
    let env: BTreeMap<String, String> =
        serde_json::from_slice(&std::fs::read(f.root.path().join("environment.json")).unwrap())
            .unwrap();
    let frozen = PathBuf::from(&env["SIMCTL_CHILD_FRONTCAP_WEBVIEW_SCRIPT"]);
    std::fs::write(config.script.unwrap().path, b"changed after launch").unwrap();
    assert_eq!(std::fs::read(frozen).unwrap(), b"frozen script");
    assert_eq!(env["SIMCTL_CHILD_CONTENT_URL"], "http://127.0.0.1:1234");
    assert_eq!(env["SIMCTL_CHILD_FRONTCAP_MUTE_AUDIO"], "1");
    assert!(app.stop().passed);
    assert!(app.stop().passed);
    assert!(
        app.ensure_running(
            Instant::now() + Duration::from_secs(5),
            &AtomicBool::new(false)
        )
        .is_err()
    );
    let calls = std::fs::read_to_string(f.root.path().join("calls")).unwrap();
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.contains("\"delete\""))
            .count(),
        1
    );
}
#[test]
fn attachment_mismatch_and_install_failure_retire_only_the_owned_device() {
    for mode in ["bad-digest", "install-failure", "reserved-environment"] {
        let (f, mut config) = setup(mode);
        let simulator = acquire(&f);
        if mode == "bad-digest" {
            config.library.sha256 = "0".repeat(64);
        }
        if mode == "reserved-environment" {
            config
                .environment
                .insert("DYLD_INSERT_LIBRARIES".into(), "foreign".into());
        }
        let error = match App::launch(
            simulator,
            config,
            &f.root.path().join("app"),
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
        ) {
            Err(e) => e,
            Ok(_) => panic!("invalid launch accepted"),
        };
        assert!(error.retirement.unwrap().passed);
        assert!(f.root.path().join("deleted").exists());
        assert!(!f.root.path().join("environment.json").exists());
    }
}

#[test]
fn occupied_evidence_directory_is_preserved_while_new_device_is_retired() {
    let (f, config) = setup("normal");
    let simulator = acquire(&f);
    let directory = f.root.path().join("app");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(directory.join("retirement.json"), b"old evidence").unwrap();
    let error = match App::launch(
        simulator,
        config,
        &directory,
        Instant::now() + Duration::from_secs(5),
        &AtomicBool::new(false),
    ) {
        Err(error) => error,
        Ok(_) => panic!("reused evidence directory"),
    };
    assert!(error.retirement.unwrap().passed);
    assert_eq!(
        std::fs::read(directory.join("retirement.json")).unwrap(),
        b"old evidence"
    );
    assert!(!directory.join("failure.json").exists());
}

#[test]
fn xctest_attachment_preserves_app_identity_and_retires_transferred_ownership() {
    for mode in [
        "ready",
        "detach-publication-failure",
        "changed-products",
        "wrong-consumer",
        "publication-failure",
        "replaced-process",
        "invalid-readiness",
    ] {
        let (f, config) = setup("normal");
        let mut runner = crate::xctest::tests::config(&f);
        runner.app.bundle = config.bundle.clone();
        crate::xctest::tests::prepare_ready_runner(&f, &runner);
        // The fake simctl must keep app and runner containers distinct, as the real command does.
        let script = std::fs::read_to_string(&f.config.xcrun).unwrap().replace(
            "pointer=p/'container-path'",
            "pointer=p/('runner-container-path' if args[3].endswith('.xctrunner') else 'container-path')",
        );
        std::fs::write(&f.config.xcrun, script).unwrap();
        let script = std::fs::read_to_string(&runner.xcodebuild)
            .unwrap()
            .replace("root/'container-path'", "root/'runner-container-path'");
        std::fs::write(&runner.xcodebuild, script).unwrap();
        let mut app = App::launch(
            acquire(&f),
            config,
            &f.root.path().join("app"),
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
        )
        .unwrap();
        let process = app.process().clone();
        let container = app.container().to_owned();
        let simulator = app.simulator_id().to_owned();
        match mode {
            "changed-products" => {
                std::fs::write(runner.products.join("SampleApp.xctestrun"), "changed").unwrap()
            }
            "wrong-consumer" => runner.app.bundle = "another.consumer".into(),
            "replaced-process" => app.process.started = "another process incarnation".into(),
            "invalid-readiness" => {
                let script = std::fs::read_to_string(&runner.xcodebuild)
                    .unwrap()
                    .replace("'status':'ready'", "'status':'ok'");
                std::fs::write(&runner.xcodebuild, script).unwrap();
            }

            "publication-failure" => {
                std::fs::create_dir(app.directory.join("xctest-ready.json")).unwrap()
            }
            _ => (),
        }
        let ready = app.attach_xctest(
            runner.into(),
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
        );
        if matches!(mode, "ready" | "detach-publication-failure") {
            let ready = ready.unwrap();
            assert_eq!(ready.identity.simulator, simulator);
            assert_eq!(app.process(), &process);
            assert_eq!(app.container(), container);
            app.ensure_running(
                Instant::now() + Duration::from_secs(5),
                &AtomicBool::new(false),
            )
            .unwrap();
            let marker = container.join("persisted-user-data");
            std::fs::write(&marker, b"unchanged consumer bytes").unwrap();
            if mode == "detach-publication-failure" {
                std::fs::create_dir(app.directory.join("xctest/simulator-transfer.json")).unwrap();
                assert!(app.detach_xctest().unwrap_err().retirement.unwrap().passed);
                assert!(app.owner.is_none());
            } else {
                app.detach_xctest().unwrap();
                assert!(ready.process.has_exited().unwrap());
                assert!(!ready.descriptor.exists());
                assert!(app.directory.join("xctest/transfer-runner-documents/ui-control/ui-control.json").exists());
                assert_eq!(app.process(), &process);
                assert_eq!(app.container(), container);
                assert_eq!(app.simulator_id(), simulator);
                assert_eq!(std::fs::read(&marker).unwrap(), b"unchanged consumer bytes");
                assert!(!f.root.path().join("deleted").exists());
                // The same host lease stays held after the runner relinquishes it.
                assert!(Simulator::acquire_at(f.config.clone(), &f.root.path().join("contender"),
                    Instant::now()+Duration::from_secs(1), &AtomicBool::new(false),
                    &f.root.path().join("host")).is_err());
                app.ensure_running(Instant::now()+Duration::from_secs(5), &AtomicBool::new(false)).unwrap();
            }
            assert!(app.stop().passed);
            assert!(ready.process.has_exited().unwrap());
        } else {
            assert!(ready.unwrap_err().retirement.unwrap().passed);
            assert!(
                app.ensure_running(
                    Instant::now() + Duration::from_secs(1),
                    &AtomicBool::new(false)
                )
                .is_err()
            );
        }
        assert!(app.stop().passed);
        if let Some(ready) = frontcap_core::state::load::<crate::xctest::UiReady>(
            &app.directory.join("xctest/ui-ready.json"),
        )
        .unwrap()
        {
            assert!(ready.process.has_exited().unwrap());
        }
        assert!(!process.has_exited().unwrap()); // Unrelated fixture test process is never killed.
        let calls = std::fs::read_to_string(f.root.path().join("calls")).unwrap();
        let calls: Vec<Vec<String>> = calls
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        for operation in ["create", "launch", "delete"] {
            assert_eq!(
                calls.iter().filter(|args| args[1] == operation).count(),
                1,
                "{mode}: {operation}"
            );
        }
    }
}

#[test]
fn same_install_restart_preserves_data_and_retires_uncertain_replacement_without_retry() {
    // Real disposable processes with a reaper; simctl remains a command substitute.
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
            if !self.identity.has_exited().unwrap() { unsafe { libc::kill(self.identity.pid as i32, libc::SIGTERM); } }
            self.reaper.take().unwrap().join().unwrap();
        }
    }
    for mode in ["success", "changed-attachment", "lost-terminate", "lost-launch", "changed-container"] {
        let first = Consumer::new();
        let second = Consumer::new();
        let (f, mut config) = setup("normal");
        if mode == "success" { with_initial_data(&f, &mut config); }
        std::fs::write(f.root.path().join("app-pid"), first.identity.pid.to_string()).unwrap();
        let script = std::fs::read_to_string(&f.config.xcrun).unwrap().replace(
            "elif args[1]=='bootstatus':pass", r#"elif args[1]=='terminate':
    import signal
    os.kill(int((p/'app-pid').read_text()),signal.SIGTERM)
    (p/'app-pid').write_text((p/'next-pid').read_text())
    if (p/'restart-mode').read_text()=='changed-container':
        other=p/'other-container';other.mkdir();(p/'container-path').write_text(str(other))
    if (p/'restart-mode').read_text()=='lost-terminate':sys.exit(6)
elif args[1]=='bootstatus':pass"#).replace(
            "    print(args[3]+': '+(p/'app-pid').read_text())",
            "    print(args[3]+': '+(p/'app-pid').read_text())\n    if (p/'restart-mode').exists() and (p/'restart-mode').read_text()=='lost-launch':sys.exit(7)");
        std::fs::write(&f.config.xcrun, script).unwrap();
        let mut app = App::launch(acquire(&f), config.clone(), &f.root.path().join("app"),
            Instant::now()+Duration::from_secs(10), &AtomicBool::new(false)).unwrap();
        if mode == "success" {
            std::fs::write(app.container().join("Documents/state.json"), b"user edit after launch").unwrap();
            std::fs::remove_dir_all(config.initial_data.as_ref().unwrap()).unwrap();
        }
        let original_container = app.container().to_owned();
        let marker = original_container.join("consumer-save");
        std::fs::write(&marker, b"persisted round result").unwrap();
        let transport = original_container.join("Documents/frontcap-hid");
        std::fs::create_dir_all(&transport).unwrap();
        let endpoint = serde_json::json!({"generation":"abcd0000-0000-0000-0000-000000000002","pid":first.identity.pid,"available":true,"observed_contacts":true,"request_capacity":4096});
        save_atomic(&transport.join("endpoint.json"), &endpoint).unwrap();
        std::fs::write(transport.join("request-1.json"), b"prior transport command").unwrap();
        // Restart uses the frozen copies, never changed source attachment inputs.
        std::fs::write(&config.library.path, b"changed source library").unwrap();
        if mode == "changed-attachment" { std::fs::write(&app.launch_config.library.path, b"corrupted frozen library").unwrap(); }
        std::fs::write(f.root.path().join("restart-mode"), mode).unwrap();
        std::fs::write(f.root.path().join("next-pid"), second.identity.pid.to_string()).unwrap();
        let epoch = f.root.path().join("replacement");
        let outcome = app.restart(&epoch, Instant::now()+Duration::from_secs(10), &AtomicBool::new(false));
        if mode == "success" {
            outcome.unwrap();
            assert_eq!(app.process(), &second.identity);
            assert_eq!(std::fs::read(original_container.join("Documents/state.json")).unwrap(), b"user edit after launch");
            assert_eq!(std::fs::read(f.root.path().join("launch-data")).unwrap(), b"user edit after launch");
            assert!(first.identity.has_exited().unwrap());
            assert_eq!(app.container(), original_container);
            assert_eq!(std::fs::read(&marker).unwrap(), b"persisted round result");
            assert_eq!(std::fs::read(epoch.join("attachment.dylib")).unwrap(), b"frozen library");
            assert!(!f.root.path().join("deleted").exists());
            assert!(epoch.join("restarted.json").exists());
            assert!(!transport.exists());
            assert_eq!(std::fs::read(epoch.join("previous-native-contacts/request-1.json")).unwrap(), b"prior transport command");
        } else {
            assert!(outcome.unwrap_err().retirement.unwrap().passed, "{mode}");
            assert!(!epoch.join("restarted.json").exists());
            assert!(epoch.join("failure.json").exists());
        }
        assert!(app.stop().passed);
        let calls: Vec<Vec<String>> = std::fs::read_to_string(f.root.path().join("calls")).unwrap()
            .lines().map(|s|serde_json::from_str(s).unwrap()).collect();
        let count=|op|calls.iter().filter(|args|args[1]==op).count();
        assert_eq!(count("install"),1, "{mode}: never reinstall");
        assert_eq!(count("terminate"),usize::from(mode!="changed-attachment"));
        assert_eq!(count("launch"),if matches!(mode,"changed-attachment"|"lost-terminate") {1} else {2});
        assert_eq!(count("delete"),1);
    }
}

#[test]
fn native_only_attachment_does_not_require_or_enable_a_page_script() {
    let (f,mut config)=setup("normal");config.script=None;
    let mut app=App::launch(acquire(&f),config,&f.root.path().join("app"),Instant::now()+Duration::from_secs(10),&AtomicBool::new(false)).unwrap();
    let env:BTreeMap<String,String>=serde_json::from_slice(&std::fs::read(f.root.path().join("environment.json")).unwrap()).unwrap();
    assert!(!env.contains_key("SIMCTL_CHILD_FRONTCAP_WEBVIEW_SCRIPT"));
    assert!(!env.contains_key("SIMCTL_CHILD_FRONTCAP_WEBVIEW_CONTROL"));
    assert!(!f.root.path().join("app/attachment.js").exists());
    assert!(app.begin_entry("local-consumer://history", &f.root.path().join("entry"),
        Instant::now()+Duration::from_secs(1), &AtomicBool::new(false)).is_err());
    assert!(!f.root.path().join("entry").exists());
    assert!(app.stop().passed);
}

#[test]
fn attachment_budget_refuses_publication_and_preserves_original_bytes() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let (fixture, config) = setup("normal");
    let root = fixture.root.path();
    let bytes = std::fs::read(&config.library.path).unwrap();
    let budget = WriteBudget::create(&root.join("budget"), LaunchId::generate(),
        Limits { ordinary: bytes.len() as u64, retirement: 1024 }).unwrap();
    let first = root.join("first.dylib");
    config.library.freeze_with_budget(&first, Some(&budget)).unwrap();
    let refused = root.join("refused.dylib");
    assert!(config.library.freeze_with_budget(&refused, Some(&budget)).is_err());
    assert!(!refused.exists());
    assert_eq!(std::fs::read(&first).unwrap(), bytes);
    assert_eq!(std::fs::read(&config.library.path).unwrap(), bytes);
    assert_eq!(budget.charged().unwrap(), Limits { ordinary: bytes.len() as u64, retirement: 0 });
}

#[test]
fn ordinary_initial_data_allows_only_verified_library_execute_bit_removal() {
    use std::os::unix::fs::PermissionsExt;
    for change in ["library-mode", "library-bytes", "app-mode", "extra-file"] {
        let (f, mut config) = setup("normal");
        for (name, kind) in [("library.dylib", 6u32), ("Consumer", 2u32)] {
            let mut header = vec![0xcf, 0xfa, 0xed, 0xfe];
            header.extend_from_slice(&[0; 8]); header.extend_from_slice(&kind.to_le_bytes());
            std::fs::write(config.app.join(name), header).unwrap();
            std::fs::set_permissions(config.app.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        with_initial_data(&f, &mut config);
        let installed = f.root.path().join("Installed.app");
        crate::products::BundleInventory::capture(&config.app).unwrap().copy_verified(&config.app, &installed).unwrap();
        std::fs::set_permissions(installed.join("library.dylib"), std::fs::Permissions::from_mode(0o644)).unwrap();
        match change {
            "library-bytes" => std::fs::write(installed.join("library.dylib"), b"changed").unwrap(),
            "app-mode" => std::fs::set_permissions(installed.join("Consumer"), std::fs::Permissions::from_mode(0o644)).unwrap(),
            "extra-file" => std::fs::write(installed.join("extra"), b"undeclared").unwrap(),
            _ => (),
        }
        let script = std::fs::read_to_string(&f.config.xcrun).unwrap().replace("p/'Consumer.app'", "p/'Installed.app'");
        std::fs::write(&f.config.xcrun, script).unwrap();
        let result = App::launch(acquire(&f), config, &f.root.path().join("app"),
            Instant::now()+Duration::from_secs(10), &AtomicBool::new(false));
        if change == "library-mode" {
            let mut app = result.unwrap();
            assert_eq!(std::fs::read(f.root.path().join("launch-data")).unwrap(), b"initial state");
            assert!(f.root.path().join("app/initial-data-installed-product.json").exists());
            assert!(app.stop().passed);
        } else {
            assert!(result.err().unwrap().retirement.unwrap().passed);
            assert!(!f.root.path().join("launch-data").exists());
            assert!(!f.root.path().join("app/initial-data-applied.json").exists());
        }
    }
}
