use super::*;
const ROOT: &str = "FRONTCAP_RESOURCE_GROUP_TEST";
const DIRECTORY: &str = "FRONTCAP_RESOURCE_GROUP_DIRECTORY";
fn arguments(entry: &str) -> Vec<String> {
    vec!["-c".into(), format!("export {DIRECTORY}=\"$2\"; exec \"$1\" --exact resource_group::tests::{entry} --nocapture"),
        "resource-group-test".into(), std::env::current_exe().unwrap().to_str().unwrap().into()]
}
fn config(root: &Path) -> Config {
    Config { host: root.join("host"), target: root.join("target"), evidence_root: None, artifact_budget_bytes: None, temporary_storage_root: None, scratch_volume: None, scratch_volume_root: None, resource_executable: "/bin/sh".into(),
        resource_arguments: arguments("resource_entry"), ports: vec![], deadline_ns: monotonic_ns().unwrap() + 20_000_000_000 }
}
fn start(root: &Path, config: &Config) -> Group {
    let mut command = Command::new("/bin/sh"); command.args(arguments("group_entry")).env(ROOT, root);
    Group::start(&mut command, config, &root.join("group"), Instant::now() + Duration::from_secs(5)).unwrap()
}
fn spawn(group: &mut Group, id: &str, config: &Config) -> ProcessIdentity {
    let operation = Operation::Start { id: RequestId::new(id).unwrap(), launch: Launch {
        program: "/bin/sleep".into(), arguments: vec!["300".into()], environment: BTreeMap::new(), remove_environment: vec![],
        directory: group.ready.target.clone(), deadline_ns: config.deadline_ns } };
    let Reply::Started { process } = group.execute(operation, Instant::now() + Duration::from_secs(5)).unwrap()
        else { panic!("resource did not start") };
    process
}
fn wait_for<T>(mut read: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(value) = read() { return value; }
        assert!(Instant::now() < end, "resource group observation deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn group_entry() {
    let Some(directory) = std::env::var_os(DIRECTORY) else { return; };
    run(Path::new(&directory)).unwrap();
}
#[test]
fn resource_entry() {
    let Some(directory) = std::env::var_os(DIRECTORY) else { return; };
    crate::resource_process::run(Path::new(&directory)).unwrap();
}
fn volume_fixture(root: &Path) -> crate::storage_volume::Config {
    let worker = root.join("volume.py");
    std::fs::write(&worker, r#"import sys,json,os,pathlib,subprocess,shutil
p=pathlib.Path(sys.argv[1])
assert sys.stdin.buffer.read(1)==b'1'
p.mkdir();mount=p/'volume/mount';mount.mkdir(parents=True)
def save(name,value):
 q=p/name;t=q.with_suffix('.tmp');t.write_text(json.dumps(value));t.replace(q)
save('ready.json',dict(mount=str(mount),image=str(p/'volume/bounded.dmg'),size_mib=int(sys.argv[3]),reserve_mib=int(sys.argv[5]),owner_pid=os.getpid()))
line=sys.stdin.buffer.readline()
if not line:
 save('abandoned.json',dict(pid=os.getpid()));sys.exit(1)
request=json.loads(line)
assert request==dict(operation='retire',writers_retired=True,remove=True)
identity=json.loads((p/'watch.json').read_text())
assert subprocess.run(['ps','-p',str(identity['pid']),'-o','pid='],stdout=subprocess.PIPE).returncode==1
save('retirement-request.json',request)
shutil.rmtree(mount)
save('retirement.json',dict(passed=True,detached=True,removed=True))
"#).unwrap();
    crate::storage_volume::Config { python: "/usr/bin/python3".into(), worker, size_mib: 256, reserve_mib: 8 }
}

#[test]
fn resource_group_volume_retirement_requires_process_and_port_proofs() {
    for (blocked_port, changed_identity, external) in [(false, false, false), (true, false, false), (false, true, false), (false, false, true)] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let mut config = config(&root);
        config.scratch_volume = Some(volume_fixture(&root));
        if external { config.scratch_volume_root = Some(root.join("external-volume")); }
        let listener = blocked_port.then(|| std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap());
        if let Some(listener) = &listener { config.ports.push(listener.local_addr().unwrap().port()); }
        let mut group = start(&root, &config);
        let process = spawn(&mut group, "emulator", &config);
        let worker = config.scratch_volume_directory(&root.join("group")).join("worker");
        save_atomic(&worker.join("watch.json"), &process).unwrap();
        let scratch = group.ready.temporary_disks.clone();
        std::fs::write(scratch.join("overlay"), b"owned").unwrap();
        if changed_identity {
            std::fs::write(worker.parent().unwrap().join("worker-process.json"), br#"{"pid":1,"started":"not the original worker"}"#).unwrap();
        }
        let retirement = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
        assert!(process.has_exited().unwrap());
        assert_eq!(retirement.passed, !blocked_port && !changed_identity, "{retirement:?}");
        if blocked_port || changed_identity {
            wait_for(|| worker.join("abandoned.json").exists().then_some(()));
            assert!(worker.join("volume/mount").exists());
            assert_eq!(scratch.exists(), blocked_port);
            assert!(!worker.join("retirement-request.json").exists());
        } else {
            assert!(!scratch.exists());
            assert!(worker.join("retirement-request.json").exists());
            let inspect = || inspection::inspect_with_volume_root(&root.join("group"), &config.target,
                &config.ports, config.scratch_volume.as_ref(), config.scratch_volume_root.as_deref());
            if external {
                assert!(!root.join("group/scratch-volume").exists());
                assert!(inspection::inspect(&root.join("group"), &config.target, &config.ports, config.scratch_volume.as_ref()).is_err());
            }
            assert!(matches!(inspect().unwrap(), inspection::Inspection::Finished(_)));
            assert!(inspection::inspect(&root.join("group"), &config.target, &config.ports, None).is_err());
            for path in [worker.join("ready.json"), worker.join("retirement.json"), worker.parent().unwrap().join("worker-process.json")] {
                let original = std::fs::read(&path).unwrap();
                let changed = if path.file_name().unwrap() == "worker-process.json" {
                    br#"{"pid":1,"started":"not the original worker"}"#.to_vec()
                } else {
                    // Still valid JSON with identical semantics: the original byte binding matters.
                    let mut bytes = original.clone(); bytes.push(b'\n'); bytes
                };
                std::fs::write(&path, changed).unwrap();
                match inspect() {
                    Err(_) => (),
                    Ok(inspection::Inspection::Active) => panic!("inspection lock remained active while checking changed proof: {}", path.display()),
                    Ok(_) => panic!("changed volume proof was not rejected: {}", path.display()),
                }
                std::fs::write(&path, original).unwrap();
            }
            assert!(matches!(inspect().unwrap(), inspection::Inspection::Finished(_)));
        }
    }
}
#[test]
fn resource_group_volume_owner_loss_preserves_unresolved_storage_and_fence() {
    for lose_group in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let mut config = config(&root);
        config.scratch_volume = Some(volume_fixture(&root));
        let mut group = start(&root, &config);
        let process = spawn(&mut group, "emulator", &config);
        let worker = root.join("group/scratch-volume/worker");
        save_atomic(&worker.join("watch.json"), &process).unwrap();
        let volume_process: ProcessIdentity = load(&worker.parent().unwrap().join("worker-process.json")).unwrap().unwrap();
        let scratch = group.ready.temporary_disks.clone();
        std::fs::write(scratch.join("overlay"), b"owned").unwrap();
        if lose_group {
            group.child.as_mut().unwrap().kill().unwrap();
            assert!(group.stop(Instant::now() + Duration::from_secs(10)).is_err());
            wait_for(|| worker.join("abandoned.json").exists().then_some(()));
            assert!(matches!(inspection::inspect(&root.join("group"), &config.target,
                &config.ports, config.scratch_volume.as_ref()).unwrap(), inspection::Inspection::Unresolved));
            assert_eq!(std::fs::read(scratch.join("overlay")).unwrap(), b"owned");
        } else {
            assert!(volume_process.kill().unwrap());
            let retirement = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
            assert!(!retirement.passed, "{retirement:?}");
            assert!(retirement.errors.iter().any(|error| error.contains(
                root.join("group/scratch-volume").to_str().unwrap())), "{retirement:?}");
            assert!(retirement.scratch_volume.is_none());
            assert!(!scratch.exists());
            let original = std::fs::read(root.join("group/retirement.json")).unwrap();
            assert!(matches!(inspection::inspect(&root.join("group"), &config.target,
                &config.ports, config.scratch_volume.as_ref()).unwrap(), inspection::Inspection::Finished(result) if !result.passed));
            assert_eq!(std::fs::read(root.join("group/retirement.json")).unwrap(), original);
        }
        wait_for(|| process.has_exited().unwrap().then_some(()));
        wait_for(|| volume_process.has_exited().unwrap().then_some(()));
        assert!(worker.join("volume/mount").is_dir());
        assert!(!worker.join("retirement-request.json").exists());
        assert!(!worker.join("retirement.json").exists());
        assert!(!load::<HostSession>(&root.join("host/native-session.json")).unwrap().unwrap().retired);
        assert!(host::acquire(&root.join("host")).is_err());
    }
}
#[test]
fn resource_group_avd_copy_relocates_registry_and_retires_only_owned_data() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let source = root.join("source"); std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(source.join("Phone.avd")).unwrap();
    std::fs::write(source.join("Phone.ini"), "path=/shared/Phone.avd\npath.rel=avd/Phone.avd\ntarget=android-35\n").unwrap();
    std::fs::write(source.join("Phone.avd/config.ini"), "disk.dataPartition.path=<temp>\n").unwrap();
    std::fs::write(source.join("Phone.avd/userdata.img"), b"frozen app data").unwrap();
    std::fs::write(source.join("Phone.avd/snapshot.lock.lock"), format!("{}\0", std::process::id())).unwrap();
    std::fs::write(source.join("Phone.avd/consumer.lock"), b"consumer data, not an owner record").unwrap();
    let inventory = crate::products::BundleInventory::capture(&source).unwrap();
    let config = config(&root); let mut group = start(&root, &config);
    let request = || Operation::PrepareAvd { source: source.clone(), name: "Phone".into(), inventory: inventory.clone() };
    let Reply::AvdPrepared { directory } = group.execute(request(), Instant::now() + Duration::from_secs(5)).unwrap()
        else { panic!("AVD copy not prepared") };
    assert_eq!(directory, group.ready.temporary_disks.join("avd"));
    let registry = std::fs::read_to_string(directory.join("Phone.ini")).unwrap();
    assert!(registry.contains(&format!("path={}\n", directory.join("Phone.avd").display())));
    assert!(!registry.contains("path.rel=") && !registry.contains("/shared/"));
    assert!(root.join("target/avd-copy.json").exists());
    assert!(!directory.join("Phone.avd/snapshot.lock.lock").exists());
    assert_eq!(std::fs::read(directory.join("Phone.avd/consumer.lock")).unwrap(), b"consumer data, not an owner record");
    let receipt: serde_json::Value = load(&root.join("target/avd-copy.json")).unwrap().unwrap();
    assert_eq!(receipt["discarded_owner_records"]["Phone.avd/snapshot.lock.lock"],
        serde_json::to_value(&inventory.files["Phone.avd/snapshot.lock.lock"]).unwrap());
    assert!(receipt["prepared_inventory"]["files"].get("Phone.avd/snapshot.lock.lock").is_none());
    assert!(matches!(group.execute(request(), Instant::now() + Duration::from_secs(5)).unwrap(), Reply::Failed { .. }));
    let process = spawn(&mut group, "emulator", &config);
    std::fs::write(directory.join("Phone.avd/userdata.img"), b"changed private data").unwrap();
    assert!(matches!(group.execute(request(), Instant::now() + Duration::from_secs(5)).unwrap(), Reply::Failed { .. }));
    let retirement = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(retirement.passed, "{retirement:?}"); assert!(process.has_exited().unwrap());
    assert!(!directory.exists()); inventory.verify(&source).unwrap();
    assert!(root.join("target/avd-copy.json").exists());
}

