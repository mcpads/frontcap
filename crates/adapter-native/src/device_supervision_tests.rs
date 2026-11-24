//! Device call-site acceptance with real owners and scripted Android tools.
use super::*;
use crate::device::Supervision;
const ROOT: &str = "FRONTCAP_SUPERVISED_DEVICE_TEST";
fn settings() -> Supervision {
    fn arguments(entry: &str) -> Vec<String> {
        vec!["-c".into(), format!("export FRONTCAP_RESOURCE_GROUP_DIRECTORY=\"$2\"; exec \"$1\" --exact resource_group::tests::{entry} --nocapture"),
            "device-supervisor".into(), std::env::current_exe().unwrap().to_str().unwrap().into()]
    }
    Supervision { executable: "/bin/sh".into(), group_arguments: arguments("group_entry"),
        resource_arguments: arguments("resource_entry"), deadline_ns: crate::worker::monotonic_ns().unwrap() + 120_000_000_000 }
}
fn setup() -> Fixture {
    let f = fixture();
    let source = std::fs::read_to_string(&f.config.adb).unwrap();
    let extra = r#"
if args==['shell','cat','/proc/net/unix']:
 print('0 0 0 0 0 0 0 @webview_devtools_remote_'+str(pid));sys.exit(0)
if args[:2]==['forward','--no-rebind']:
 assert not (root/'forward').exists()
 (root/'forward').write_text('SERIAL '+args[2]+' '+args[3]+'\n');sys.exit(0)
if args==['forward','--list']:
 if (root/'forward').exists():print((root/'forward').read_text(),end='')
 sys.exit(0)
if args[:2]==['forward','--remove']:
 (root/'forward').unlink();sys.exit(0)
"#.replace("SERIAL", &format!("emulator-{}", f.config.port));
    let source = source.replace("if args[:1] == ['install']:",
        "if args[:1]==['install'] and (root/'before-ready').exists():\n (root/'install-blocked').touch();time.sleep(240);sys.exit(0)\nif args[:1] == ['install']:");
    let source = source.replace("def stat(p,t):", &(extra + "\ndef stat(p,t):"));
    let source = source.replace("sys.exit(9 if (root/'lose-input').exists() else 0)",
        "if (root/'effect').exists():\n  (root/'effect-blocked').touch();time.sleep(240)\n sys.exit(9 if (root/'lose-input').exists() else 0)");
    std::fs::write(&f.config.adb, source).unwrap();
    f
}
fn acquire_supervised(config: AndroidLaunch, root: &Path) -> Device {
    Device::acquire_owned(config, &root.join("session"), Instant::now()+Duration::from_secs(30),
        &AtomicBool::new(false), &root.join("host"), Some(settings())).unwrap()
}
fn prepare(device: &mut Device, root: &Path) {
    let end = Instant::now()+Duration::from_secs(30); let cancel = AtomicBool::new(false);
    device.start_contact_driver(&root.join("input.jar"), &frontcap_core::canonical::sha256_bytes_hex(b"frozen-driver"), end, &cancel).unwrap();
    device.discover_webview_debugger(end, &cancel).unwrap();
    device.connect_webview_debugger(end, &cancel).unwrap();
}
fn wait_for<T>(mut read: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now()+Duration::from_secs(45);
    loop { if let Some(value) = read() { return value; }
        assert!(Instant::now()<end, "supervised Device observation expired"); std::thread::sleep(Duration::from_millis(10)); }
}
fn helper_exchange(result: &crate::resource_group::Retirement, id: &str) -> crate::process::Exchange {
    let proof = &result.helper_evidence[id];
    if let Some(stdout) = result.helper_evidence.get(&format!("{id}/stdout")) {
        let artifact: crate::process::ArtifactExchange = load(&proof.path).unwrap().unwrap();
        let bytes = artifact.read_stdout(&stdout.path).unwrap();
        assert_eq!(frontcap_core::canonical::sha256_bytes_hex(&bytes), stdout.sha256);
        artifact.exchange
    } else {
        crate::process::load_exchange(&proof.path).unwrap().unwrap()
    }
}
fn assert_retired(root: &Path, expected_resources: usize) -> crate::resource_group::Retirement {
    let result: crate::resource_group::Retirement = wait_for(|| load(&root.join("session-resources/retirement.json")).unwrap());
    assert!(result.passed, "{result:?}"); assert_eq!(result.resources.len(), expected_resources);
    for result in result.resources.values() { assert!(result.process.as_ref().unwrap().has_exited().unwrap()); }
    for (id, proof) in &result.helper_evidence {
        assert_eq!(frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(&proof.path).unwrap()), proof.sha256);
        if id.ends_with("/stdout") { continue; }
        let exchange = helper_exchange(&result, id);
        assert!(exchange.cleanup_failure.is_none());
        let process: ProcessIdentity = load(&proof.path.with_file_name("process.json")).unwrap().unwrap();
        assert!(process.has_exited().unwrap());
    }
    assert!(!root.join("forward").exists()); assert!(!root.join("session/emulator-temporary").exists());
    let owner: ProcessIdentity = load(&root.join("session-resources/owner.json")).unwrap().unwrap();
    wait_for(|| owner.has_exited().unwrap().then_some(()));
    assert!(crate::host::acquire(&root.join("host")).is_ok());
    result
}
#[test]
fn supervised_device_avd_failure_does_not_snapshot_apk_or_start_emulator() {
    let mut f = setup();
    let source = f.root().join("source-avd"); std::fs::create_dir(&source).unwrap();
    let name = f.config.avd.clone();
    std::fs::create_dir(source.join(format!("{name}.avd"))).unwrap();
    std::fs::write(source.join(format!("{name}.ini")), "target=android-35\n").unwrap();
    let configuration = source.join(format!("{name}.avd/config.ini"));
    std::fs::write(&configuration, "disk.dataPartition.path=<temp>\n").unwrap();
    f.config.avd_home = Some(source.clone());
    f.config.avd_inventory = Some(crate::products::BundleInventory::capture(&source).unwrap());
    std::fs::write(&configuration, "changed frozen source\n").unwrap();
    let acquired = Device::acquire_owned(f.config.clone(), &f.root().join("session"),
        Instant::now()+Duration::from_secs(10), &AtomicBool::new(false), &f.root().join("host"), Some(settings()));
    let Err(error) = acquired else { panic!("changed AVD input accepted") };
    assert!(error.cause.contains("AVD preparation"), "{error:?}");
    assert!(error.retirement.unwrap().passed);
    for name in ["app.apk", "emulator-process.json", "emulator-command.json", "emulator-temporary"] {
        assert!(!f.root().join("session").join(name).exists(), "unexpected {name}");
    }
    assert!(f.root().join("session/acquisition-failure.json").exists());
    assert_eq!(std::fs::read_to_string(configuration).unwrap(), "changed frozen source\n");
}

