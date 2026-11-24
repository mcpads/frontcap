use super::*;
const DIRECTORY: &str = "FRONTCAP_DIAGNOSTIC_TEST_DIRECTORY";
fn command() -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", &format!("export {DIRECTORY}=\"$2\"; exec \"$1\" --exact diagnostic_log::tests::entry --nocapture"), "log-test"])
        .arg(std::env::current_exe().unwrap());
    command
}
fn wait<T>(mut read: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = read() { return value; }
        assert!(Instant::now() < deadline, "diagnostic observation expired");
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn results(root: &Path, expected: usize) -> Vec<Summary> {
    wait(|| {
        let results = std::fs::read_dir(root).unwrap().map(|e|
            load::<Summary>(&e.unwrap().path().join("result.json")).unwrap()).collect::<Option<Vec<_>>>()?;
        (results.len() == expected).then_some(results)
    })
}
#[test]
fn entry() {
    if let Some(directory) = std::env::var_os(DIRECTORY) { run(Path::new(&directory)).unwrap(); }
}
#[test]
fn concurrent_collectors_share_the_retained_prefix_bound() {
    let temp = tempfile::tempdir().unwrap(); let root = temp.path().join("collectors");
    let log = temp.path().join("stderr.log"); std::fs::write(&log, b"original\n").unwrap();
    let mut writers = Vec::new();
    for _ in 0..2 {
        let stderr = start(&mut command(), &log, &root).unwrap();
        writers.push(Command::new("/usr/bin/python3").args(["-c", "import os; os.write(2,b'x'*(5*1024*1024))"])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr).spawn().unwrap());
    }
    for writer in &mut writers { assert!(writer.wait().unwrap().success()); }
    let reports = results(&root, 2);
    assert!(reports.iter().all(|r| r.eof && r.error.is_none()));
    assert_eq!(reports.iter().map(|r| r.observed_bytes).sum::<u64>(), 10 * 1024 * 1024);
    assert_eq!(reports.iter().map(|r| r.retained_bytes).sum::<u64>(), LIMIT - 9);
    let bytes = std::fs::read(&log).unwrap();
    assert_eq!(bytes.len() as u64, LIMIT); assert!(bytes.starts_with(b"original\n"));
}
#[test]
#[expect(clippy::zombie_processes, reason = "The parent test kills this controller and independently verifies writer exit; waiting here would remove the tested owner-loss boundary.")]
fn controller_entry() {
    let Some(root) = std::env::var_os("FRONTCAP_DIAGNOSTIC_CONTROLLER") else { return; };
    let root = PathBuf::from(root);
    let stderr = start(&mut command(), &root.join("stderr.log"), &root.join("collectors")).unwrap();
    let writer = Command::new("/usr/bin/python3").args(["-c", "import pathlib,os,time,sys\np=pathlib.Path(sys.argv[1])\nend=time.monotonic()+5\nwhile not (p/'continue').exists() and time.monotonic()<end: time.sleep(.01)\nos.write(2,b'after-controller-loss')\n(p/'done').touch()"])
        .arg(&root).stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr).process_group(0).spawn().unwrap();
    save_atomic(&root.join("writer.json"), &ProcessIdentity::of(writer.id()).unwrap().unwrap()).unwrap();
    loop { std::thread::sleep(Duration::from_secs(1)); }
}
#[test]
fn collector_survives_controller_loss_and_finishes_after_writer_eof() {
    let temp = tempfile::tempdir().unwrap(); let root = temp.path();
    struct Controller(std::process::Child);
    impl Drop for Controller { fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); } }
    let mut controller = Controller(Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "diagnostic_log::tests::controller_entry", "--nocapture"])
        .env("FRONTCAP_DIAGNOSTIC_CONTROLLER", root).stdout(Stdio::null()).spawn().unwrap());
    let writer: ProcessIdentity = wait(|| load(&root.join("writer.json")).unwrap());
    let directory = std::fs::read_dir(root.join("collectors")).unwrap().next().unwrap().unwrap().path();
    let collector: ProcessIdentity = load(&directory.join("ready.json")).unwrap().unwrap();
    controller.0.kill().unwrap(); controller.0.wait().unwrap();
    assert!(!collector.has_exited().unwrap()); assert!(!writer.has_exited().unwrap());
    std::fs::write(root.join("continue"), b"continue").unwrap();
    let report = results(&root.join("collectors"), 1).remove(0);
    assert!(report.eof && report.error.is_none());
    assert_eq!(std::fs::read(root.join("stderr.log")).unwrap(), b"after-controller-loss");
    wait(|| (collector.has_exited().unwrap() && writer.has_exited().unwrap()).then_some(()));
    assert!(root.join("done").exists());
}