#[test]
fn resource_group_avd_copy_rejects_changed_input_and_external_writable_paths() {
    for changed_source in [false, true] {
        let temporary = tempfile::tempdir().unwrap(); let root = temporary.path().canonicalize().unwrap();
        let source = root.join("source"); std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(source.join("Phone.avd")).unwrap();
        std::fs::write(source.join("Phone.ini"), "target=android-35\n").unwrap();
        let config_path = source.join("Phone.avd/config.ini");
        std::fs::write(&config_path, "disk.dataPartition.path=/shared/writable.img\n").unwrap();
        let inventory = crate::products::BundleInventory::capture(&source).unwrap();
        if changed_source { std::fs::write(&config_path, "disk.dataPartition.path=<temp>\n").unwrap(); }
        let original = std::fs::read(&config_path).unwrap();
        let config = config(&root); let mut group = start(&root, &config);
        let scratch = group.ready.temporary_disks.clone();
        let reply = group.execute(Operation::PrepareAvd { source: source.clone(), name: "Phone".into(), inventory },
            Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(matches!(reply, Reply::Failed { .. }), "{reply:?}");
        assert!(!root.join("target/avd-copy.json").exists());
        assert!(group.stop(Instant::now() + Duration::from_secs(10)).unwrap().passed);
        assert!(!scratch.exists()); assert_eq!(std::fs::read(config_path).unwrap(), original);
    }
}

#[test]
fn resource_group_external_scratch_keeps_retirement_records_outside_storage_root() {
    let root = tempfile::tempdir().unwrap();
    let storage = tempfile::tempdir().unwrap();
    let mut config = config(root.path());
    config.temporary_storage_root = Some(storage.path().to_owned());
    let mut group = start(root.path(), &config);
    let scratch = group.ready.temporary_disks.clone();
    assert!(scratch.starts_with(storage.path().canonicalize().unwrap()));
    let process = spawn(&mut group, "emulator", &config);
    std::fs::write(scratch.join("overlay"), b"owned scratch").unwrap();
    let result = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(result.passed, "{result:?}");
    assert!(process.has_exited().unwrap());
    assert!(!scratch.exists());
    assert!(storage.path().is_dir());
    assert!(root.path().join("target/emulator-temporary-cleanup.json").exists());
    assert!(root.path().join("group/retirement.json").exists());
}
#[test]
fn controller_entry() {
    let Some(root) = std::env::var_os(ROOT) else { return; };
    let root = PathBuf::from(root); let config = config(&root); let mut group = start(&root, &config);
    std::fs::write(group.ready.temporary_disks.join("overlay"), b"owned scratch").unwrap();
    let processes = [spawn(&mut group, "emulator", &config), spawn(&mut group, "input", &config)];
    save_atomic(&root.join("controller-ready.json"), &processes).unwrap();
    loop { std::thread::sleep(Duration::from_secs(1)); }
}
#[test]
fn resource_group_controller_loss_retires_children_disks_and_host_lease() {
    let root = tempfile::tempdir().unwrap();
    struct Controller(Child);
    impl Drop for Controller { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let mut controller = Controller(Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "resource_group::tests::controller_entry", "--nocapture"])
        .env(ROOT, root.path()).stdout(Stdio::null()).spawn().unwrap());
    let processes: Vec<ProcessIdentity> = wait_for(|| load(&root.path().join("controller-ready.json")).unwrap());
    assert!(host::acquire(&root.path().join("host")).is_err());
    controller.0.kill().unwrap(); controller.0.wait().unwrap();
    let result: Retirement = wait_for(|| load(&root.path().join("group/retirement.json")).unwrap());
    assert!(result.passed, "{result:?}"); assert_eq!(result.resources.len(), 2);
    for process in processes { assert!(process.has_exited().unwrap()); }
    assert!(!root.path().join("target/emulator-temporary").exists());
    let owner: ProcessIdentity = load(&root.path().join("group/owner.json")).unwrap().unwrap();
    wait_for(|| owner.has_exited().unwrap().then_some(()));
    assert!(load::<HostSession>(&root.path().join("host/native-session.json")).unwrap().unwrap().retired);
    assert!(host::acquire(&root.path().join("host")).is_ok());
    let bytes = std::fs::read(root.path().join("group/retirement.json")).unwrap();
    assert!(run(&root.path().join("group")).is_err());
    assert_eq!(std::fs::read(root.path().join("group/retirement.json")).unwrap(), bytes);
}
#[test]
fn resource_group_stops_selected_child_and_keeps_reused_identity_from_spawning() {
    let root = tempfile::tempdir().unwrap(); let config = config(root.path()); let mut group = start(root.path(), &config);
    let process = spawn(&mut group, "input", &config);
    let stop = group.execute(Operation::Stop { id: RequestId::new("input").unwrap() }, Instant::now() + Duration::from_secs(5)).unwrap();
    assert!(matches!(stop, Reply::Stopped { result } if result.retirement_error.is_none()));
    assert!(process.has_exited().unwrap());
    let request = Operation::Start { id: RequestId::new("input").unwrap(), launch: Launch {
        program: "/bin/sleep".into(), arguments: vec!["300".into()], environment: BTreeMap::new(), remove_environment: vec![],
        directory: root.path().into(), deadline_ns: config.deadline_ns } };
    assert!(matches!(group.execute(request, Instant::now() + Duration::from_secs(5)).unwrap(), Reply::Failed { .. }));
    let result = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(result.passed, "{result:?}"); assert_eq!(result.resources.len(), 1);
    assert!(!root.path().join("target/emulator-temporary").exists());
}
#[test]
fn resource_group_changed_scratch_identity_keeps_shared_data_and_host_fence() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap(); let config = config(root.path()); let group = start(root.path(), &config);
    let shared = root.path().join("shared"); std::fs::create_dir(&shared).unwrap();
    std::fs::write(shared.join("base"), b"unrelated").unwrap();
    std::fs::rename(&group.ready.temporary_disks, root.path().join("displaced")).unwrap();
    symlink(&shared, &group.ready.temporary_disks).unwrap();
    let result = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(!result.passed);
    assert!(result.errors.iter().any(|e| e.contains("identity changed")), "{result:?}");
    assert_eq!(std::fs::read(shared.join("base")).unwrap(), b"unrelated");
    assert!(!load::<HostSession>(&root.path().join("host/native-session.json")).unwrap().unwrap().retired);
    assert!(host::acquire(&root.path().join("host")).is_err());
}

#[test]
fn resource_group_retirement_does_not_erase_operation_failure_or_keep_clean_host_fenced() {
    let root = tempfile::tempdir().unwrap(); let config = config(root.path()); let mut group = start(root.path(), &config);
    let process = spawn(&mut group, "input", &config);
    let invalid = root.path().join("group/commands/2.json");
    std::fs::write(&invalid, b"{invalid").unwrap();
    let _: Retirement = wait_for(|| load(&root.path().join("group/retirement.json")).unwrap());
    let result = group.stop(Instant::now() + Duration::from_secs(5)).unwrap();
    assert!(result.operation_error.is_some());
    assert!(result.passed, "{result:?}");
    assert!(process.has_exited().unwrap());
    assert!(!root.path().join("target/emulator-temporary").exists());
    assert_eq!(std::fs::read(invalid).unwrap(), b"{invalid");
    assert!(load::<HostSession>(&root.path().join("host/native-session.json")).unwrap().unwrap().retired);
    assert!(host::acquire(&root.path().join("host")).is_ok());
}