#[test]
fn supervised_device_preserves_restart_input_and_original_results() {
    let f = setup(); let mut device = acquire_supervised(f.config.clone(), f.root()); prepare(&mut device, f.root());
    let cancel = AtomicBool::new(false); let end = Instant::now()+Duration::from_secs(45);
    let old = device.app_process().unwrap().clone();
    let old_driver = device.inspect_contact_progress().unwrap().owner;
    device.capture_contacts(&id("before"), end, &cancel).unwrap();
    let operation = Operation::Set { expected_revision: 0, contacts: vec![Contact { id: 1, x: 0.5, y: 0.5 }] };
    let input = device.input_contacts(&id("held"), &id("before"), &operation, end, &cancel).unwrap();
    let replacement = device.restart_host(&id("restart"), &old, None, end, &cancel).unwrap();
    device.discover_webview_debugger(end, &cancel).unwrap(); device.connect_webview_debugger(end, &cancel).unwrap();
    device.capture_contacts(&id("after"), end, &cancel).unwrap();
    let count = device.command_number;
    assert_eq!(device.restart_host(&id("restart"), &old, None, end, &cancel).unwrap(), replacement);
    assert_eq!(device.command_number, count);
    assert_eq!(Device::read_contact_result(&f.root().join("session"), &old_driver, &id("held"), &id("before"), &operation).unwrap(), Some(input));
    let result = device.stop(); assert!(result.passed, "{result:?}");
    assert_eq!(device.stop(), result);
    let result = assert_retired(f.root(), 3);
    assert_eq!(result.forwarding.len(), 2);
    assert!(result.forwarding.values().all(|proof| proof.binding_confirmed && proof.removal_confirmed));
    assert!(read_host_replacement(&f.root().join("session"), &id("restart"), &old, None).unwrap().is_some());
}
#[test]
fn controller_entry() {
    let Some(root) = std::env::var_os(ROOT) else { return; }; let root = PathBuf::from(root);
    let config: AndroidLaunch = load(&root.join("controller-config.json")).unwrap().unwrap();
    let mut device = acquire_supervised(config, &root); prepare(&mut device, &root);
    if root.join("effect").exists() {
        let end=Instant::now()+Duration::from_secs(30); let cancel=AtomicBool::new(false);
        device.capture_contacts(&id("before"), end, &cancel).unwrap();
        device.input_contacts(&id("held"), &id("before"),
            &Operation::Set { expected_revision: 0, contacts: vec![Contact { id: 1, x: 0.5, y: 0.5 }] }, end, &cancel).unwrap();
        panic!("controller must die while helper effect awaits completion");
    }
    std::fs::write(root.join("controller-ready"), b"ready").unwrap();
    loop { std::thread::sleep(Duration::from_secs(1)); }
}
#[test]
fn supervised_device_worker_loss_retires_before_ready_idle_and_after_input_effect() {
    for phase in ["before-ready", "idle", "effect"] {
        eprintln!("supervised worker-loss phase: {phase}");
        let f = setup(); std::fs::write(f.root().join(phase), b"phase").unwrap();
        save_atomic(&f.root().join("controller-config.json"), &f.config).unwrap();
        struct Controller(std::process::Child);
        impl Drop for Controller { fn drop(&mut self) { let _=self.0.kill(); let _=self.0.wait(); } }
        let mut controller=Controller(Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "device::host_restart::tests::supervised_tests::controller_entry", "--nocapture"])
            .env(ROOT, f.root()).stdout(Stdio::null()).spawn().unwrap());
        let marker=match phase { "before-ready"=>"install-blocked", "effect"=>"effect-blocked", _=>"controller-ready" };
        wait_for(|| f.root().join(marker).exists().then_some(()));
        controller.0.kill().unwrap(); controller.0.wait().unwrap();
        let result=assert_retired(f.root(), if phase=="before-ready" {1} else {2});
        assert_eq!(result.forwarding.len(), if phase=="before-ready" {0} else {1});
        assert!(!f.root().join("session/retirement.json").exists(), "dead worker must not publish successful retirement");
        if phase=="effect" {
            assert!(f.root().join("input-reply").exists());
            assert!(result.helper_evidence.keys().filter(|id| !id.ends_with("/stdout")).any(|id| {
                let exchange = helper_exchange(&result, id);
                exchange.failure.as_deref()==Some("bridge cancelled")
            }));
        }
    }
}
