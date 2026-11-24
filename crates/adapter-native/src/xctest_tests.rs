use super::*;
use crate::{products::tests::fixture, simulator::tests::Fixture};
use std::{os::unix::fs::PermissionsExt, time::Duration};
pub(crate) fn config(f: &Fixture) -> XctestLaunch {
    let products = f.root.path().join("build");
    fixture(&products);
    let runner = f.root.path().join("xcodebuild");
    std::fs::write(
        &runner,
        "#!/usr/bin/env python3\nimport time\ntime.sleep(60)\n",
    )
    .unwrap();
    std::fs::set_permissions(&runner, std::fs::Permissions::from_mode(0o700)).unwrap();
    XctestLaunch {
        initial_data: None,
        bootstrap_app: None,
        app: AppProfile::default(),
        simulator: f.config.clone(),
        xcodebuild: runner,
        manifest: ProductsManifest::capture(&products, "SampleApp.xctestrun").unwrap(),
        products,
    }
}
#[test]
fn owned_runner_uses_verified_copy_and_retires_before_simulator() {
    let f = Fixture::new("normal");
    let config = config(&f);
    let directory = f.root.path().join("session");
    let mut owner = Xctest::start_at(
        config.clone(),
        &directory,
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
        &f.root.path().join("host"),
    )
    .unwrap();
    owner.ensure_running().unwrap();
    config.manifest.verify(&directory.join("products")).unwrap();
    let process: ProcessIdentity =
        frontcap_core::state::load(&directory.join("runner-process.json"))
            .unwrap()
            .unwrap();
    let result = owner.stop();
    assert!(result.passed, "{result:?}");
    assert!(process.has_exited().unwrap());
    assert!(owner.ensure_running().is_err());
    assert_eq!(owner.stop().passed, result.passed);
    assert!(directory.join("runner-retirement.json").exists());
    assert!(directory.join("simulator/retirement.json").exists());
}
#[test]
fn changed_products_fail_before_any_simulator_command() {
    let f = Fixture::new("normal");
    let config = config(&f);
    std::fs::write(config.products.join("SampleApp.xctestrun"), "changed").unwrap();
    assert!(
        Xctest::start_at(
            config,
            &f.root.path().join("session"),
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
            &f.root.path().join("host")
        )
        .is_err()
    );
    assert!(!f.root.path().join("calls").exists());
}
#[test]
fn failed_runner_spawn_still_retires_acquired_simulator() {
    let f = Fixture::new("normal");
    let mut config = config(&f);
    config.xcodebuild = f.root.path().join("missing");
    let error = match Xctest::start_at(
        config,
        &f.root.path().join("session"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
        &f.root.path().join("host"),
    ) {
        Err(error) => error,
        Ok(_) => panic!(),
    };
    assert!(error.retirement.unwrap().passed);
    assert!(f.root.path().join("deleted").exists());
}

#[test]
fn failed_retirement_publication_never_releases_host_fence() {
    for name in ["runner-retirement.json", "retirement.json"] {
        let f = Fixture::new("normal");
        let config = config(&f);
        let directory = f.root.path().join("session");
        let mut owner = Xctest::start_at(
            config,
            &directory,
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
            &f.root.path().join("host"),
        )
        .unwrap();
        std::fs::create_dir(directory.join(name)).unwrap();
        assert!(!owner.stop().passed);
        let host: crate::host::HostSession =
            frontcap_core::state::load(&f.root.path().join("host/native-session.json"))
                .unwrap()
                .unwrap();
        assert!(!host.retired);
        assert!(crate::host::acquire(&f.root.path().join("host")).is_err());
        assert!(f.root.path().join("deleted").exists());
    }
}

#[test]
fn readiness_binds_installed_runner_and_rejects_an_already_used_endpoint() {
    for custom in [false, true] {
        let f = Fixture::new("normal");
        let mut config = config(&f);
        if custom {
            let names = crate::products::ProductNames {
                app: "ClientApp".into(),
                tests: "ClientAppTests".into(),
            };
            config.products = f.root.path().join("client-products");
            crate::products::tests::named_fixture(&config.products, &names);
            config.manifest =
                ProductsManifest::capture_named(&config.products, "Consumer.xctestrun", names)
                    .unwrap();
            config.app = AppProfile {
                bundle: "org.example.client".into(),
                runner_bundle: "org.example.client.uitests.xctrunner".into(),
                test_identifier: "ClientAppTests/ClientController/testSession".into(),
                ui_descriptor: "Documents/ui-evidence/ready.json".into(),
                clock_descriptor: "Documents/clock-control.json".into(),
            };
        }
        let descriptor = config.app.ui_descriptor.clone();
        let runner_bundle = config.app.runner_bundle.clone();
        prepare_ready_runner(&f, &config);
        if custom {
            let script = std::fs::read_to_string(&config.xcodebuild).unwrap()
                .replace("'last_id':0", "'last_id':0,'system_alerts':True");
            std::fs::write(&config.xcodebuild, script).unwrap();
        }
        let mut owner = Xctest::start_at(
            config,
            &f.root.path().join("session"),
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
            &f.root.path().join("host"),
        )
        .unwrap();
        // A read-only discovery may outlast the former five-second subdivision while
        // remaining inside the caller's readiness deadline.
        std::fs::write(f.root.path().join("discovery-delay"), b"yes").unwrap();
        let ready = owner
            .wait_ready(
                Instant::now() + Duration::from_secs(10),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(
            ready.identity.simulator,
            "ABCD0000-0000-0000-0000-000000000001"
        );
        assert!(!ready.process.has_exited().unwrap());
        assert_eq!(ready.system_alerts, custom);
        // A live unrelated process is not an installed runner for this Simulator.
        let unrelated = serde_json::json!({"generation":ready.identity.generation,"pid":std::process::id(),"port":1234,"status":"ready","last_id":0});
        save_atomic(&ready.descriptor, &unrelated).unwrap();
        let error = owner
            .wait_ready(
                Instant::now() + Duration::from_secs(5),
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert!(error.to_string().contains("not the owned installed runner"));
        let used = serde_json::json!({"generation":ready.identity.generation,"pid":ready.identity.pid,"port":1234,"status":"ok","last_id":1});
        save_atomic(&ready.descriptor, &used).unwrap();
        assert!(
            owner
                .wait_ready(
                    Instant::now() + Duration::from_secs(5),
                    &AtomicBool::new(false)
                )
                .is_err()
        );
        let retirement = owner.stop();
        assert!(retirement.passed, "{retirement:?}");
        assert!(ready.process.has_exited().unwrap());
        assert!(
            f.root
                .path()
                .join(format!(
                    "session/documents-{runner_bundle}/{}",
                    descriptor.strip_prefix("Documents").unwrap().display()
                ))
                .exists()
        );
    }
}

#[test]
fn discovery_cancellation_never_restarts_the_owned_runner() {
    let f = Fixture::new("normal");
    let config = config(&f);
    let mut owner = Xctest::start_at(
        config,
        &f.root.path().join("session"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
        &f.root.path().join("host"),
    )
    .unwrap();
    assert!(
        owner
            .wait_ready(
                Instant::now() + Duration::from_secs(5),
                &AtomicBool::new(true)
            )
            .is_err()
    );
    owner.ensure_running().unwrap();
    assert!(owner.stop().passed);
}

#[test]
fn uncertain_bridge_retirement_keeps_host_fenced_after_device_deletion() {
    let f = Fixture::new("normal");
    let mut owner = Xctest::start_at(
        config(&f),
        &f.root.path().join("session"),
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
        &f.root.path().join("host"),
    )
    .unwrap();
    owner.retain_cleanup_failure("bridge retirement unconfirmed".into());
    let result = owner.stop();
    assert!(!result.passed);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("bridge retirement unconfirmed"))
    );
    assert!(f.root.path().join("deleted").exists());
    assert!(crate::host::acquire(&f.root.path().join("host")).is_err());
}

#[test]
fn configured_app_selects_its_test_and_limits_container_discovery() {
    let f = Fixture::new("normal");
    let mut config = config(&f);
    let products = f.root.path().join("client-products");
    let names = crate::products::ProductNames {
        app: "ClientApp".into(),
        tests: "ClientAppTests".into(),
    };
    crate::products::tests::named_fixture(&products, &names);
    config.products = products;
    config.manifest =
        ProductsManifest::capture_named(&config.products, "Consumer.xctestrun", names).unwrap();
    config.app = AppProfile {
        bundle: "org.example.client".into(),
        runner_bundle: "org.example.client.uitests.xctrunner".into(),
        test_identifier: "ClientAppTests/ClientController/testSession".into(),
        ui_descriptor: "Documents/ui-evidence/ui-control.json".into(),
        clock_descriptor: "Documents/clock-control.json".into(),
    };
    let directory = f.root.path().join("session");
    let mut owner = Xctest::start_at(
        config,
        &directory,
        Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false),
        &f.root.path().join("host"),
    )
    .unwrap();
    let command: (String, Vec<String>) =
        frontcap_core::state::load(&directory.join("command.json"))
            .unwrap()
            .unwrap();
    assert!(
        command
            .1
            .contains(&"-only-testing:ClientAppTests/ClientController/testSession".into())
    );
    let calls = std::fs::read(f.root.path().join("calls")).unwrap();
    assert!(
        owner
            .app_container(
                "org.example.app",
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(false)
            )
            .is_err()
    );
    assert_eq!(std::fs::read(f.root.path().join("calls")).unwrap(), calls);
    assert!(owner.stop().passed);
}

#[test]
fn invalid_app_profile_fails_before_device_acquisition() {
    for invalid in ["bundle", "descriptor", "test", "external-runner"] {
        let f = Fixture::new("normal");
        let mut config = config(&f);
        match invalid {
            "external-runner" => config.manifest.role = crate::products::ProductRole::ExternalRunner,
            "bundle" => config.app.bundle = "../another-app".into(),
            "descriptor" => config.app.ui_descriptor = "../outside.json".into(),
            _ => config.app.test_identifier = "OtherTests/Case/testSession".into(),
        }
        assert!(
            Xctest::start_at(
                config,
                &f.root.path().join("session"),
                Instant::now() + Duration::from_secs(10),
                &AtomicBool::new(false),
                &f.root.path().join("host")
            )
            .is_err()
        );
        assert!(!f.root.path().join("calls").exists());
    }
}

#[test]
fn runner_on_owned_simulator_transfers_the_existing_lease_and_retires_on_failure() {
    for fail in [false, true] {
        let f = Fixture::new("normal");
        let config = config(&f);
        let simulator = Simulator::acquire_at(
            f.config.clone(),
            &f.root.path().join("existing"),
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
            &f.root.path().join("host"),
        )
        .unwrap();
        let udid = simulator.udid().to_owned();
        let runner: RunnerLaunch = config.into();
        if fail {
            std::fs::write(runner.products.join("SampleApp.xctestrun"), "changed").unwrap();
        }
        let directory = f.root.path().join("attached");
        let result = Xctest::on_simulator(
            runner,
            simulator,
            &directory,
            Instant::now() + Duration::from_secs(10),
            &AtomicBool::new(false),
        );
        if fail {
            let error = match result {
                Ok(_) => panic!("changed products accepted"),
                Err(error) => error,
            };
            assert!(error.retirement.unwrap().passed);
            assert!(!directory.join("runner-process.json").exists());
        } else {
            let mut owner = result.unwrap();
            assert_eq!(owner.simulator_id(), udid);
            owner.ensure_running().unwrap();
            let identity: ProcessIdentity =
                frontcap_core::state::load(&directory.join("runner-process.json"))
                    .unwrap()
                    .unwrap();
            assert!(owner.stop().passed);
            assert!(identity.has_exited().unwrap());
        }
        let calls = std::fs::read_to_string(f.root.path().join("calls")).unwrap();
        let calls: Vec<Vec<String>> = calls
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(calls.iter().filter(|args| args[1] == "create").count(), 1);
        assert_eq!(calls.iter().filter(|args| args[1] == "delete").count(), 1);
        assert!(f.root.path().join("deleted").exists());
    }
}

pub(crate) fn prepare_ready_runner(f: &Fixture, config: &XctestLaunch) {
    let runner = format!("{}-Runner", config.manifest.names.tests);
    let device = f.root.path().join("ABCD0000-0000-0000-0000-000000000001");
    let container = device.join("container");
    std::fs::create_dir_all(container.join(config.app.ui_descriptor.parent().unwrap())).unwrap();
    let executable = device.join(&runner);
    std::fs::copy("/bin/sleep", &executable).unwrap();
    // A copied platform binary can be killed by macOS before main; sign only this fixture copy.
    #[cfg(target_os = "macos")]
    assert!(
        Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-"])
            .arg(&executable)
            .status()
            .unwrap()
            .success()
    );
    let script = r#"#!/usr/bin/env python3
import pathlib,subprocess,json,time
root=pathlib.Path(__file__).parent
device=root/'ABCD0000-0000-0000-0000-000000000001'
child=subprocess.Popen([str(device/'SampleAppTests-Runner'),'60'])
endpoint=device/'container/Documents/ui-control/ui-control.json'
pending=endpoint.with_suffix('.tmp')
pending.write_text(json.dumps({'generation':'abcd0000-0000-0000-0000-000000000002','pid':child.pid,'port':1234,'status':'ready','last_id':0}))
pending.replace(endpoint)
(root/'container-path').write_text(str(device/'container'))
try:time.sleep(60)
finally:child.terminate();child.wait()
"#;
    let script = script.replace("SampleAppTests-Runner", &runner).replace(
        "Documents/ui-control/ui-control.json",
        config.app.ui_descriptor.to_str().unwrap(),
    );
    std::fs::write(&config.xcodebuild, script).unwrap();
}

#[test]
fn readiness_rediscovers_a_runner_container_replaced_during_installation() {
    let f=Fixture::new("normal");
    let config=config(&f);
    prepare_ready_runner(&f,&config);
    let old=f.root.path().join("ABCD0000-0000-0000-0000-000000000001/old-container");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::write(f.root.path().join("container-path"),old.to_str().unwrap()).unwrap();
    let script=std::fs::read_to_string(&f.config.xcrun).unwrap().replace("print(pointer.read_text())", "print(pointer.read_text());(p/'first-discovered').write_text('yes')");
    std::fs::write(&f.config.xcrun,script).unwrap();
    let script=std::fs::read_to_string(&config.xcodebuild).unwrap().replace("child=subprocess.Popen", "while not (root/'first-discovered').exists():time.sleep(.01)\nchild=subprocess.Popen");
    std::fs::write(&config.xcodebuild,script).unwrap();
    let mut runner=Xctest::start_at(config,&f.root.path().join("runner"),Instant::now()+Duration::from_secs(10),&AtomicBool::new(false),&f.root.path().join("host")).unwrap();
    let ready=runner.wait_ready(Instant::now()+Duration::from_secs(10),&AtomicBool::new(false)).unwrap();
    assert!(!ready.descriptor.starts_with(&old));
    assert!(runner.stop().passed);
    assert!(ready.process.has_exited().unwrap());
}

#[test]
fn transfer_terminates_a_native_runner_supervised_outside_the_xcodebuild_group() {
    struct Guard(ProcessIdentity);
    impl Drop for Guard {
        fn drop(&mut self) { if !self.0.has_exited().unwrap() { unsafe { libc::kill(self.0.pid as i32,libc::SIGTERM); } } }
    }
    for mode in ["normal", "exited-before-terminate", "still-alive", "other-error"] {
    let f=Fixture::new("normal"); let config=config(&f); prepare_ready_runner(&f,&config);
    let script=std::fs::read_to_string(&config.xcodebuild).unwrap().replace(
        "'60'])", "'60'],start_new_session=True)");
    std::fs::write(&config.xcodebuild,script).unwrap();
    std::fs::write(f.root.path().join("termination-mode"),mode).unwrap();
    let script=std::fs::read_to_string(&f.config.xcrun).unwrap().replace("elif args[1]=='bootstatus':pass",r#"elif args[1]=='terminate':
    import os,signal
    assert args[3]=='org.example.app.uitests.xctrunner'
    endpoint=json.loads((pathlib.Path((p/'container-path').read_text())/'Documents/ui-control/ui-control.json').read_text())
    mode=(p/'termination-mode').read_text()
    if mode!='still-alive':os.kill(endpoint['pid'],signal.SIGTERM);time.sleep(.05)
    if mode in ('exited-before-terminate','still-alive'):sys.exit(3)
    if mode=='other-error':sys.exit(9)
elif args[1]=='bootstatus':pass"#);
    std::fs::write(&f.config.xcrun,script).unwrap();
    let dir=f.root.path().join("runner");
    let mut runner=Xctest::start_at(config,&dir,Instant::now()+Duration::from_secs(10),&AtomicBool::new(false),&f.root.path().join("host")).unwrap();
    let ready=runner.wait_ready(Instant::now()+Duration::from_secs(10),&AtomicBool::new(false)).unwrap();
    let _guard=Guard(ready.process.clone());
    let result=runner.release_simulator();
    if mode == "still-alive" || mode == "other-error" {
        let error=match result {Err(e)=>e,Ok(_)=>panic!("unverified termination accepted")};
        assert!(error.retirement.unwrap().passed);
        assert!(f.root.path().join("deleted").exists());
        assert!(!dir.join("simulator-transfer.json").exists());
        continue;
    }
    let mut simulator=result.unwrap();
    assert_eq!(dir.join("native-runner-exit-race.json").exists(),mode=="exited-before-terminate");
    assert!(ready.process.has_exited().unwrap());
    assert!(dir.join("native-runner-terminated.json").exists());
    assert!(!f.root.path().join("deleted").exists());
    let calls:Vec<Vec<String>>=std::fs::read_to_string(f.root.path().join("calls")).unwrap().lines().map(|s|serde_json::from_str(s).unwrap()).collect();
    assert_eq!(calls.iter().filter(|c|c[1]=="terminate").count(),1);
    assert!(simulator.stop().passed);
}
}