#[test]
fn resource_group_helper_capture_preserves_output_exit_and_original_evidence() {
    let root = tempfile::tempdir().unwrap(); let config = config(root.path()); let mut group = start(root.path(), &config);
    let evidence = group.ready.target.join("command-1");
    let mut command = Command::new("/bin/sh"); command.args(["-c", "printf output; printf diagnostic >&2; exit 7"]);
    let result = group.capture(&command, &evidence, Instant::now() + Duration::from_secs(5), &AtomicBool::new(false), 4096).unwrap();
    assert_eq!(result.stdout, b"output"); assert_eq!(result.stderr, b"diagnostic");
    assert_eq!(result.exit_code, Some(7)); assert!(result.failure.is_some()); assert!(result.cleanup_failure.is_none());
    let original = std::fs::read(evidence.join("exchange.json")).unwrap();
    let result = group.capture(&Command::new("/usr/bin/true"), &group.ready.target.join("command-2"),
        Instant::now() + Duration::from_secs(5), &AtomicBool::new(false), 4096).unwrap();
    assert_eq!(result.exit_code, Some(0)); assert!(result.failure.is_none());
    let marker = root.path().join("must-not-run");
    let mut duplicate = Command::new("/usr/bin/touch"); duplicate.arg(&marker);
    assert!(group.capture(&duplicate, &evidence, Instant::now() + Duration::from_secs(5), &AtomicBool::new(false), 4096).is_err());
    let retirement = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(retirement.passed, "{retirement:?}"); assert!(retirement.operation_error.is_some());
    assert_eq!(retirement.helper_evidence.len(), 2); assert!(!marker.exists());
    assert_eq!(std::fs::read(evidence.join("exchange.json")).unwrap(), original);
}
#[test]
fn capture_controller_entry() {
    let Some(root) = std::env::var_os(ROOT) else { return; };
    let root = PathBuf::from(root); let config = config(&root); let mut group = start(&root, &config);
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf applied; printf retained >&2; printf once > \"$1\"; exec /bin/sleep 300", "helper"]).arg(root.join("effect"));
    if std::env::var_os("FRONTCAP_BINARY_CAPTURE").is_some() {
        group.capture_artifact(&command, &group.ready.target.join("helper"), "screen.png",
            Instant::now()+Duration::from_secs(15), &AtomicBool::new(false), 4096).unwrap();
    } else {
    group.capture(&command, &group.ready.target.join("helper"), Instant::now() + Duration::from_secs(15), &AtomicBool::new(false), 4096).unwrap();
    }
    panic!("test controller must be killed while the helper is active");
}
#[test]
fn resource_group_controller_loss_cancels_owned_helper_and_preserves_partial_exchange() {
    for binary in [false, true] {
        let root = tempfile::tempdir().unwrap();
        struct Controller(Child);
        impl Drop for Controller { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.env_remove("FRONTCAP_BINARY_CAPTURE");
        if binary { command.env("FRONTCAP_BINARY_CAPTURE", "1"); }
        let mut controller = Controller(command
            .args(["--exact", "resource_group::tests::capture_controller_entry", "--nocapture"])
            .env(ROOT, root.path()).stdout(Stdio::null()).spawn().unwrap());
        wait_for(|| root.path().join("effect").exists().then_some(()));
        // Child effects can precede the owner publishing its process identity. Await both independently.
        let helper: ProcessIdentity = wait_for(|| load(&root.path().join("target/helper/process.json")).unwrap());
        assert!(!helper.has_exited().unwrap());
        controller.0.kill().unwrap(); controller.0.wait().unwrap();
        let retirement: Retirement = wait_for(|| load(&root.path().join("group/retirement.json")).unwrap());
        assert!(retirement.passed, "{retirement:?}"); assert_eq!(retirement.helper_evidence.len(), if binary { 2 } else { 1 });
        let path = root.path().join("target/helper/exchange.json");
        let exchange = if binary {
            let result: crate::process::ArtifactExchange = load(&path).unwrap().unwrap();
            assert_eq!(result.read_stdout(&root.path().join("target/helper/screen.png").canonicalize().unwrap()).unwrap(), b"applied");
            assert!(result.exchange.stdout.is_empty());
            result.exchange
        } else {
            let result: crate::process::Exchange = crate::process::load_exchange(&path).unwrap().unwrap();
            assert_eq!(result.stdout, b"applied"); result
        };
     assert_eq!(exchange.stderr, b"retained");
        assert_eq!(exchange.failure.as_deref(), Some("bridge cancelled")); assert!(exchange.cleanup_failure.is_none());
        assert!(helper.has_exited().unwrap());
        assert_eq!(std::fs::read(root.path().join("effect")).unwrap(), b"once");
        assert!(!root.path().join("target/emulator-temporary").exists());
        let original = std::fs::read(&path).unwrap();
        for _ in 0..2 {
            if binary { let _: crate::process::ArtifactExchange = load(&path).unwrap().unwrap(); }
            else { let _: crate::process::Exchange = crate::process::load_exchange(&path).unwrap().unwrap(); }
        }
        assert_eq!(std::fs::read(path).unwrap(), original);
        let owner: ProcessIdentity = load(&root.path().join("group/owner.json")).unwrap().unwrap();
        wait_for(|| owner.has_exited().unwrap().then_some(()));
        assert!(host::acquire(&root.path().join("host")).is_ok());
    }
}

#[test]
fn resource_group_helper_keeps_session_evidence_paths_and_rejects_symlink_escape() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap(); let outside = tempfile::tempdir().unwrap();
    let mut config = config(root.path()); config.evidence_root = Some(root.path().to_owned());
    let mut group = start(root.path(), &config);
    let evidence = group.ready.evidence_root.join("page-discovery");
    let result = group.capture(&Command::new("/usr/bin/true"), &evidence,
        Instant::now() + Duration::from_secs(5), &AtomicBool::new(false), 4096).unwrap();
    assert!(result.failure.is_none()); assert!(evidence.join("exchange.json").exists());
    symlink(outside.path(), root.path().join("escaped")).unwrap();
    let escaped = group.ready.evidence_root.join("escaped/command");
    assert!(group.capture(&Command::new("/usr/bin/true"), &escaped,
        Instant::now() + Duration::from_secs(5), &AtomicBool::new(false), 4096).is_err());
    assert!(!outside.path().join("command").exists());
    let retirement = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(retirement.passed, "{retirement:?}"); assert_eq!(retirement.helper_evidence.len(), 1);
    assert!(evidence.join("exchange.json").exists());
}

fn forwarding_config(root: &Path) -> Config {
    let mut result = config(root);
    let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    result.ports.push(reservation.local_addr().unwrap().port());
    result
}
fn fake_adb(root: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join("adb");
    std::fs::write(&path, br##"#!/bin/sh
cd "$(dirname "$0")" || exit 2
case "$4" in
  --list) if test -f mapping; then cat mapping; fi ;;
  --no-rebind)
    if test -s mapping; then exit 1; fi
    printf '%s %s %s\n' "$2" "$5" "$6" > mapping
    if test -f uncertain; then exec /bin/sleep 300; fi ;;
  --remove) printf 'removed\n' >> removals; rm -f mapping ;;
  *) exit 2 ;;
esac
"##).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}
fn register_forward(group: &mut Group, config: &Config, process: &ProcessIdentity,
    adb: &Path, port: u16, id: &str) -> Reply {
    group.execute(Operation::RegisterForward { id: RequestId::new(id).unwrap(), registration: ForwardRegistration {
        adb: adb.to_owned(), server_port: None, write_root: None, emulator: process.clone(), serial: format!("emulator-{}", config.ports[0]),
        socket: "webview_devtools_remote_123".into(), port, bind_evidence: format!("bind-{id}").into(),
    } }, Instant::now() + Duration::from_secs(5)).unwrap()
}
fn forward_command(adb: &Path, config: &Config, port: u16, bind: bool) -> Command {
    let mut command = Command::new(adb);
    command.args(["-s", &format!("emulator-{}", config.ports[0]), "forward",
        if bind { "--no-rebind" } else { "--remove" }, &format!("tcp:{port}")]);
    if bind { command.arg("localabstract:webview_devtools_remote_123"); }
    command
}
fn capture_forward(group: &mut Group, command: &Command, evidence: &str) -> crate::process::Exchange {
    group.capture(command, &group.ready.target.join(evidence), Instant::now() + Duration::from_secs(10),
        &AtomicBool::new(false), 4096).unwrap()
}

