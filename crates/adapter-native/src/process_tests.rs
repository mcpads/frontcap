use super::*;

#[test]
fn stopped_owned_child_remains_live_until_exit() {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "kill -STOP $$; exit 0"]).process_group(0);
    let mut owned = OwnedChild::new(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = Command::new("ps").args(["-o", "stat=", "-p", &owned.id().to_string()])
            .output().unwrap();
        if String::from_utf8_lossy(&status.stdout).contains('T') { break; }
        assert!(Instant::now() < deadline, "child did not stop");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(!owned.exited().unwrap(), "stopped child is still live");
    owned.signal(libc::SIGCONT).unwrap();
    while !owned.exited().unwrap() {
        assert!(Instant::now() < deadline, "continued child did not exit");
        std::thread::sleep(Duration::from_millis(2));
    }
    let mut code = None;
    let mut signal_error = None;
    owned.retire(&mut code, &mut signal_error).unwrap();
    assert_eq!(code, Some(0));
}

#[test]
fn timeout_preserves_buffered_streams_and_retires_term_resistant_child() {
    let temp = tempfile::tempdir().unwrap();
    let ready = temp.path().join("output-ready");
    let mut command = Command::new("python3");
    command.args(["-c", "import sys,time,signal,pathlib; signal.signal(signal.SIGTERM,signal.SIG_IGN); print('partial',flush=True); print('diagnostic',file=sys.stderr,flush=True); pathlib.Path(sys.argv[1]).touch(); time.sleep(30)"]).arg(&ready);
    let started = Instant::now();
    let deadline = started + Duration::from_millis(300);
    let result = capture_started(&mut command, &[], deadline, &AtomicBool::new(false), 4096, |_| {
        // Delay ownership publication until both writes are known to have completed.
        // The I/O loop must see expiration before its first read, exercising the final
        // drain after retirement independently of interpreter startup speed.
        let watchdog = Instant::now() + Duration::from_secs(3);
        while !ready.exists() {
            if Instant::now() >= watchdog { return Err(io::Error::other("output readiness timed out")); }
            std::thread::sleep(Duration::from_millis(2));
        }
        while Instant::now() < deadline { std::thread::sleep(Duration::from_millis(2)); }
        Ok(())
    });
    assert!(started.elapsed() < Duration::from_secs(4));
    assert!(result.cleanup_failure.is_none(), "{result:?}");
    assert!(frontcap_core::process::ProcessIdentity::of(result.pid.unwrap()).unwrap().is_none());
    assert_eq!(result.failure.as_deref(), Some("bridge deadline exceeded"));
    assert_eq!(result.stdout, b"partial\n");
    assert_eq!(result.stderr, b"diagnostic\n");
}
