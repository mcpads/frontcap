use super::*;
const ROOT: &str = "FRONTCAP_RESOURCE_PROCESS_TEST";

fn supervisor(root: &Path) -> Command {
    // exec keeps the test entry itself as the supervisor; no shell owns its resource.
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "exec \"$1\" --exact resource_process::tests::supervisor_entry --nocapture", "resource-owner"])
        .arg(std::env::current_exe().unwrap()).env(ROOT, root);
    command
}
fn launch(root: &Path, lifetime: Duration) -> Launch {
    Launch { program: "/bin/sleep".into(), arguments: vec!["300".into()],
        environment: BTreeMap::new(), remove_environment: vec![], directory: root.into(),
        deadline_ns: monotonic_ns().unwrap() + lifetime.as_nanos() as u64 }
}
fn wait_for<T>(mut read: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = read() { return value; }
        assert!(Instant::now() < deadline, "resource test observation deadline");
        std::thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn supervisor_entry() {
    let Some(root) = std::env::var_os(ROOT) else { return; };
    if std::env::var_os("FRONTCAP_RESOURCE_EXPECT_FD9").is_some() {
        // SAFETY: read the deliberately injected test descriptor's flags without changing it.
        let flags = unsafe { libc::fcntl(9, libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC == 0, "missing inheritable control descriptor");
    }
    run(&PathBuf::from(root).join("resource")).unwrap();
}
#[test]
fn controller_entry() {
    let Some(root) = std::env::var_os(ROOT) else { return; };
    let root = PathBuf::from(root);
    let resource = ResourceProcess::start(&mut supervisor(&root), &launch(&root, Duration::from_secs(20)),
        &root.join("resource"), Instant::now() + Duration::from_secs(5)).unwrap();
    save_atomic(&root.join("controller-ready.json"), &resource.process).unwrap();
    loop { std::thread::sleep(Duration::from_secs(1)); }
}
#[test]
fn resource_supervisor_retires_after_controller_sigkill_without_recovered_pid_authority() {
    let root = tempfile::tempdir().unwrap();
    struct Controller(Child);
    impl Drop for Controller { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let mut controller = Controller(Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "resource_process::tests::controller_entry", "--nocapture"])
        .env(ROOT, root.path()).stdout(Stdio::null()).spawn().unwrap());
    let process: ProcessIdentity = wait_for(|| load(&root.path().join("controller-ready.json")).unwrap());
    let directory = root.path().join("resource");
    let owner: ProcessIdentity = load(&directory.join("supervisor.json")).unwrap().unwrap();
    assert!(!process.has_exited().unwrap());
    assert!(!owner.has_exited().unwrap());
    controller.0.kill().unwrap();
    controller.0.wait().unwrap();
    let result: ResultRecord = wait_for(|| load(&directory.join("result.json")).unwrap());
    assert_eq!(result.reason, "authority lost");
    assert!(result.retirement_error.is_none(), "{result:?}");
    assert!(process.has_exited().unwrap());
    wait_for(|| owner.has_exited().unwrap().then_some(()));
    let before = std::fs::read(directory.join("result.json")).unwrap();
    assert!(run(&directory).is_err()); // Cannot reacquire an existing resource attempt.
    assert_eq!(std::fs::read(directory.join("result.json")).unwrap(), before);
}
#[test]
fn resource_supervisor_stop_and_lifetime_retire_only_the_owned_child() {
    for expired in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let lifetime = Duration::from_secs(if expired { 2 } else { 20 });
        let mut unrelated = Command::new("/bin/sleep").arg("30").spawn().unwrap();
        let resource = ResourceProcess::start(&mut supervisor(root.path()), &launch(root.path(), lifetime),
            &root.path().join("resource"), Instant::now() + Duration::from_secs(5)).unwrap();
        let process = resource.process.clone();
        if expired {
            let result: ResultRecord = wait_for(|| load(&root.path().join("resource/result.json")).unwrap());
            assert_eq!(result.reason, "lifetime expired");
        }
        let result = resource.stop(Instant::now() + Duration::from_secs(5)).unwrap();
        let unrelated_alive = unrelated.try_wait().unwrap().is_none();
        unrelated.kill().unwrap(); unrelated.wait().unwrap();
        assert!(unrelated_alive);
        assert!(result.retirement_error.is_none(), "{result:?}");
        assert!(process.has_exited().unwrap());
        assert_eq!(result.reason, if expired { "lifetime expired" } else { "authority lost" });
    }
}