#[test]
fn owned_adb_server_retirement_survives_server_and_controller_loss() {
    use super::inspection::{inspect, Inspection};
    for (socket, (disconnected, lost, occupied)) in [
        (false, false, false), (true, false, false), (false, true, false),
        (true, true, false), (false, true, true),
    ].into_iter().flat_map(|case| ["webview_devtools_remote_123", "tcp:43210"].map(|socket| (socket, case))) {
        let root = tempfile::tempdir().unwrap();
        let config = forwarding_config(&root.path().canonicalize().unwrap());
        let adb = fake_adb(root.path());
        let script = std::fs::read_to_string(&adb).unwrap().replace("case \"$4\" in", r#"
test ! -f group/resources/adb/result.json || exit 7
if test -f group/resources/emulator/result.json; then printf 'after-target\n' >> phases; fi
case "$4" in"#);
        std::fs::write(&adb, script).unwrap();
        let mut group = start(root.path(), &config);
        let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let server_port = reservation.local_addr().unwrap().port(); drop(reservation);
        let mut server_command = Command::new("/bin/sh");
        server_command.args(["-c", "while test ! -f retire-adb; do sleep 0.02; done; rm -f mapping"]).current_dir(root.path());
        let server = group.start_adb_server(RequestId::new("adb").unwrap(), server_port,
            &server_command, Instant::now() + Duration::from_secs(5), &AtomicBool::new(false)).unwrap();
        assert!(matches!(group.execute(Operation::Stop { id: RequestId::new("adb").unwrap() },
            Instant::now() + Duration::from_secs(5)).unwrap(), Reply::Failed { .. }));
        assert!(!server.has_exited().unwrap());
        let process = spawn(&mut group, "emulator", &config);
        let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        // A group with an owned server refuses fallback to the shared server.
        assert!(matches!(register_forward(&mut group, &config, &process, &adb, port, "wrong"), Reply::Failed { .. }));
        let reply = group.execute(Operation::RegisterForward { id: RequestId::new("private").unwrap(),
            registration: ForwardRegistration { adb: adb.clone(), server_port: Some(server_port), write_root: None,
                emulator: process, serial: format!("emulator-{}", config.ports[0]),
                socket: socket.into(), port, bind_evidence: "bind-private".into() },
        }, Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(matches!(reply, Reply::ForwardRegistered));
        let mut command = forward_command(&adb, &config, port, true);
        if socket.starts_with("tcp:") {
            command = Command::new(&adb);
            command.args(["-s", &format!("emulator-{}", config.ports[0]), "forward", "--no-rebind",
                &format!("tcp:{port}"), socket]);
        }
        command.env("ADB_SERVER_SOCKET", format!("tcp:127.0.0.1:{server_port}"));
        assert!(capture_forward(&mut group, &command, "bind-private").failure.is_none());
        let retained_listener = occupied.then_some(reservation);
        if lost {
            std::fs::write(root.path().join("retire-adb"), b"retire").unwrap();
            wait_for(|| (server.has_exited().unwrap()
                && root.path().join("group/resources/adb/result.json").exists()).then_some(()));
        }
        let target = group.ready.target.clone();
        let retirement = if disconnected {
            drop(group);
            wait_for(|| match inspect(&root.path().join("group"), &target, &config.ports, None).unwrap() {
                Inspection::Finished(result) => Some(*result), _ => None,
            })
        } else { group.stop(Instant::now() + Duration::from_secs(15)).unwrap() };
        if occupied {
            assert!(!retirement.passed, "foreign listener must prevent retirement");
            assert!(matches!(retirement.forwarding["private"].disposition, ForwardDisposition::Unresolved));
            assert!(TcpStream::connect((Ipv4Addr::LOCALHOST, port)).is_ok());
            drop(retained_listener);
            continue;
        }
        assert!(retirement.passed, "{retirement:?}");
        assert!(server.has_exited().unwrap());
        assert_eq!(retirement.adb_server.as_ref().unwrap().port, server_port);
        assert!(retirement.resources.contains_key("adb"));
        assert_eq!(retirement.forwarding["private"].removal_confirmed, !lost);
        assert!(!root.path().join("phases").exists(), "no ADB inventory after target/server retirement");
        assert!(matches!(retirement.forwarding["private"].disposition, ForwardDisposition::OwnedServerRetired));
        assert!(!root.path().join("mapping").exists());
        assert!(matches!(inspect(&root.path().join("group"), &target, &config.ports, None).unwrap(), Inspection::Finished(_)));
        let mut changed = retirement.adb_server.unwrap(); changed.port = 5037;
        save_atomic(&root.path().join("group/adb-server.json"), &changed).unwrap();
        assert!(inspect(&root.path().join("group"), &target, &config.ports, None).is_err());
    }
}

#[test]
#[cfg(target_os = "macos")]
fn resource_group_confines_private_forward_bind_and_cleanup_to_owned_scratch() {
    let root = tempfile::tempdir().unwrap();
    let config = forwarding_config(&root.path().canonicalize().unwrap());
    let adb = fake_adb(root.path());
    let script = std::fs::read_to_string(&adb).unwrap().replace("cd \"$(dirname \"$0\")\" || exit 2", &format!(
        "cd \"$HOME\" || exit 2\nif printf forbidden > '{}'; then exit 9; fi\nprintf allowed > \"$TMPDIR/client-write\" || exit 8", root.path().join("forbidden").display()));
    std::fs::write(&adb, script).unwrap();
    let mut group = start(root.path(), &config);
    let storage = group.ready.temporary_disks.canonicalize().unwrap();
    std::fs::create_dir(storage.join("adb-home")).unwrap();
    std::fs::create_dir(storage.join("adb-temporary")).unwrap();
    let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let server_port = reservation.local_addr().unwrap().port(); drop(reservation);
    let mut server = Command::new("/bin/sleep"); server.arg("300");
    group.start_adb_server(RequestId::new("adb").unwrap(), server_port, &server,
        Instant::now() + Duration::from_secs(5), &AtomicBool::new(false)).unwrap();
    let process = spawn(&mut group, "emulator", &config);
    let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    for (id, write_root) in [("foreign", root.path().canonicalize().unwrap()), ("private", storage.clone())] {
        let reply = group.execute(Operation::RegisterForward { id: RequestId::new(id).unwrap(),
            registration: ForwardRegistration { adb: adb.clone(), server_port: Some(server_port),
                write_root: Some(write_root), emulator: process.clone(), serial: format!("emulator-{}", config.ports[0]),
                socket: "webview_devtools_remote_123".into(), port, bind_evidence: format!("bind-{id}").into() },
        }, Instant::now() + Duration::from_secs(5)).unwrap();
        if id == "foreign" { assert!(matches!(reply, Reply::Failed { .. })); }
        else { assert!(matches!(reply, Reply::ForwardRegistered)); }
    }
    let mut command = forward_command(&adb, &config, port, true);
    command.env("ADB_SERVER_SOCKET", format!("tcp:127.0.0.1:{server_port}"));
    let mut wrong = Command::new(&adb);
    wrong.env("ADB_SERVER_SOCKET", "tcp:127.0.0.1:5037");
    assert!(capture::confined_adb(&wrong, &storage, server_port).is_err());
    let reply = group.execute(Operation::Capture {
        id: RequestId::new("confined-bind").unwrap(),
        launch: Launch { program: adb.clone(), arguments: command.get_args().map(|arg| arg.to_str().unwrap().into()).collect(),
            environment: BTreeMap::from([("ADB_SERVER_SOCKET".into(), format!("tcp:127.0.0.1:{server_port}"))]),
            remove_environment: vec![], directory: root.path().canonicalize().unwrap(), deadline_ns: config.deadline_ns },
        evidence: "bind-private".into(), output_limit: 4096, request: None, stdout_file: None, confine_adb: true,
    }, Instant::now() + Duration::from_secs(10)).unwrap();
    let Reply::Captured { evidence } = reply else { panic!("confined bind did not complete") };
    let exchange: crate::process::Exchange = crate::process::load_exchange(&evidence).unwrap().unwrap();
    assert!(exchange.failure.is_none());
    assert!(storage.join("adb-home/mapping").exists());
    assert_eq!(std::fs::read(storage.join("adb-temporary/client-write")).unwrap(), b"allowed");
    drop(reservation);
    let retirement = group.stop(Instant::now() + Duration::from_secs(15)).unwrap();
    assert!(retirement.passed, "{retirement:?}");
    assert!(retirement.forwarding["private"].removal_confirmed);
    assert!(!root.path().join("forbidden").exists());
    assert!(!storage.exists());
    assert!(matches!(inspection::inspect(&root.path().join("group"), &config.target, &config.ports, None).unwrap(),
        inspection::Inspection::Finished(_)));
}

#[test]
fn resource_group_forward_cleanup_uses_registered_server_and_preserves_shared_mapping() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap(); let config = forwarding_config(root.path());
    let adb = fake_adb(root.path());
    let original = std::fs::read_to_string(&adb).unwrap();
    let routed = original.replace("case \"$4\" in", r#"case "$ADB_SERVER_SOCKET" in
  tcp:127.0.0.1:15037) cd private || exit 3 ;;
  '') cd shared || exit 3 ;;
  *) exit 4 ;;
esac
case "$4" in"#);
    std::fs::write(&adb, routed).unwrap();
    std::fs::set_permissions(&adb, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::create_dir(root.path().join("private")).unwrap();
    std::fs::create_dir(root.path().join("shared")).unwrap();
    let shared = root.path().join("shared/mapping");
    std::fs::write(&shared, "foreign-device tcp:1234 localabstract:foreign\n").unwrap();
    let mut group = start(root.path(), &config);
    let process = spawn(&mut group, "emulator", &config);
    let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    let reply = group.execute(Operation::RegisterForward { id: RequestId::new("private").unwrap(),
        registration: ForwardRegistration { adb: adb.clone(), server_port: Some(15037), write_root: None,
            emulator: process, serial: format!("emulator-{}", config.ports[0]),
            socket: "webview_devtools_remote_123".into(), port, bind_evidence: "bind-private".into() },
    }, Instant::now() + Duration::from_secs(5)).unwrap();
    assert!(matches!(reply, Reply::ForwardRegistered));
    let mut command = forward_command(&adb, &config, port, true);
    command.env("ADB_SERVER_SOCKET", "tcp:127.0.0.1:15037");
    assert!(capture_forward(&mut group, &command, "bind-private").failure.is_none());
    assert!(root.path().join("private/mapping").exists());
    drop(reservation);
    let retirement = group.stop(Instant::now() + Duration::from_secs(15)).unwrap();
    assert!(retirement.passed, "{retirement:?}");
    assert!(retirement.forwarding["private"].removal_confirmed);
    assert!(!root.path().join("private/mapping").exists());
    assert_eq!(std::fs::read_to_string(shared).unwrap(), "foreign-device tcp:1234 localabstract:foreign\n");
}

#[test]
fn resource_group_forward_rejects_bind_to_a_different_server_before_effect() {
    let root = tempfile::tempdir().unwrap(); let config = forwarding_config(root.path());
    let adb = fake_adb(root.path()); let mut group = start(root.path(), &config);
    let process = spawn(&mut group, "emulator", &config);
    let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    let reply = group.execute(Operation::RegisterForward { id: RequestId::new("private").unwrap(),
        registration: ForwardRegistration { adb: adb.clone(), server_port: Some(15037), write_root: None,
            emulator: process, serial: format!("emulator-{}", config.ports[0]),
            socket: "webview_devtools_remote_123".into(), port, bind_evidence: "bind-private".into() },
    }, Instant::now() + Duration::from_secs(5)).unwrap();
    assert!(matches!(reply, Reply::ForwardRegistered));
    let mut command = forward_command(&adb, &config, port, true);
    command.env("ADB_SERVER_SOCKET", "tcp:127.0.0.1:15038");
    // Rejection can retire the group before capture returns; the fixture must not
    // keep an unrelated listener on the very port whose absence cleanup proves.
    drop(reservation);
    assert!(group.capture(&command, &group.ready.target.join("bind-private"),
        Instant::now() + Duration::from_secs(5), &AtomicBool::new(false), 4096).is_err());
    let retirement = group.stop(Instant::now() + Duration::from_secs(15)).unwrap();
    assert!(retirement.passed, "{retirement:?}");
    assert!(!root.path().join("mapping").exists());
    assert!(!retirement.forwarding["private"].binding_confirmed);
}

