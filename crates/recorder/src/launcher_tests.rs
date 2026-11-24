use super::*;

#[tokio::test]
async fn invalid_passthrough_origin_is_rejected_before_registry_or_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace { root: tmp.path().join(".frontcap") };
    let req = LaunchRequest {
        request_id: RequestId::new("invalid-origin").unwrap(),
        url: "about:blank".into(),
        profile: DeterminismProfile::default(),
        rules: RuleSet::default(),
        passthrough_origins: Some(vec!["https://example.test/path".into()]),
        watchdog_ms: 1_000,
        repo_root: tmp.path().to_path_buf(),
        origin: None,
        restore: None,
    };
    let error = launch(&ws, req, &tmp.path().join("no-recorder"), &[]).await.unwrap_err();
    assert_eq!(error.0.code, ErrorCode::InvalidArguments);
    assert!(!ws.root.exists(), "invalid input must not reserve a launch or create runtime state");
}

#[tokio::test]
async fn a_locale_that_is_not_a_language_tag_is_rejected_before_registry_or_spawn() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace { root: tmp.path().join(".frontcap") };
    for locale in ["en-US\n--renderer-cmd-prefix=sh", "", "en US"] {
        let req = LaunchRequest {
            request_id: RequestId::new("invalid-locale").unwrap(),
            url: "about:blank".into(),
            profile: DeterminismProfile { locale: locale.into(), ..DeterminismProfile::default() },
            rules: RuleSet::default(),
            passthrough_origins: None,
            watchdog_ms: 1_000,
            repo_root: tmp.path().to_path_buf(),
            origin: None,
            restore: None,
        };
        let error = launch(&ws, req, &tmp.path().join("no-recorder"), &[]).await.unwrap_err();
        assert_eq!(error.0.code, ErrorCode::InvalidArguments, "{locale:?}");
    }
    assert!(!ws.root.exists(), "invalid input must not reserve a launch or create runtime state");
}

#[test]
fn registry_entry_is_exclusive_and_checks_arguments() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace { root: tmp.path().join(".frontcap") };
    let id = RequestId::new("launch-1").unwrap();
    let first = register(&ws, &id, "aaa").unwrap();
    let Registered::New(launch) = &first else { panic!("{first:?}") };
    let again = register(&ws, &id, "aaa").unwrap();
    assert_eq!(again, Registered::Existing(launch.clone()));
    let conflict = register(&ws, &id, "bbb").unwrap_err();
    assert_eq!(conflict.0.code, ErrorCode::RequestIdConflict);
}

#[test]
fn run_ids_follow_launch_ids() {
    let launch = LaunchId::new("g-0123456789abcdef").unwrap();
    assert_eq!(run_id_of(&launch).as_str(), "r-0123456789abcdef");
}

#[tokio::test]
async fn recover_waits_for_a_recorder_that_is_still_starting() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace { root: tmp.path().join(".frontcap") };
    let id = LaunchId::new("g-1").unwrap();
    let dir = ws.runtime_dir(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("recorder.sock");
    // A live recorder (this process stands in for it) that becomes ready only after a while.
    let mut record = runtime::RuntimeRecord {
        launch_id: id.clone(),
        state: RuntimeState::Starting,
        recorder: frontcap_core::process::ProcessIdentity::current(),
        browser: None,
        endpoint: None,
        socket: socket.clone(),
    };
    runtime::save_runtime(&dir, &record).unwrap();
    let d = dir.clone();
    let ready = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let listener = tokio::net::UnixListener::bind(&record.socket).unwrap();
        record.state = RuntimeState::Ready;
        runtime::save_runtime(&d, &record).unwrap();
        listener
    });
    let start = std::time::Instant::now();
    recover(&ws, &id, Path::new("/nonexistent/frontcap-recorder"), &[]).await.unwrap();
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
    drop(ready.await.unwrap());
}

#[test]
fn an_empty_registry_entry_left_by_a_crash_does_not_wedge_the_request_id() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace { root: tmp.path().join(".frontcap") };
    std::fs::create_dir_all(ws.launches()).unwrap();
    // The launcher died between creating the entry and writing it.
    std::fs::write(ws.launches().join("launch-2.json"), "").unwrap();
    let id = RequestId::new("launch-2").unwrap();
    let first = register(&ws, &id, "aaa").unwrap();
    assert!(matches!(first, Registered::New(_)), "{first:?}");
    assert!(matches!(register(&ws, &id, "aaa").unwrap(), Registered::Existing(_)));
}

#[test]
fn concurrent_registrations_agree_on_one_launch() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace { root: tmp.path().join(".frontcap") };
    let id = RequestId::new("launch-3").unwrap();
    let results: Vec<_> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8).map(|_| s.spawn(|| register(&ws, &id, "aaa").unwrap())).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let ids: std::collections::BTreeSet<_> = results.iter().map(|r| match r { Registered::New(l) | Registered::Existing(l) => l.clone() }).collect();
    assert_eq!(ids.len(), 1, "{results:?}");
    assert_eq!(results.iter().filter(|r| matches!(r, Registered::New(_))).count(), 1);
}

