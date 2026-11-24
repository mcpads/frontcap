use super::*;

#[test]
fn current_process_is_alive_and_identified_by_start_time() {
    let me = ProcessIdentity::current();
    assert!(me.is_alive());
    let forged = ProcessIdentity { pid: me.pid, started: "Thu Jan  1 00:00:00 1970".into() };
    assert!(!forged.is_alive(), "a reused pid with another start time is not the same process");
}

#[test]
fn exited_process_is_not_alive_and_is_never_killed() {
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let id = ProcessIdentity::of(child.id()).unwrap().expect("identity while running or zombie");
    child.wait().unwrap();
    assert!(!id.is_alive());
    assert!(!id.kill().unwrap());
}

#[test]
fn kill_by_identity() {
    let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let id = ProcessIdentity::of(child.id()).unwrap().unwrap();
    assert!(id.kill().unwrap());
    assert!(!id.is_alive(), "kill returns once the process has exited");
    child.wait().unwrap();
}

#[test]
fn a_failed_signal_is_an_error_while_the_owned_process_is_alive() {
    let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let id = ProcessIdentity::of(child.id()).unwrap().unwrap();
    let result = id.kill_with(|| Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "signal denied")));
    let alive = id.is_alive();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(alive, "the rejected signal must leave this oracle's process alive");
    assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn a_signal_failure_racing_an_exit_is_resolved_by_the_exit_evidence() {
    let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let id = ProcessIdentity::of(child.id()).unwrap().unwrap();
    let result = id.kill_with(|| {
        child.kill()?;
        child.wait()?;
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "process exited before signal"))
    });
    assert!(!result.unwrap());
    assert!(!id.is_alive());
}

#[test]
fn failed_process_inspection_is_not_successful_termination() {
    const OWNER: &str = "FRONTCAP_PROCESS_PROBE_ORACLE";
    if let Some(encoded) = std::env::var_os(OWNER) {
        let id: ProcessIdentity = serde_json::from_str(&encoded.to_string_lossy()).unwrap();
        assert_eq!(id.kill().unwrap_err().kind(), std::io::ErrorKind::NotFound);
        return;
    }
    let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
    let id = ProcessIdentity::of(child.id()).unwrap().unwrap();
    let empty_path = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "process::tests::failed_process_inspection_is_not_successful_termination", "--nocapture"])
        .env(OWNER, serde_json::to_string(&id).unwrap()).env("PATH", empty_path.path()).output().unwrap();
    let alive = id.is_alive();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(alive, "an unavailable inspector must not signal an unverified process");
}

#[test]
fn identity_does_not_depend_on_the_observers_locale_or_time_zone() {
    let me = ProcessIdentity::current();
    for env in [[("TZ", "Asia/Seoul"), ("LANG", "ko_KR.UTF-8")], [("TZ", "UTC"), ("LANG", "C")]] {
        // What a control server started under another environment records for the same process.
        let out = std::process::Command::new("ps")
            .args(["-o", "lstart=", "-p", &me.pid.to_string()])
            .env_clear()
            .envs(env)
            .output()
            .unwrap();
        assert!(!String::from_utf8_lossy(&out.stdout).trim().is_empty());
    }
    let fields: Vec<&str> = me.started.split_whitespace().collect();
    assert_eq!(fields.len(), 5, "C-locale lstart: {}", me.started);
    assert!(fields[0].len() == 3 && fields[0].is_ascii(), "{}", me.started);
    assert!(fields[3].len() == 8 && fields[3].as_bytes()[2] == b':', "UTC clock time: {}", me.started);
}