#[test]
fn resource_group_forward_removes_confirmed_binding_and_tracks_port_successor() {
    let root = tempfile::tempdir().unwrap(); let config = forwarding_config(root.path());
    let adb = fake_adb(root.path()); let mut group = start(root.path(), &config);
    let process = spawn(&mut group, "emulator", &config);
    let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    assert!(matches!(register_forward(&mut group, &config, &process, &adb, port, "first"), Reply::ForwardRegistered));
    // A second owner cannot claim this local endpoint before confirmed removal.
    assert!(matches!(register_forward(&mut group, &config, &process, &adb, port, "premature"), Reply::Failed { .. }));
    drop(reservation);
    assert!(capture_forward(&mut group, &forward_command(&adb, &config, port, true), "bind-first").failure.is_none());
    let original = std::fs::read(group.ready.target.join("bind-first/exchange.json")).unwrap();
    assert!(capture_forward(&mut group, &forward_command(&adb, &config, port, false), "remove-first").failure.is_none());
    assert!(matches!(register_forward(&mut group, &config, &process, &adb, port, "second"), Reply::ForwardRegistered));
    assert!(capture_forward(&mut group, &forward_command(&adb, &config, port, true), "bind-second").failure.is_none());
    let result = group.stop(Instant::now() + Duration::from_secs(15)).unwrap();
    assert!(result.passed, "{result:?}"); assert!(process.has_exited().unwrap());
    assert!(matches!(result.forwarding["first"].disposition, ForwardDisposition::RemovedBeforeSuccessor));
    assert!(matches!(result.forwarding["second"].disposition, ForwardDisposition::Absent));
    assert!(result.forwarding["second"].binding_confirmed && result.forwarding["second"].removal_confirmed);
    assert_eq!(std::fs::read_to_string(root.path().join("removals")).unwrap(), "removed\nremoved\n");
    assert_eq!(std::fs::read(root.path().join("target/bind-first/exchange.json")).unwrap(), original);
    assert!(host::acquire(&root.path().join("host")).is_ok());
}
#[test]
fn resource_group_forward_preserves_foreign_mapping_and_listener() {
    let root = tempfile::tempdir().unwrap(); let config = forwarding_config(root.path());
    let adb = fake_adb(root.path()); let mut group = start(root.path(), &config);
    let process = spawn(&mut group, "emulator", &config);
    let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let foreign = format!("other-device tcp:{port} localabstract:unrelated\n");
    std::fs::write(root.path().join("mapping"), &foreign).unwrap();
    assert!(matches!(register_forward(&mut group, &config, &process, &adb, port, "foreign"), Reply::ForwardRegistered));
    let exchange = capture_forward(&mut group, &forward_command(&adb, &config, port, true), "bind-foreign");
    assert_eq!(exchange.exit_code, Some(1)); assert!(exchange.failure.is_some());
    let result = group.stop(Instant::now() + Duration::from_secs(15)).unwrap();
    assert!(result.passed, "{result:?}");
    assert!(matches!(result.forwarding["foreign"].disposition, ForwardDisposition::ForeignMappingPreserved));
    assert!(!result.forwarding["foreign"].binding_confirmed); assert!(!result.forwarding["foreign"].removal_confirmed);
    assert_eq!(std::fs::read_to_string(root.path().join("mapping")).unwrap(), foreign);
    assert!(!root.path().join("removals").exists());
    assert!(TcpStream::connect(listener.local_addr().unwrap()).is_ok());
    assert!(host::acquire(&root.path().join("host")).is_ok());
}
#[test]
fn forward_controller_entry() {
    let Some(root) = std::env::var_os(ROOT) else { return; };
    let root = PathBuf::from(root); let config = forwarding_config(&root); let adb = fake_adb(&root);
    let mut group = start(&root, &config); let process = spawn(&mut group, "emulator", &config);
    let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = reservation.local_addr().unwrap().port();
    assert!(matches!(register_forward(&mut group, &config, &process, &adb, port, "owned"), Reply::ForwardRegistered));
    drop(reservation);
    let _ = capture_forward(&mut group, &forward_command(&adb, &config, port, true), "bind-owned");
    save_atomic(&root.join("bound.json"), &process).unwrap();
    loop { std::thread::sleep(Duration::from_secs(1)); }
}
#[test]
fn resource_group_forward_controller_loss_cleans_confirmed_and_fences_uncertain_bind() {
    for uncertain in [false, true] {
        let root = tempfile::tempdir().unwrap();
        if uncertain { std::fs::write(root.path().join("uncertain"), b"before helper completion").unwrap(); }
        struct Controller(Child);
        impl Drop for Controller { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
        let mut controller = Controller(Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "resource_group::tests::forward_controller_entry", "--nocapture"])
            .env(ROOT, root.path()).stdout(Stdio::null()).spawn().unwrap());
        // File creation precedes printf's write. Interrupt only after the modeled
        // forwarding effect is complete, while its helper reply can still be absent.
        wait_for(|| std::fs::read_to_string(root.path().join("mapping")).ok()
            .filter(|value| value.ends_with('\n') && value.split_whitespace().count() == 3
                && value.split_whitespace().last() == Some("localabstract:webview_devtools_remote_123")));
        let helper: ProcessIdentity = wait_for(|| load(&root.path().join("target/bind-owned/process.json")).unwrap());
        if !uncertain { let _: ProcessIdentity = wait_for(|| load(&root.path().join("bound.json")).unwrap()); }
        controller.0.kill().unwrap(); controller.0.wait().unwrap();
        let result: Retirement = wait_for(|| load(&root.path().join("group/retirement.json")).unwrap());
        let forward = &result.forwarding["owned"];
        assert_eq!(result.passed, !uncertain, "{result:?}");
        assert_eq!(forward.binding_confirmed, !uncertain);
        assert_eq!(forward.removal_confirmed, !uncertain);
        assert_eq!(root.path().join("removals").exists(), !uncertain);
        assert_eq!(root.path().join("mapping").exists(), uncertain);
        assert!(helper.has_exited().unwrap());
        let resource = result.resources["emulator"].process.as_ref().unwrap();
        assert!(resource.has_exited().unwrap());
        let exchange: crate::process::Exchange = crate::process::load_exchange(&root.path().join("target/bind-owned/exchange.json")).unwrap().unwrap();
        assert_eq!(exchange.failure.is_some(), uncertain);
        let owner: ProcessIdentity = load(&root.path().join("group/owner.json")).unwrap().unwrap();
        wait_for(|| owner.has_exited().unwrap().then_some(()));
        assert_eq!(host::acquire(&root.path().join("host")).is_ok(), !uncertain);
        assert_eq!(root.path().join("target/emulator-temporary").exists(), uncertain);
    }
}

#[test]
fn simulator_controller_entry() {
    let Some(root) = std::env::var_os(ROOT) else { return; };
    let root = PathBuf::from(root); let config = config(&root); let mut group = start(&root, &config);
    let process = spawn(&mut group, "runner", &config);
    save_atomic(&root.join("runner.json"), &process).unwrap();
    let launch = load(&root.join("simulator-launch.json")).unwrap().unwrap();
    let reply = group.execute(Operation::AcquireSimulator { config: launch, deadline_ns: config.deadline_ns },
        Instant::now() + Duration::from_secs(15)).unwrap();
    assert!(matches!(reply, Reply::SimulatorAcquired { .. }));
    save_atomic(&root.join("simulator-acquired.json"), &reply).unwrap();
    loop { std::thread::sleep(Duration::from_secs(1)); }
}

#[test]
fn resource_group_simulator_controller_loss_retires_created_device_and_runners() {
    for mode in ["normal", "slow-boot"] {
        let fixture = crate::simulator::tests::Fixture::new(mode);
        let root = fixture.root.path();
        save_atomic(&root.join("simulator-launch.json"), &fixture.config).unwrap();
        struct Controller(Child);
        impl Drop for Controller { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
        let mut controller = Controller(Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "resource_group::tests::simulator_controller_entry", "--nocapture"])
            .env(ROOT, root).stdout(Stdio::null()).spawn().unwrap());
        let marker = if mode == "normal" { "simulator-acquired.json" } else { "boot-started" };
        wait_for(|| root.join(marker).exists().then_some(()));
        assert!(host::acquire(&root.join("host")).is_err());
        let runner: ProcessIdentity = load(&root.join("runner.json")).unwrap().unwrap();
        assert!(!runner.has_exited().unwrap());
        controller.0.kill().unwrap(); controller.0.wait().unwrap();
        let result: Retirement = wait_for(|| load(&root.join("group/retirement.json")).unwrap());
        assert!(result.passed, "{mode}: {result:?}");
        assert!(result.simulator.unwrap().passed); assert!(runner.has_exited().unwrap());
        assert_eq!(std::fs::read_to_string(root.join("devices.json")).unwrap(), "[]");
        let calls = std::fs::read_to_string(root.join("calls")).unwrap();
        assert_eq!(calls.lines().filter(|line| line.contains("\"create\"")).count(), 1);
        assert_eq!(calls.lines().filter(|line| line.contains("\"delete\"")).count(), 1);
        if mode == "slow-boot" { assert!(!root.join("simulator-acquired.json").exists()); }
        let owner: ProcessIdentity = load(&root.join("group/owner.json")).unwrap().unwrap();
        wait_for(|| owner.has_exited().unwrap().then_some(()));
        assert!(host::acquire(&root.join("host")).is_ok());
    }
}

#[test]
fn resource_group_simulator_normal_stop_and_unresolved_creation_preserve_authority() {
    for mode in ["normal", "lost-create", "foreign"] {
        let fixture = crate::simulator::tests::Fixture::new(mode);
        let root = fixture.root.path(); let config = config(root); let mut group = start(root, &config);
        let result = group.execute(Operation::AcquireSimulator { config: fixture.config.clone(), deadline_ns: config.deadline_ns },
            Instant::now() + Duration::from_secs(10));
        if mode == "normal" {
            assert!(matches!(result.unwrap(), Reply::SimulatorAcquired { .. }));
            // A second request cannot create a replacement or relinquish ownership of the first.
            let reply = group.execute(Operation::AcquireSimulator { config: fixture.config.clone(), deadline_ns: config.deadline_ns },
                Instant::now() + Duration::from_secs(5)).unwrap();
            assert!(matches!(reply, Reply::Failed { .. }));
            assert!(!load::<HostSession>(&root.join("host/native-session.json")).unwrap().unwrap().retired);
        } else { assert!(result.is_err()); }
        let result = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
        assert_eq!(result.passed, mode == "normal", "{mode}: {result:?}");
        assert_eq!(result.simulator.unwrap().passed, mode == "normal");
        let calls = std::fs::read_to_string(root.join("calls")).unwrap();
        assert_eq!(calls.lines().filter(|line| line.contains("\"create\"")).count(), 1);
        assert_eq!(calls.lines().any(|line| line.contains("\"delete\"")), mode == "normal");
        assert_eq!(host::acquire(&root.join("host")).is_ok(), mode == "normal");
    }
}

#[test]
fn resource_group_environment_and_typed_stdin_preserve_original_exchanges() {
    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct Message { value: String }
    let root = tempfile::tempdir().unwrap(); let config = config(root.path());
    let mut command = Command::new("/bin/sh"); command.args(arguments("group_entry"))
        .env(ROOT, root.path()).env("FRONTCAP_GROUP_REMOVE", "inherited");
    let mut group = Group::start(&mut command, &config, &root.path().join("group"), Instant::now() + Duration::from_secs(5)).unwrap();
    let mut bridge = Command::new("/bin/sh");
    bridge.args(["-c", "test -z \"${FRONTCAP_GROUP_REMOVE+x}\" && test \"$FRONTCAP_GROUP_KEEP\" = explicit && cat"])
        .env_remove("FRONTCAP_GROUP_REMOVE").env("FRONTCAP_GROUP_KEEP", "explicit");
    let request = Message { value: "quoted \" text and newline\nremain original".into() };
    let evidence = group.ready.target.join("bridge");
    let result: crate::process::Exchange<Message> = group.exchange(&bridge, &request, &evidence,
        Instant::now() + Duration::from_secs(5), &AtomicBool::new(false), 4096).unwrap();
    assert!(result.failure.is_none(), "{result:?}"); assert_eq!(result.reply, Some(request));
    let original = std::fs::read(evidence.join("exchange.json")).unwrap();
    let mut leaf = Command::new("/bin/sh");
    leaf.args(["-c", "test -z \"${FRONTCAP_GROUP_REMOVE+x}\" && printf removed; printf ready > \"$1\"; exec /bin/sleep 300", "runner"]).arg(root.path().join("runner-ready"))
        .env_remove("FRONTCAP_GROUP_REMOVE");
    let process = group.start_resource(RequestId::new("runner").unwrap(), &leaf,
        Instant::now() + Duration::from_secs(5), &AtomicBool::new(false)).unwrap();
    wait_for(|| root.path().join("runner-ready").exists().then_some(()));
    let result = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(result.passed, "{result:?}"); assert!(process.has_exited().unwrap());
    assert_eq!(std::fs::read(evidence.join("exchange.json")).unwrap(), original);
    assert_eq!(std::fs::read(root.path().join("group/resources/runner/stdout.bin")).unwrap(), b"removed");
}