#[test]
fn output_entry() {
    let Some(root) = std::env::var_os(ROOT) else { return; };
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let output = [b'o'; 8192];
    let errors = [b'e'; 8192];
    for _ in 0..128 {
        stdout.write_all(&output).unwrap();
        stderr.write_all(&errors).unwrap();
    }
    std::fs::write(PathBuf::from(root).join("producer-ready"), b"both streams exceed retention").unwrap();
    loop {
        stdout.write_all(&output).unwrap();
        stderr.write_all(&errors).unwrap();
    }
}

#[test]
fn resource_supervisor_bounds_both_output_streams_without_starving_stop() {
    let root = tempfile::tempdir().unwrap();
    let mut request = launch(root.path(), Duration::from_secs(20));
    request.program = std::env::current_exe().unwrap();
    request.arguments = vec!["--exact".into(), "resource_process::tests::output_entry".into(), "--nocapture".into()];
    request.environment.insert(ROOT.into(), root.path().to_str().unwrap().into());
    let resource = ResourceProcess::start(&mut supervisor(root.path()), &request,
        &root.path().join("resource"), Instant::now() + Duration::from_secs(5)).unwrap();
    wait_for(|| root.path().join("producer-ready").exists().then_some(()));
    let result = resource.stop(Instant::now() + Duration::from_secs(5)).unwrap();
    assert_eq!(result.reason, "authority lost");
    assert!(result.retirement_error.is_none(), "{result:?}");
    for (name, output) in [("stdout.bin", result.stdout.unwrap()), ("stderr.bin", result.stderr.unwrap())] {
        assert_eq!(output.retained_bytes, OUTPUT_LIMIT);
        assert!(output.total_bytes >= 1024 * 1024);
        assert!(output.eof);
        assert!(output.failure.is_none(), "{output:?}");
        assert_eq!(std::fs::metadata(root.path().join("resource").join(name)).unwrap().len(), OUTPUT_LIMIT as u64);
    }
}

#[test]
fn inherited_file_authority_does_not_reach_the_resource() {
    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("outside");
    let baseline = Command::new("/bin/sh").args(["-c", "exec 9>\"$1\"; printf baseline >&9", "control"])
        .arg(&outside).status().unwrap();
    assert!(baseline.success());
    assert_eq!(std::fs::read(&outside).unwrap(), b"baseline");
    let mut owner = Command::new("/bin/sh");
    owner.args(["-c", "exec 9>\"$1/outside\"; exec \"$2\" --exact resource_process::tests::supervisor_entry --nocapture", "owner"])
        .arg(root.path()).arg(std::env::current_exe().unwrap()).env(ROOT, root.path())
        .env("FRONTCAP_RESOURCE_EXPECT_FD9", "1");
    let mut request = launch(root.path(), Duration::from_secs(20));
    request.program = "/bin/sh".into();
    request.arguments = vec!["-c".into(), "if (printf leaked >&9) 2>/dev/null; then exit 3; fi; printf ready > ready; exec /bin/sleep 300".into()];
    let resource = ResourceProcess::start(&mut owner, &request,
        &root.path().join("resource"), Instant::now() + Duration::from_secs(5)).unwrap();
    wait_for(|| root.path().join("ready").exists().then_some(()));
    let result = resource.stop(Instant::now() + Duration::from_secs(5)).unwrap();
    assert!(result.retirement_error.is_none(), "{result:?}");
    assert_eq!(std::fs::read(&outside).unwrap(), b"");
}