#[test]
fn unavailable_log_lock_is_disclosed_without_breaking_the_writer_pipe() {
    let temp = tempfile::tempdir().unwrap(); let root = temp.path().join("collectors");
    let log = temp.path().join("stderr.log");
    let locked = File::create(&log).unwrap(); locked.lock().unwrap();
    let stderr = start(&mut command(), &log, &root).unwrap();
    let mut writer = Command::new("/usr/bin/python3")
        .args(["-c", "import os; os.write(2,b'x'*(5*1024*1024))"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr).spawn().unwrap();
    assert!(writer.wait().unwrap().success());
    let report = results(&root, 1).remove(0);
    assert!(report.eof && report.error.is_some());
    assert_eq!(report.observed_bytes, 5 * 1024 * 1024);
    assert_eq!(report.retained_bytes, 0);
    assert_eq!(std::fs::metadata(&log).unwrap().len(), 0);
}

#[test]
fn shared_budget_exhaustion_drains_all_writers_and_publishes_retirement() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let temp = tempfile::tempdir().unwrap();
    let budget = WriteBudget::create(&temp.path().join("budget"), LaunchId::new("g-logs").unwrap(),
        Limits { ordinary: 16384, retirement: 16384 }).unwrap();
    let root = temp.path().join("collectors");
    let mut writers = Vec::new();
    for n in 0..2 {
        let log = temp.path().join(format!("stderr-{n}.log"));
        std::fs::write(&log, b"old").unwrap();
        let stderr = start_with_budget(&mut command(), &log, &root, Some(&budget)).unwrap();
        writers.push(Command::new("/usr/bin/python3").args(["-c", "import os; os.write(2,b'x'*65536)"])
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(stderr).spawn().unwrap());
    }
    for writer in &mut writers { assert!(writer.wait().unwrap().success()); }
    let reports = results(&root, 2);
    assert!(reports.iter().all(|r| r.eof && r.error.as_ref().is_some_and(|e| e.contains("allowance exhausted"))));
    assert_eq!(reports.iter().map(|r| r.observed_bytes).sum::<u64>(), 131072);
    let retained = reports.iter().map(|r| r.retained_bytes).sum::<u64>();
    assert!(retained > 0 && retained <= 16384);
    assert_eq!(budget.charged().unwrap().ordinary, retained);
    assert!(budget.charged().unwrap().retirement > 0);
    for n in 0..2 {
        assert!(std::fs::read(temp.path().join(format!("stderr-{n}.log"))).unwrap().starts_with(b"old"));
    }
    for entry in std::fs::read_dir(&root).unwrap() {
        let identity: ProcessIdentity = load(&entry.unwrap().path().join("ready.json")).unwrap().unwrap();
        wait(|| identity.has_exited().unwrap().then_some(()));
    }
}

#[test]
fn failed_startup_distinguishes_no_collector_exited_collector_and_live_collector() {
    let temp = tempfile::tempdir().unwrap(); let root = temp.path();
    let no_worker = start_with_budget_outcome(&mut Command::new(root.join("missing")),
        &root.join("absent.log"), &root.join("absent"), None);
    assert!(no_worker.result.is_err());
    assert!(matches!(no_worker.cleanup, OutputCleanup::NoCollectorStarted));
    for hold in [false, true] {
        let case = root.join(if hold { "live" } else { "eof" }); std::fs::create_dir(&case).unwrap();
        struct Release(PathBuf);
        impl Drop for Release { fn drop(&mut self) { let _ = std::fs::write(&self.0, b"release"); } }
        let release = Release(case.join("release"));
        let mut command = Command::new("/usr/bin/python3");
        command.args(["-c", r#"import pathlib,sys,os,time
case=pathlib.Path(sys.argv[1]);directory=pathlib.Path(sys.argv[-1])
(case/'pid').write_text(str(os.getpid()))
(directory/'ready.json').write_text('malformed')
if sys.argv[2]=='true':
 end=time.monotonic()+20
 while not (case/'release').exists() and time.monotonic()<end:time.sleep(.01)
else:sys.stdin.buffer.read()
"#]).arg(&case).arg(if hold { "true" } else { "false" });
        let outcome = start_with_budget_outcome(&mut command, &case.join("stderr.log"), &case.join("collectors"), None);
        assert!(outcome.result.is_err());
        let pid: u32 = std::fs::read_to_string(case.join("pid")).unwrap().parse().unwrap();
        if hold {
            assert!(matches!(outcome.cleanup, OutputCleanup::Unproven));
            let process = ProcessIdentity::of(pid).unwrap().expect("collector must still be live");
            drop(release);
            wait(|| process.has_exited().unwrap().then_some(()));
        } else {
            assert!(matches!(outcome.cleanup, OutputCleanup::CollectorExited { pid: found } if found == pid));
            assert!(ProcessIdentity::of(pid).unwrap().is_none());
        }
    }
    let log = root.join("existing.log");std::fs::write(&log, b"keep").unwrap();
    let existing = create_output_with_budget_outcome(&log, None);
    assert!(existing.result.is_err() && matches!(existing.cleanup, OutputCleanup::Unproven));
    assert_eq!(std::fs::read(&log).unwrap(), b"keep");
}