#[test]
fn exchange_controller_entry() {
    let Some(root) = std::env::var_os(ROOT) else { return; };
    let root = PathBuf::from(root); let config = config(&root); let mut group = start(&root, &config);
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "cat > \"$1\"; printf '{'; printf ready > \"$2\"; exec /bin/sleep 300", "bridge"])
        .arg(root.join("effect")).arg(root.join("effect-ready"));
    let _: crate::process::Exchange<String> = group.exchange(&command, &"original request", &group.ready.target.join("bridge"),
        Instant::now() + Duration::from_secs(15), &AtomicBool::new(false), 4096).unwrap();
    panic!("controller must be killed during its bridge exchange");
}

#[test]
fn resource_group_controller_loss_preserves_stdin_effect_and_partial_reply() {
    let root = tempfile::tempdir().unwrap();
    struct Controller(Child);
    impl Drop for Controller { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let mut controller = Controller(Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "resource_group::tests::exchange_controller_entry", "--nocapture"])
        .env(ROOT, root.path()).stdout(Stdio::null()).spawn().unwrap());
    wait_for(|| root.path().join("effect-ready").exists().then_some(()));
    let helper: ProcessIdentity = wait_for(|| load(&root.path().join("target/bridge/bridge-process.json")).unwrap());
    controller.0.kill().unwrap(); controller.0.wait().unwrap();
    let result: Retirement = wait_for(|| load(&root.path().join("group/retirement.json")).unwrap());
    assert!(result.passed, "{result:?}"); assert!(helper.has_exited().unwrap());
    let result: crate::process::Exchange<String> = crate::process::load_exchange(&root.path().join("target/bridge/exchange.json")).unwrap().unwrap();
    assert_eq!(result.failure.as_deref(), Some("bridge cancelled")); assert!(result.reply.is_none());
    assert_eq!(result.stdout, b"{");
    let effect = std::fs::read(root.path().join("effect")).unwrap();
    assert_eq!(serde_json::from_slice::<String>(&effect).unwrap(), "original request");
    assert_eq!(load::<String>(&root.path().join("target/bridge/call.json")).unwrap().unwrap(), "original request");
}

#[test]
fn resource_group_simulator_commands_keep_cursor_and_failed_effect_evidence() {
    let fixture = crate::simulator::tests::Fixture::new("normal");
    let root = fixture.root.path(); let config = config(root); let mut group = start(root, &config);
    let reply = group.execute(Operation::AcquireSimulator { config: fixture.config.clone(), deadline_ns: config.deadline_ns },
        Instant::now() + Duration::from_secs(10)).unwrap();
    let Reply::SimulatorAcquired { udid, command_number: before } = reply else { panic!("acquisition failed") };
    let call = |arguments: Vec<String>| Operation::SimulatorCommand { arguments, environment: BTreeMap::new(), deadline_ns: config.deadline_ns };
    let calls = std::fs::read(root.join("calls")).unwrap();
    for args in [vec!["create".into()], vec!["delete".into(), udid.clone()], vec!["listapps".into(), "foreign".into()]] {
        assert!(matches!(group.execute(call(args), Instant::now() + Duration::from_secs(5)).unwrap(), Reply::Failed { .. }));
    }
    assert_eq!(std::fs::read(root.join("calls")).unwrap(), calls);
    let reply = group.execute(call(vec!["get_app_container".into(), udid, "dev.frontcap.app".into(), "data".into()]),
        Instant::now() + Duration::from_secs(5)).unwrap();
    let Reply::SimulatorCommandCompleted { command_number, evidence } = reply else { panic!("missing original exchange") };
    assert_eq!(command_number, before + 1);
    let exchange: crate::process::Exchange = crate::process::load_exchange(&evidence).unwrap().unwrap();
    assert_eq!(exchange.exit_code, Some(1)); assert!(exchange.failure.is_some());
    let original = std::fs::read(&evidence).unwrap();
    let result = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
    assert!(result.passed, "{result:?}"); assert!(result.simulator.unwrap().passed);
    assert_eq!(std::fs::read(&evidence).unwrap(), original);
    assert_eq!(result.helper_evidence.len(), 1);
}

#[test]
fn resource_group_supervised_simulator_transfers_runner_and_preserves_command_evidence() {
    let fixture = crate::simulator::tests::Fixture::new("normal");
    let runner = crate::xctest::tests::config(&fixture);
    crate::xctest::tests::prepare_ready_runner(&fixture, &runner);
    let root = fixture.root.path(); let config = config(root); let group = start(root, &config);
    let end = Instant::now() + Duration::from_secs(15); let cancel = AtomicBool::new(false);
    let simulator = crate::simulator::Simulator::acquire_supervised(fixture.config.clone(), group, end, &cancel).unwrap();
    let mut first = crate::xctest::Xctest::on_simulator(runner.clone().into(), simulator, &root.join("target/first"), end, &cancel).unwrap();
    let first_ready = first.wait_ready(end, &cancel).unwrap();
    let original = std::fs::read(root.join("target/first/discovery-1/exchange.json")).unwrap();
    let simulator = first.release_simulator().unwrap();
    assert!(first_ready.process.has_exited().unwrap());
    assert!(!load::<HostSession>(&root.join("host/native-session.json")).unwrap().unwrap().retired);
    let mut second = crate::xctest::Xctest::on_simulator(runner.into(), simulator, &root.join("target/second"), end, &cancel).unwrap();
    let second_ready = second.wait_ready(end, &cancel).unwrap();
    assert_ne!(first_ready.process, second_ready.process);
    assert!(second.stop().passed); assert!(second_ready.process.has_exited().unwrap());
    assert_eq!(std::fs::read(root.join("target/first/discovery-1/exchange.json")).unwrap(), original);
    let result: Retirement = load(&root.join("group/retirement.json")).unwrap().unwrap();
    assert!(result.passed, "{result:?}"); assert_eq!(result.resources.len(), 2);
    assert!(!root.join("target/first/xctest.log").exists());
    assert!(result.helper_evidence.len() >= 4);
}

#[test]
fn supervised_ios_controller_entry() {
    let Some(root) = std::env::var_os(ROOT) else { return; };
    let root = PathBuf::from(root).canonicalize().unwrap();
    let lock = File::options().read(true).write(true).open(root.join("session.lock")).unwrap(); lock.lock().unwrap();
    let config = config(&root);
    let mut executable = Command::new("/bin/sh"); executable.args(arguments("group_entry")).env(ROOT, &root);
    let group = Group::start(&mut executable, &config, &root.join("target-resources"), Instant::now() + Duration::from_secs(5)).unwrap();
    let end = Instant::now() + Duration::from_secs(15); let cancel = AtomicBool::new(false);
    let launch = load(&root.join("simulator-launch.json")).unwrap().unwrap();
    let simulator = crate::simulator::Simulator::acquire_supervised(launch, group, end, &cancel).unwrap();
    let app_config = load(&root.join("app-launch.json")).unwrap().unwrap();
    let mut app = crate::simulator_app::App::launch(simulator, app_config, &root.join("target/app"), end, &cancel).unwrap();
    let runner = load(&root.join("runner-launch.json")).unwrap().unwrap();
    let ready = app.attach_xctest(runner, end, &cancel).unwrap();
    save_atomic(&root.join("controller-ready.json"), &ready).unwrap();
    loop { std::thread::sleep(Duration::from_secs(1)); }
}

#[test]
fn resource_group_ios_app_worker_loss_retires_runner_and_recovers_only_resource_outcome() {
    let (fixture, app) = crate::simulator_app::tests::setup("normal");
    let mut runner = crate::xctest::tests::config(&fixture); runner.app.bundle = app.bundle.clone();
    crate::xctest::tests::prepare_ready_runner(&fixture, &runner);
    let root = fixture.root.path();
    let device = root.join("ABCD0000-0000-0000-0000-000000000001");
    let consumer = device.join("consumer"); std::fs::create_dir(&consumer).unwrap();
    std::fs::write(root.join("consumer-container-path"), consumer.to_str().unwrap()).unwrap();
    std::fs::write(root.join("container-path"), device.join("container").to_str().unwrap()).unwrap();
    let sdk = std::fs::read_to_string(&fixture.config.xcrun).unwrap().replace("pointer=p/'container-path'",
        "pointer=p/('consumer-container-path' if args[3]=='dev.frontcap.consumer' else 'container-path')");
    std::fs::write(&fixture.config.xcrun, sdk).unwrap();
    save_atomic(&root.join("simulator-launch.json"), &fixture.config).unwrap();
    save_atomic(&root.join("app-launch.json"), &app).unwrap();
    save_atomic(&root.join("runner-launch.json"), &crate::xctest::RunnerLaunch::from(runner)).unwrap();
    let config = crate::webview_session::Config { lifetime_ms: 900_000, simulator: fixture.config.clone(),
        app, contacts:false, system_ui:None, lifecycle:None, service_url_environment:None };
    save_atomic(&root.join("launch.json"), &serde_json::json!({"host":false,"config":config,"deadline_ns":1})).unwrap();
    File::create_new(root.join("session.lock")).unwrap();
    struct Controller(Child);
    impl Drop for Controller { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let mut controller = Controller(Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "resource_group::tests::supervised_ios_controller_entry", "--nocapture"])
        .env(ROOT, root).stdout(Stdio::null()).spawn().unwrap());
    save_atomic(&root.join("worker.json"), &ProcessIdentity::of(controller.0.id()).unwrap().unwrap()).unwrap();
    let ready: crate::xctest::UiReady = wait_for(|| load(&root.join("controller-ready.json")).unwrap());
    assert!(!ready.process.has_exited().unwrap());
    controller.0.kill().unwrap(); controller.0.wait().unwrap();
    let result: Retirement = wait_for(|| load(&root.join("target-resources/retirement.json")).unwrap());
    assert!(result.passed, "{result:?}"); assert!(ready.process.has_exited().unwrap());
    let owner: ProcessIdentity = load(&root.join("target-resources/owner.json")).unwrap().unwrap();
    wait_for(|| owner.has_exited().unwrap().then_some(()));
    let original = std::fs::read(root.join("target-resources/retirement.json")).unwrap();
    for _ in 0..2 {
        assert!(matches!(crate::webview_session::inspect(root).unwrap(), crate::webview_session::Recovery::WorkerLost { retirement, .. } if retirement.passed));
    }
    assert!(!root.join("result.json").exists());
    assert_eq!(std::fs::read(root.join("target-resources/retirement.json")).unwrap(), original);
    let proof = root.join("target/simulator/retirement.json");
    save_atomic(&proof, &frontcap_core::native::ownership::Retirement { passed:false, errors:vec!["changed".into()], signal_error:None }).unwrap();
    assert!(crate::webview_session::inspect(root).is_err());
}