#[test]
fn saved_results_preserve_failures_and_do_not_resolve_dispatched_requests() {
    use frontcap_core::journal::Journal;
    use frontcap_recorder_protocol::{Answer, RequestResult};
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace { root: tmp.path().to_path_buf() };
    let launch = LaunchId::new("g-results").unwrap();
    let dir = ws.runtime_dir(&launch);
    std::fs::create_dir_all(&dir).unwrap();
    let path = runtime::journal_path(&dir);
    let done = RequestId::new("done").unwrap();
    let pending = RequestId::new("pending").unwrap();
    let intent = RequestId::new("intent").unwrap();
    let mut journal = Journal::open(&path).unwrap();
    let answer = Answer::Err(ErrorReply::new(ErrorCode::ScriptFailed, "original failure"));
    journal.begin(&done, "args").unwrap();
    journal.complete(&done, serde_json::to_value(&answer).unwrap()).unwrap();
    journal.begin(&pending, "args").unwrap();
    journal.dispatched(&pending).unwrap();
    journal.begin(&intent, "args").unwrap();
    drop(journal);
    let before = std::fs::read(&path).unwrap();
    let RequestResult::Completed { answer: saved } = saved_result(&ws, &launch, &done).unwrap() else { panic!("missing completion") };
    assert_eq!(serde_json::to_value(saved).unwrap(), serde_json::to_value(answer).unwrap());
    assert!(matches!(saved_result(&ws, &launch, &pending).unwrap(), RequestResult::Pending { dispatched: true }));
    assert!(matches!(saved_result(&ws, &launch, &intent).unwrap(), RequestResult::NotApplied));
    assert!(matches!(saved_result(&ws, &launch, &RequestId::new("unknown").unwrap()).unwrap(), RequestResult::Unknown));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn termination_cannot_take_over_an_owned_execution() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace { root: tmp.path().to_path_buf() };
    let id = LaunchId::new("g-owned").unwrap();
    let dir = ws.runtime_dir(&id);
    std::fs::create_dir_all(&dir).unwrap();
    let _owner = crate::take_lock(&dir).unwrap().unwrap();
    let error = terminate_failed(&ws, &id, &RequestId::new("stop").unwrap()).await.unwrap_err();
    assert_eq!(error.0.code, ErrorCode::Busy);
    assert!(!runtime::journal_path(&dir).exists());
}

#[test]
#[ignore = "requires an explicitly built FRONTCAP_DIAGNOSTIC_RECORDER executable"]
fn native_recorder_failure_uses_independent_diagnostic_collection() {
    let binary = PathBuf::from(std::env::var_os("FRONTCAP_DIAGNOSTIC_RECORDER")
        .expect("set FRONTCAP_DIAGNOSTIC_RECORDER to the current recorder build"));
    for exhausted in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let dir = temporary.path();
        // An invalid native spec exercises actual executable dispatch without acquiring any target.
        std::fs::write(crate::native::spec_path(dir), b"{").unwrap();
        let budget = exhausted.then(|| frontcap_core::write_budget::WriteBudget::create(
            &dir.join("budget"), LaunchId::generate(),
            frontcap_core::write_budget::Limits { ordinary: 1, retirement: 65536 }).unwrap());
        if let Some(budget) = &budget {
            budget.charge(frontcap_core::write_budget::Class::Ordinary, 1).unwrap();
        }
        spawn_with_budget(&binary, "launch", dir, &[], budget.as_ref()).unwrap();
        let root = dir.join("recorder-diagnostics");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let directory = std::fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
            if let Some(result) = frontcap_core::state::load::<frontcap_native::diagnostic_log::Summary>(
                &directory.join("result.json")).unwrap() {
                assert!(result.eof && result.observed_bytes > 0, "{result:?}");
                if let Some(budget) = &budget {
                    assert!(result.error.as_ref().is_some_and(|error| error.contains("allowance exhausted")));
                    assert_eq!(result.retained_bytes, 0);
                    assert_eq!(budget.charged().unwrap().ordinary, 1);
                    assert!(budget.charged().unwrap().retirement > 0);
                } else {
                    assert!(result.error.is_none(), "{result:?}");
                    assert!(result.retained_bytes > 0);
                }
                assert_eq!(std::fs::metadata(dir.join("recorder.log")).unwrap().len(), result.retained_bytes);
                let process: frontcap_core::process::ProcessIdentity = frontcap_core::state::load(
                    &directory.join("ready.json")).unwrap().unwrap();
                while !process.has_exited().unwrap() {
                    assert!(std::time::Instant::now() < deadline);
                    std::thread::sleep(Duration::from_millis(10));
                }
                break;
            }
            assert!(std::time::Instant::now() < deadline, "collector did not reach EOF after recorder failure");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read(crate::native::spec_path(dir)).unwrap(), b"{");
    }
}