#[test]
fn binary_helper_returns_file_reference_and_retirement_detects_changed_payload() {
    for tamper in [false, true] {
        let root = tempfile::tempdir().unwrap(); let config = config(root.path());
        let mut group = start(root.path(), &config);
        let evidence = group.ready.target.join("binary");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf binary; printf diagnostic >&2"]);
        let result = group.capture_artifact(&command, &evidence, "screen.png",
            Instant::now()+Duration::from_secs(5), &AtomicBool::new(false), 4096).unwrap();
        assert!(result.exchange.failure.is_none());
        let path = evidence.join("screen.png");
        assert_eq!(result.read_stdout(&path).unwrap(), b"binary");
        assert_eq!(result.exchange.stderr, b"diagnostic");
        let original = std::fs::read(evidence.join("exchange.json")).unwrap();
        if tamper { std::fs::write(&path, b"edited").unwrap(); }
        let retirement = group.stop(Instant::now()+Duration::from_secs(10)).unwrap();
        assert_eq!(retirement.passed, !tamper, "{retirement:?}");
        assert_eq!(retirement.helper_evidence.len(), 2);
        assert_eq!(std::fs::read(evidence.join("exchange.json")).unwrap(), original);
    }
}

#[test]
fn artifact_exhaustion_preserves_diagnostics_original_evidence_and_stop() {
    let root = tempfile::tempdir().unwrap(); let mut config = config(root.path());
    config.artifact_budget_bytes = Some(5);
    let mut group = start(root.path(), &config);
    let evidence = group.ready.target.join("bounded");
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "printf 123456789; printf diagnostic >&2"]);
    let result = group.capture_artifact(&command, &evidence, "screen.png",
        Instant::now()+Duration::from_secs(5), &AtomicBool::new(false), 4096).unwrap();
    assert!(result.exchange.failure.is_some());
    assert_eq!(result.read_stdout(&evidence.join("screen.png")).unwrap(), b"12345");
    assert_eq!(result.exchange.stderr, b"diagnostic");
    let original = std::fs::read(evidence.join("exchange.json")).unwrap();
    let refused = group.ready.target.join("refused");
    let error = group.capture_artifact(&command, &refused, "screen.png",
        Instant::now()+Duration::from_secs(5), &AtomicBool::new(false), 4096).unwrap_err();
    assert!(error.to_string().contains("artifacts retained 5 bytes, remaining 0 bytes"));
    assert!(!refused.exists());
    let diagnostics = group.ready.target.join("diagnostics");
    let result = group.capture(&command, &diagnostics, Instant::now()+Duration::from_secs(5),
        &AtomicBool::new(false), 4096).unwrap();
    assert!(result.failure.is_none());
    assert_eq!(result.stdout, b"123456789");
    let retirement = group.stop(Instant::now()+Duration::from_secs(10)).unwrap();
    assert!(retirement.passed, "{retirement:?}");
    assert_eq!(std::fs::read(evidence.join("exchange.json")).unwrap(), original);
    assert_eq!(std::fs::read(evidence.join("screen.png")).unwrap(), b"12345");
}

#[test]
fn diagnostic_stream_tampering_invalidates_retirement_and_read_only_recovery() {
    for before_stop in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap(); let config = config(&root);
        let mut group = start(&root, &config);
        let evidence = group.ready.target.join("diagnostic");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf diagnostic"]);
        let result = group.capture(&command, &evidence,
            Instant::now()+Duration::from_secs(5), &AtomicBool::new(false), 4096).unwrap();
        assert_eq!(result.stdout, b"diagnostic");
        let path = evidence.join("stdout.bin");
        if before_stop { std::fs::write(&path, b"changed!!!").unwrap(); }
        let retirement = group.stop(Instant::now()+Duration::from_secs(10)).unwrap();
        assert_eq!(retirement.passed, !before_stop, "{retirement:?}");
        if !before_stop {
            assert!(matches!(inspection::inspect(&root.join("group"),
                &root.join("target"), &[], None).unwrap(), inspection::Inspection::Finished(_)));
            std::fs::write(&path, b"changed!!!").unwrap();
            assert!(inspection::inspect(&root.join("group"),
                &root.join("target"), &[], None).is_err());
        }
    }
}

/// Opt-in process-boundary qualification: observe real copied bytes, freeze only this test's
/// live child, revoke authority or let its monotonic deadline expire, then resume it.
#[test]
#[ignore = "explicit process-stop AVD interruption qualification"]
fn resource_group_avd_copy_interruption_retires_partial_scratch() {
    for authority_loss in [true, false] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let source = root.join("source");
        std::fs::create_dir_all(source.join("Phone.avd")).unwrap();
        std::fs::write(source.join("Phone.ini"), "path=/original/Phone.avd\n").unwrap();
        std::fs::write(source.join("Phone.avd/config.ini"), "disk.dataPartition.path=<temp>\n").unwrap();
        let data = source.join("Phone.avd/data.img");
        File::create_new(&data).unwrap().set_len(16 * 1024 * 1024).unwrap();
        let inventory = crate::products::BundleInventory::capture(&source).unwrap();
        let mut config = config(&root);
        config.deadline_ns = monotonic_ns().unwrap() + 8_000_000_000;
        let mut group = start(&root, &config);
        let scratch = group.ready.temporary_disks.clone();
        let partial = scratch.join("avd/Phone.avd/data.img");
        save_atomic(&group.directory.join("commands/1.json"), &Operation::PrepareAvd {
            source: source.clone(), name: "Phone".into(), inventory: inventory.clone(),
        }).unwrap();
        wait_for(|| std::fs::metadata(&partial).ok().filter(|m| m.len() > 0));
        let pid = group.child.as_ref().unwrap().id() as libc::pid_t;
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        // Always resume this live owned child, including if a subsequent assertion unwinds.
        struct Resume(libc::pid_t);
        impl Drop for Resume {
            fn drop(&mut self) { unsafe { libc::kill(self.0, libc::SIGCONT); } }
        }
        let resume = Resume(pid);
        let copied = std::fs::metadata(&partial).unwrap().len();
        assert!(copied > 0 && copied < inventory.files["Phone.avd/data.img"].bytes);
        assert!(!config.target.join("avd-copy.json").exists());
        if authority_loss {
            group.authority.take();
        } else {
            while monotonic_ns().unwrap() < config.deadline_ns {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        drop(resume);
        let reply: Reply = wait_for(|| load(&group.directory.join("replies/1.json")).unwrap());
        let Reply::Failed { error } = reply else { panic!("interrupted copy published success") };
        assert!(error.contains(if authority_loss { "authority revoked" } else { "deadline exceeded" }), "{error}");
        let retirement = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
        assert!(retirement.passed, "{retirement:?}");
        assert!(!scratch.exists());
        assert!(!config.target.join("avd-copy.json").exists());
        inventory.verify(&source).unwrap();
        assert!(load::<HostSession>(&root.join("host/native-session.json")).unwrap().unwrap().retired);
        assert!(host::acquire(&root.join("host")).is_ok());
        eprintln!("AVD interruption: authority_loss={authority_loss}, observed_partial_bytes={copied}, error={error}, retirement=passed, source=unchanged, fence=retired");
    }
}

#[test]
fn managed_logging_entry() {
    let Some(root) = std::env::var_os("FRONTCAP_MANAGED_LOG_GROUP") else { return; };
    let base = PathBuf::from(root);
    for lose_owner in [false, true] {
        let root = base.join(if lose_owner { "owner-loss" } else { "normal" });
        std::fs::create_dir(&root).unwrap();
        let mut config = config(&root);
        config.scratch_volume = Some(volume_fixture(&root));
        let mut group = start(&root, &config);
        let process = spawn(&mut group, "emulator", &config);
        save_atomic(&root.join("group/scratch-volume/worker/watch.json"), &process).unwrap();
        let scratch = group.ready.temporary_disks.clone();
        if lose_owner {
            let mut child = group.child.take().unwrap(); child.kill().unwrap(); child.wait().unwrap();
            drop(group);
            wait_for(|| process.has_exited().unwrap().then_some(()));
            wait_for(|| root.join("group/scratch-volume/worker/abandoned.json").exists().then_some(()));
            assert!(!root.join("group/retirement.json").exists());
            assert!(scratch.exists()); // Lost owner cannot attest deletion or release its fence.
            assert!(host::acquire(&root.join("host")).is_err());
        } else {
            assert!(group.stop(Instant::now() + Duration::from_secs(10)).unwrap().passed);
            assert!(process.has_exited().unwrap()); assert!(!scratch.exists());
        }
        for name in ["group/group.log.diagnostics", "group/resources/emulator/supervisor.log.diagnostics",
            "group/scratch-volume/worker.log.diagnostics"] {
            let directory = std::fs::read_dir(root.join(name)).unwrap().next().unwrap().unwrap().path();
            let result: crate::diagnostic_log::Summary = wait_for(|| load(&directory.join("result.json")).unwrap());
            assert!(result.eof && result.error.is_none(), "{name}: {result:?}");
            let collector: ProcessIdentity = load(&directory.join("ready.json")).unwrap().unwrap();
            wait_for(|| collector.has_exited().unwrap().then_some(()));
        }
    }

}

#[test]
fn managed_group_and_resource_logs_finish_after_owned_retirement() {
    let temporary = tempfile::tempdir().unwrap(); let root = temporary.path().canonicalize().unwrap();
    let collector = root.join("collector.sh");
    // Trusted test executable route for the same private mode used by the real recorder.
    std::fs::write(&collector, format!("#!/bin/sh\nexport FRONTCAP_DIAGNOSTIC_TEST_DIRECTORY=\"$2\"\nexec '{}' --exact diagnostic_log::tests::entry --nocapture\n",
        std::env::current_exe().unwrap().display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&collector, std::fs::Permissions::from_mode(0o700)).unwrap();
    let status = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "resource_group::tests::managed_logging_entry", "--nocapture"])
        .env("FRONTCAP_MANAGED_LOG_GROUP", &root)
        .env(crate::diagnostic_log::RECORDER_ENV, &collector).status().unwrap();
    assert!(status.success());
}

#[cfg(unix)]
pub(crate) fn release_while_child_retains_descriptor(release: impl FnOnce()) {
    let mut pipe = [0; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        // No Rust runtime, allocation or destructors after fork in a multithreaded test process.
        unsafe {
            libc::close(pipe[1]);
            let mut byte = 0u8;
            libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
            libc::_exit(0);
        }
    }
    unsafe {
        libc::close(pipe[0]);
    }
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(release));
    unsafe {
        let byte = 1u8;
        libc::write(pipe[1], (&byte as *const u8).cast(), 1);
        libc::close(pipe[1]);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    if let Err(error) = observed { std::panic::resume_unwind(error); }
}

#[test]
fn finished_inspection_releases_lock_while_child_retains_descriptor() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path();
    let config = config(directory);
    save_atomic(&directory.join("config.json"), &config).unwrap();
    File::create(directory.join("group.lock")).unwrap();
    let lease = inspection::lock(directory).unwrap().unwrap();
    release_while_child_retains_descriptor(|| {
        drop(lease);
        match inspection::inspect(directory, &config.target, &config.ports, None).unwrap() {
            inspection::Inspection::Unresolved => (),
            inspection::Inspection::Active => panic!("completed inspection retained its lock in an unrelated child"),
            inspection::Inspection::Finished(_) => panic!("absent retirement proof was accepted"),
        }
    });
}

#[path = "resource_group_forward_budget_tests.rs"]
mod forward_budget;

#[test]
fn registered_bundle_owner_loss_preserves_host_fence_after_owned_cleanup() {
    for missing_owner in [false, true] {
        let fixture = crate::simulator::tests::Fixture::new("normal");
        let root = fixture.root.path();
        let config = config(root);
        let mut group = start(root, &config);
        let reply = group.execute(Operation::AcquireSimulator {
            config: fixture.config.clone(), deadline_ns: config.deadline_ns,
        }, Instant::now() + Duration::from_secs(10)).unwrap();
        assert!(matches!(reply, Reply::SimulatorAcquired { .. }));
        let process = spawn(&mut group, "sdk-writer", &config);
        let owner = crate::xctest::bundle_volume::tests::retain_owner(&group.ready.target);
        if missing_owner {
            // Keep original retained proof, but remove the registered pathname from discovery.
            std::fs::rename(&owner, group.ready.target.join("hidden-owner")).unwrap();
        }
        let result = group.stop(Instant::now() + Duration::from_secs(10)).unwrap();
        assert!(process.has_exited().unwrap());
        assert!(result.simulator.as_ref().unwrap().passed);
        assert_eq!(std::fs::read_to_string(root.join("devices.json")).unwrap(), "[]");
        assert_eq!(result.passed, !missing_owner, "{result:?}");
        assert_eq!(host::acquire(&root.join("host")).is_ok(), !missing_owner);
    }
}

#[test]
#[cfg(target_os = "macos")]
#[ignore = "creates real Simulator devices and APFS images; requires explicit fresh output root and volume worker"]
fn simulator_data_volume_retires_after_stop_and_controller_eof() {
    use std::os::unix::fs::MetadataExt;
    let base = PathBuf::from(std::env::var_os("FRONTCAP_SIMULATOR_VOLUME_PROBE_ROOT").expect("fresh output root"));
    std::fs::create_dir(&base).unwrap(); let base = base.canonicalize().unwrap();
    for revoke in [false, true] {
        let root = base.join(if revoke { "controller-eof" } else { "normal-stop" });
        std::fs::create_dir(&root).unwrap();
        let mut config = config(&root);
        config.host = PathBuf::from(std::env::var_os("HOME").unwrap()).join(".cache/frontcap");
        config.deadline_ns = monotonic_ns().unwrap() + 240_000_000_000;
        let mut group = start(&root, &config);
        let launch = crate::simulator::SimulatorLaunch {
            data_volume_root: None,
            xcrun: "/usr/bin/xcrun".into(), runtime: "com.apple.CoreSimulator.SimRuntime.iOS-27-2".into(),
            device_type: "com.apple.CoreSimulator.SimDeviceType.iPhone-16".into(), source_sha256: "a".repeat(64),
            data_volume: Some(crate::storage_volume::Config {
                python: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PYTHON").expect("explicit Python")),
                worker: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_WORKER").expect("explicit volume worker")),
                size_mib: 8192, reserve_mib: 64,
            }),
        };
        let end = Instant::now() + Duration::from_secs(150);
        let reply = group.execute(Operation::AcquireSimulator { config: launch,
            deadline_ns: monotonic_ns().unwrap() + 150_000_000_000 }, end).unwrap();
        let Reply::SimulatorAcquired { udid, .. } = reply else { panic!("{reply:?}"); };
        let simulator = root.join("target/simulator");
        let relocation: serde_json::Value = load(&simulator.join("data-relocation.json")).unwrap().unwrap();
        let data = PathBuf::from(relocation["source"].as_str().unwrap());
        let destination = PathBuf::from(relocation["destination"].as_str().unwrap());
        assert!(data.is_symlink()); assert_eq!(data.canonicalize().unwrap(), destination);
        assert_ne!(std::fs::metadata(&data).unwrap().dev(), std::fs::metadata(&root).unwrap().dev());
        let process: ProcessIdentity = load(&root.join("group/owner.json")).unwrap().unwrap();
        if revoke { drop(group); } else { assert!(group.stop(Instant::now() + Duration::from_secs(75)).unwrap().passed); }
        let end = Instant::now() + Duration::from_secs(75);
        loop {
            if process.has_exited().unwrap() { break; }
            assert!(Instant::now() < end, "original group still alive"); std::thread::sleep(Duration::from_millis(50));
        }
        let inspection::Inspection::Finished(retired) = inspection::inspect(&root.join("group"), &config.target, &[], None).unwrap()
            else { panic!("original retirement not recoverable"); };
        assert!(retired.passed, "{retired:?}"); assert!(retired.simulator.unwrap().passed);
        assert!(!data.parent().unwrap().exists(), "created device {udid} remains");
        assert!(!simulator.join("data-volume/worker/volume/bounded.dmg").exists());
        assert!(load::<HostSession>(&config.host.join("native-session.json")).unwrap().unwrap().retired);
    }
}

#[test]
fn fresh_xctest_uses_original_group_for_runner_and_simulator_retirement() {
    let fixture = crate::simulator::tests::Fixture::new("normal");
    let root = fixture.root.path().canonicalize().unwrap();
    let launch = crate::xctest::tests::config(&fixture);
    let group = start(&root, &config(&root));
    let mut target = crate::xctest::Xctest::start_in_group(launch, &root.join("target"),
        Instant::now() + Duration::from_secs(10), &AtomicBool::new(false), group).unwrap();
    target.ensure_running().unwrap();
    let runner: ProcessIdentity = load(&root.join("target/runner-process.json")).unwrap().unwrap();
    let retirement = target.stop();
    assert!(retirement.passed, "{retirement:?}");
    assert!(runner.has_exited().unwrap());
    let group: Retirement = load(&root.join("group/retirement.json")).unwrap().unwrap();
    assert!(group.passed && group.simulator.unwrap().passed);
    assert!(group.resources.contains_key("runner-1"));
    let host: HostSession = load(&root.join("host/native-session.json")).unwrap().unwrap();
    assert!(host.retired);
    assert!(root.join("deleted").exists());
}

#[test]
fn resource_group_tcp_forward_removes_only_the_confirmed_exact_mapping() {
    for replacement in [None, Some("tcp:43211"), Some("localabstract:tcp:43210")] {
        let root = tempfile::tempdir().unwrap(); let config = forwarding_config(root.path());
        let adb = fake_adb(root.path()); let mut group = start(root.path(), &config);
        let process = spawn(&mut group, "emulator", &config);
        let reservation = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        let serial = format!("emulator-{}", config.ports[0]);
        let registration = |socket: &str| ForwardRegistration {
            adb: adb.clone(), server_port: None, write_root: None, emulator: process.clone(),
            serial: serial.clone(), socket: socket.into(), port, bind_evidence: "bind-tcp".into(),
        };
        // Reject ambiguous/dynamic destinations before publishing any cleanup authority.
        for socket in ["tcp:0", "tcp:65536", "tcp:043210", "tcp:+1", "tcp:-1", "tcp:localhost:1", "tcp:1\n"] {
            let rejected = root.path().join("rejected");
            assert!(forwarding::Forward::new(registration(socket), &group.ready.target, &rejected).is_err());
            assert!(!rejected.exists());
        }
        assert!(matches!(group.execute(Operation::RegisterForward {
            id: RequestId::new("tcp").unwrap(), registration: registration("tcp:43210"),
        }, Instant::now() + Duration::from_secs(5)).unwrap(), Reply::ForwardRegistered));
        let mut command = Command::new(&adb);
        command.args(["-s", &serial, "forward", "--no-rebind", &format!("tcp:{port}"), "tcp:43210"]);
        assert!(capture_forward(&mut group, &command, "bind-tcp").failure.is_none());
        let expected = format!("{serial} tcp:{port} {}\n", replacement.unwrap_or("tcp:43210"));
        if replacement.is_some() { std::fs::write(root.path().join("mapping"), &expected).unwrap(); }
        else { assert_eq!(std::fs::read_to_string(root.path().join("mapping")).unwrap(), expected); }
        drop(reservation);
        let result = group.stop(Instant::now() + Duration::from_secs(15)).unwrap();
        assert!(result.passed, "{result:?}");
        let forward = &result.forwarding["tcp"];
        assert!(forward.binding_confirmed);
        assert_eq!(forward.removal_confirmed, replacement.is_none());
        if replacement.is_some() {
            assert!(matches!(forward.disposition, ForwardDisposition::ForeignMappingPreserved));
            assert_eq!(std::fs::read_to_string(root.path().join("mapping")).unwrap(), expected);
            assert!(!root.path().join("removals").exists());
        } else {
            assert!(matches!(forward.disposition, ForwardDisposition::Absent));
            assert!(!root.path().join("mapping").exists());
            assert_eq!(std::fs::read_to_string(root.path().join("removals")).unwrap(), "removed\n");
        }
    }
}

#[test]
#[cfg(target_os = "macos")]
fn confined_emulator_requires_declared_ports_avd_and_mounted_scratch() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path().canonicalize().unwrap();
    let avd = root.join("avd"); std::fs::create_dir(&avd).unwrap();
    let arguments = ["-avd", "Phone", "-port", "5580", "-no-window", "-no-audio", "-no-snapshot", "-read-only"];
    for (ports, extra, expected) in [
        (vec![5580], vec![], "port pair"),
        (vec![5580, 5582], vec![], "port pair"),
        (vec![5582, 5583], vec![], "configuration"),
        (vec![5580, 5581], vec!["-avd", "Other"], "duplicate"),
        (vec![5580, 5581], vec!["-ports", "5582,5583"], "configuration"),
        (vec![5580, 5581], vec![], "mounted scratch"),
    ] {
        let launch = Launch { program: "/bin/false".into(),
            arguments: arguments.iter().copied().chain(extra).map(str::to_owned).collect(),
            environment: BTreeMap::new(), remove_environment: vec![], directory: root.clone(), deadline_ns: u64::MAX };
        let error = capture::confined_emulator(&launch, &root, "Phone", &avd, &ports, 15037, &root).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert!(!root.join("sdk-home").exists());
        assert!(!root.join("emulator-write-root.json").exists());
    }
}
