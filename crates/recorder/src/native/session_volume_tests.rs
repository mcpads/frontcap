use super::*;

fn target() -> target::Launch {
    let mut config: frontcap_native::ios_target::IosLaunch = serde_json::from_str(
        include_str!("../../tests/fixtures/native/ios-launch.json")).unwrap();
    config.surface = frontcap_native::ios_execution::UiSurface::Uikit;
    target::Launch::Ios(config)
}
fn config() -> Config {
    Config { python: "/usr/bin/python3".into(), worker: "/worker.py".into(), size_mib: 256, reserve_mib: 8 }
}

#[test]
fn volume_admission_rejects_conflicting_paths_unsupported_targets_and_impossible_floors() {
    let config = config(); let target = target();
    validate(Some(&config), None, &target, 32 * 1024 * 1024).unwrap();
    assert!(validate(Some(&config), Some(Path::new("/external")), &target, 0).is_err());
    assert!(validate(Some(&config), None, &target, storage::DEFAULT_MIN_FREE_BYTES).is_err());
    validate(None, Some(Path::new("/external")), &target, storage::DEFAULT_MIN_FREE_BYTES).unwrap();
    let request = launcher::LaunchRequest {
        request_id: RequestId::new("launch").unwrap(), target, session_executable: "/session".into(),
        acquisition_budget_ms: 1000, min_free_bytes: Some(32 * 1024 * 1024), write_budget: None,
        session_root: None, session_volume: Some(config), service: None,
    };
    let retained = args_sha256(&serde_json::to_value(&request).unwrap());
    let mut other = request.clone(); other.session_volume = None;
    assert_ne!(retained, args_sha256(&serde_json::to_value(&other).unwrap()));
    other.session_volume = request.session_volume;
    other.session_volume.as_mut().unwrap().size_mib = 512;
    assert_ne!(retained, args_sha256(&serde_json::to_value(&other).unwrap()));
}

#[test]
fn session_volume_requires_diagnostic_eof_and_exit_without_hiding_stream_loss() {
    let temporary = tempfile::tempdir().unwrap();
    let session = temporary.path().canonicalize().unwrap();
    let directory = session.join("worker.log.diagnostics/attempt"); fs::create_dir_all(&directory).unwrap();
    #[derive(Serialize)] struct Log { log: PathBuf }
    save_atomic(&directory.join("config.json"), &Log { log: session.join("worker.log") }).unwrap();
    let mut child = std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    let process = ProcessIdentity::of(child.id()).unwrap().unwrap();
    save_atomic(&directory.join("ready.json"), &process).unwrap();
    assert!(wait_for_diagnostics(&session, Instant::now()).is_err());
    child.kill().unwrap(); child.wait().unwrap();
    assert!(wait_for_diagnostics(&session, Instant::now()).is_err());
    let mut summary = frontcap_native::diagnostic_log::Summary {
        limit_bytes: 4, observed_bytes: 8, retained_bytes: 4, eof: false, error: Some("allowance exhausted".into()),
    };
    save_atomic(&directory.join("result.json"), &summary).unwrap();
    assert!(wait_for_diagnostics(&session, Instant::now()).is_err());
    summary.eof = true; save_atomic(&directory.join("result.json"), &summary).unwrap();
    let original = fs::read(directory.join("result.json")).unwrap();
    wait_for_diagnostics(&session, Instant::now()).unwrap();
    assert_eq!(fs::read(directory.join("result.json")).unwrap(), original);
    fs::create_dir(session.join("worker.log.diagnostics/another")).unwrap();
    assert!(wait_for_diagnostics(&session, Instant::now()).is_err());
}


#[tokio::test]
#[ignore = "requires explicit fresh output root and real macOS volume worker; never starts Simulator"]
#[cfg(target_os = "macos")]
async fn preparation_failure_retires_real_unused_session_volume_before_target_dispatch() {
    let root = PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PROBE_ROOT").expect("explicit fresh probe root"));
    fs::create_dir(&root).unwrap();
    let base = root.canonicalize().unwrap();
    for failure in ["metadata", "service-port", "target-config"] {
    let root = base.join(failure); fs::create_dir(&root).unwrap();
    let config = Config {
        python: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PYTHON").expect("explicit Python")),
        worker: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_WORKER").expect("explicit volume worker")),
        size_mib: 256, reserve_mib: 8,
    };
    let mut spec = LaunchSpec {
        request_id: RequestId::new("launch").unwrap(), launch_id: LaunchId::generate(),
        run_id: frontcap_core::ids::RunId::new("preparation-refusal").unwrap(), target: target(),
        session_executable: root.join("must-not-start"), acquisition_budget_ms: 60_000,
        min_free_bytes: Some(0), write_budget: None, session_root: None,
        session_volume: Some(config), service: None,
    };
    let occupied = if failure == "service-port" {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        spec.service = Some(ServiceMode::ControlledHttpAt(frontcap_core::native::service::RequestEndpoint {
            port: listener.local_addr().unwrap().port(), request_path: "/request/".into(),
        }));
        Some(listener)
    } else { None };
    if failure == "target-config" {
        spec.service = Some(ServiceMode::ControlledHttp);
        if let target::Launch::Ios(config) = &mut spec.target {
            config.service_url_environment = Some("FRONTCAP_TEST_SERVICE".into());
            config.app_environment.insert("FRONTCAP_TEST_SERVICE".into(), "existing".into());
        }
    }
    save_atomic(&spec_path(&root), &spec).unwrap();
    if failure == "metadata" {
        // Refuse metadata publication only after the original volume owner is ready.
        fs::create_dir(root.join("native-storage-roots.json")).unwrap();
    }
    let error = super::super::launch_owned(&root).await.unwrap_err();
    assert!(error.to_string().contains("native preparation failed"), "{error}");
    let proof: UnstartedCompletion = load(&root.join("native-session-volume-acquisition-retirement.json")).unwrap().unwrap();
    assert_eq!(proof.launch_id, spec.launch_id);
    assert!(!root.join("native-session-volume/worker/volume/bounded.dmg").exists());
    assert!(!root.join("native-session-volume/worker/volume/mount").exists());
    assert!(!root.join("native-session-volume-retirement.json").exists());
    assert!(saved_stop(&root, &spec, &RequestId::new("stop").unwrap()).unwrap().is_none());
    assert!(!root.join("native-status.json").exists());
    let process: ProcessIdentity = load(&root.join("native-session-volume/worker-process.json")).unwrap().unwrap();
    assert!(process.has_exited().unwrap());
    if let Some(listener) = occupied {
        assert!(std::net::TcpStream::connect(listener.local_addr().unwrap()).is_ok(), "foreign listener survives");
    }
    if failure == "target-config" {
        let service: frontcap_service_fixture::Record = load(&root.join("service/state.json")).unwrap().unwrap();
        assert!(service.stopping);
        assert!(std::net::TcpStream::connect(service.address).is_err());
    }
    }
}

#[tokio::test]
#[ignore = "requires fresh output root, real volume worker and diagnostic recorder; UIKit session is a substitute"]
#[cfg(target_os = "macos")]
async fn failed_acquisition_retains_volume_after_original_session_and_log_retire() {
    use std::os::unix::fs::PermissionsExt;
    assert!(std::env::var_os(frontcap_native::diagnostic_log::RECORDER_ENV).is_some(), "explicit diagnostic collector required");
    let root = PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PROBE_ROOT").expect("explicit fresh probe root"));
    fs::create_dir(&root).unwrap(); let base = root.canonicalize().unwrap();
    for failure in ["registration", "before-ready", "invalid-ready"] {
    let root = base.join(failure); fs::create_dir(&root).unwrap();
    let python = PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PYTHON").expect("explicit Python"));
    let executable = root.join("session.py");
    let fixture = include_str!("../../tests/fixtures/native/uikit-session.py");
    let fixture = fixture.replacen("#!/usr/bin/env python3", &format!("#!{}", python.display()), 1);
    let fixture = match failure {
        "registration" => fixture.replace("if m['command']=='start':", &format!("if m['command']=='start':\n   Path({}).mkdir()",
            serde_json::to_string(&root.join("native-storage-acquired-roots.json")).unwrap())),
        "before-ready" => fixture.replace("if m['command']=='start':", "if m['command']=='start':\n   raise RuntimeError('refused before readiness')"),
        _ => fixture.replace("save(root/'ready.json',owner)", "save(root/'ready.json',dict(invalid=True))"),
    };
    fs::write(&executable, fixture).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let config = Config {
        python, worker: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_WORKER").expect("explicit volume worker")),
        size_mib: 256, reserve_mib: 8,
    };
    let spec = LaunchSpec {
        request_id: RequestId::new("launch").unwrap(), launch_id: LaunchId::generate(),
        run_id: frontcap_core::ids::RunId::new("registration-refusal").unwrap(), target: target(),
        session_executable: executable, acquisition_budget_ms: 60_000, min_free_bytes: Some(0),
        write_budget: None, session_root: None, session_volume: Some(config.clone()),
        service: Some(ServiceMode::ControlledHttp),
    };
    save_atomic(&spec_path(&root), &spec).unwrap();
    let error = super::super::launch_owned(&root).await.unwrap_err();
    let expected = match failure { "registration" => "acquired storage registration", "before-ready" => "native session", _ => "unknown field" };
    assert!(error.to_string().contains(expected), "{failure}: {error}");
    let proof: FailedAcquisitionCompletion = load(&root.join("native-session-volume-failed-acquisition-retirement.json")).unwrap().unwrap();
    assert_eq!(proof.launch_id, spec.launch_id); assert!(proof.retirement.passed);
    frontcap_native::storage_volume::inspect_retained(&root.join("native-session-volume"), &config, &proof.volume).unwrap();
    assert!(proof.volume.retained_image.unwrap().gzip.is_some());
    let mount = root.join("native-session-volume/worker/volume/mount");
    assert!(fs::read_dir(&mount).unwrap().next().is_none());
    assert_eq!(fs::metadata(&mount).unwrap().dev(), fs::metadata(&root).unwrap().dev());
    assert!(!root.join("native-session-volume-retirement.json").exists());
    assert!(!root.join("native-session-volume-acquisition-retirement.json").exists());
    assert!(saved_stop(&root, &spec, &RequestId::new("stop").unwrap()).unwrap().is_none());
    assert!(!root.join("native-status.json").exists());
    let service: frontcap_service_fixture::Record = load(&root.join("service/state.json")).unwrap().unwrap();
    assert!(service.stopping); assert!(std::net::TcpStream::connect(service.address).is_err());
    }
}

#[test]
fn failed_readiness_never_returns_volume_retirement_authority_without_passing_target_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let executable = root.path().join("session.py");
    let fixture = include_str!("../../tests/fixtures/native/uikit-session.py")
        .replace("save(root/'ready.json',owner)", "save(root/'ready.json',dict(invalid=True))")
        .replace("retirement=dict(passed=True,errors=[],signal_error=None)", "retirement=dict(passed=False,errors=['unretired target'],signal_error=None)");
    fs::write(&executable, fixture).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let acquired = Session::acquire_with_budget(&executable, target(), &root.path().join("session"),
        Instant::now() + Duration::from_secs(10), None);
    let error = acquired.result.err().expect("readiness must fail");
    assert!(error.to_string().contains("unknown field"), "{error}");
    assert!(error.to_string().contains("target retirement"), "{error}");
    assert!(acquired.retirement.is_none());
    assert!(matches!(acquired.ios_startup_cleanup, frontcap_native::ios_session::StartupCleanup::Unproven));
    assert!(root.path().join("session/result.json").exists());
}


#[test]
fn worker_spawn_failure_authority_requires_a_fresh_session_directory() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("session");
    let missing = root.path().join("missing-worker");
    let deadline = Instant::now() + Duration::from_secs(10);
    let acquired = Session::acquire_with_budget(&missing, target(), &directory, deadline, None);
    assert!(acquired.result.is_err()); assert!(matches!(acquired.ios_startup_cleanup, frontcap_native::ios_session::StartupCleanup::WorkerNotStarted));
    assert!(acquired.retirement.is_none());
    assert!(directory.join("launch.json").is_file());
    assert!(!directory.join("worker.json").exists());
    let repeated = Session::acquire_with_budget(&missing, target(), &directory, deadline, None);
    assert!(repeated.result.is_err()); assert!(matches!(repeated.ios_startup_cleanup, frontcap_native::ios_session::StartupCleanup::Unproven));
    assert!(repeated.retirement.is_none());
}

#[tokio::test]
#[ignore = "requires explicit fresh output root and real macOS volume worker; never starts Simulator"]
#[cfg(target_os = "macos")]
async fn worker_spawn_failure_retires_partial_session_after_diagnostic_eof() {
    use std::os::unix::fs::MetadataExt;
    assert!(std::env::var_os(frontcap_native::diagnostic_log::RECORDER_ENV).is_some());
    let root = PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PROBE_ROOT").expect("fresh probe root"));
    fs::create_dir(&root).unwrap(); let root = root.canonicalize().unwrap();
    let config = Config {
        python: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PYTHON").expect("Python")),
        worker: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_WORKER").expect("volume worker")),
        size_mib: 256, reserve_mib: 8,
    };
    let spec = LaunchSpec {
        request_id: RequestId::new("launch").unwrap(), launch_id: LaunchId::generate(),
        run_id: frontcap_core::ids::RunId::new("worker-spawn-refusal").unwrap(), target: target(),
        session_executable: root.join("missing-worker"), acquisition_budget_ms: 60_000,
        min_free_bytes: Some(0), write_budget: None, session_root: None,
        session_volume: Some(config.clone()), service: Some(ServiceMode::ControlledHttp),
    };
    save_atomic(&spec_path(&root), &spec).unwrap();
    let error = super::super::launch_owned(&root).await.unwrap_err();
    assert!(error.to_string().contains("unstarted worker volume retirement: Ok(Ok(()))"), "{error}");
    let proof: UnstartedCompletion = load(&root.join("native-session-volume-start-failure-retirement.json")).unwrap().unwrap();
    assert_eq!(proof.launch_id, spec.launch_id);
    frontcap_native::storage_volume::inspect_retained(&root.join("native-session-volume"), &config, &proof.volume).unwrap();
    assert!(proof.volume.retained_image.unwrap().gzip.is_some());
    let mount = root.join("native-session-volume/worker/volume/mount");
    assert!(fs::read_dir(&mount).unwrap().next().is_none());
    assert_eq!(fs::metadata(&mount).unwrap().dev(), fs::metadata(&root).unwrap().dev());
    let worker: ProcessIdentity = load(&root.join("native-session-volume/worker-process.json")).unwrap().unwrap();
    assert!(worker.has_exited().unwrap());
    assert!(!root.join("native-session-volume-retirement.json").exists());
    assert!(!root.join("native-session-volume-failed-acquisition-retirement.json").exists());
    assert!(saved_stop(&root, &spec, &RequestId::new("stop").unwrap()).unwrap().is_none());
    assert!(!root.join("native-status.json").exists());
    let service: frontcap_service_fixture::Record = load(&root.join("service/state.json")).unwrap().unwrap();
    assert!(service.stopping); assert!(std::net::TcpStream::connect(service.address).is_err());
}


#[test]
fn failed_initial_dispatch_returns_only_original_passing_retirement() {
    use std::os::unix::fs::PermissionsExt;
    for cleanup in ["passed", "failed", "missing", "contradictory"] {
        let root = tempfile::tempdir().unwrap(); let executable = root.path().join("session.py");
        let fixture = include_str!("../../tests/fixtures/native/uikit-session.py");
        let fixture = match cleanup {
            "contradictory" => fixture.replace("dict(error=None,retirement=dict(passed=True,errors=[],signal_error=None))",
                "dict(error=None,retirement=dict(passed=True,errors=[],signal_error=None),acquisition_not_started=True)"),
            "failed" => fixture.replace("retirement=dict(passed=True,errors=[],signal_error=None)",
                "retirement=dict(passed=False,errors=['unretired target'],signal_error=None)"),
            "missing" => fixture.replace("finally:save(root/'result.json',dict(error=None,retirement=dict(passed=True,errors=[],signal_error=None)))", "finally:pass"),
            _ => fixture.to_owned(),
        };
        fs::write(&executable, fixture).unwrap(); fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let directory = root.path().join("session");
        // Expiration before the initial write deterministically rejects after successful spawn.
        let acquired = Session::acquire_with_budget(&executable, target(), &directory, Instant::now(), None);
        let error = acquired.result.err().expect("initial dispatch must fail");
        assert!(error.to_string().contains("native session deadline exceeded"), "{cleanup}: {error}");
        assert!(matches!(acquired.ios_startup_cleanup, frontcap_native::ios_session::StartupCleanup::Unproven));
        assert_eq!(acquired.retirement.is_some(), cleanup == "passed");
        if cleanup != "passed" { assert!(error.to_string().contains("target retirement")); }
        let process: ProcessIdentity = load(&directory.join("worker.json")).unwrap().unwrap();
        assert!(process.has_exited().unwrap());
        assert!(!directory.join("target").exists());
    }
}


#[test]
fn pre_acquisition_worker_entry() {
    if let Some(directory) = std::env::var_os("FRONTCAP_PRE_ACQUISITION_WORKER") {
        if let Ok(limit) = std::env::var("FRONTCAP_PRE_ACQUISITION_EXHAUST_ORDINARY") {
            let budget = WriteBudget::from_environment().unwrap().unwrap();
            budget.charge(Class::Ordinary, limit.parse::<u64>().unwrap() - budget.charged().unwrap().ordinary).unwrap();
        }
        frontcap_native::ios_session::run(&PathBuf::from(directory)).unwrap();
    }
}

#[tokio::test]
#[ignore = "requires fresh root, real volume and diagnostic worker; real iOS worker receives no Start"]
#[cfg(target_os = "macos")]
async fn real_worker_pre_admission_exit_retires_partial_session_volume() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    assert!(std::env::var_os(frontcap_native::diagnostic_log::RECORDER_ENV).is_some());
    let root = PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PROBE_ROOT").expect("fresh root"));
    fs::create_dir(&root).unwrap(); let root = root.canonicalize().unwrap();
    let config = Config {
        python: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PYTHON").expect("Python")),
        worker: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_WORKER").expect("volume worker")),
        size_mib: 256, reserve_mib: 8,
    };
    let executable = root.join("session.py");
    fs::write(&executable, format!("#!{}\nimport os,sys\nos.environ['FRONTCAP_PRE_ACQUISITION_WORKER']=sys.argv[1]\nexe={}\nos.execv(exe,[exe,'--exact','native::session_volume::tests::pre_acquisition_worker_entry','--nocapture'])\n",
        config.python.display(), serde_json::to_string(&std::env::current_exe().unwrap()).unwrap())).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let launch = LaunchId::generate();
    let volume = SessionVolume::start(&root, &launch, &config, Instant::now() + Duration::from_secs(60), None, 0).unwrap();
    let acquired = Session::acquire_with_budget(&executable, target(), &volume.session, Instant::now(), None);
    let error = acquired.result.err().expect("expired Start dispatch");
    assert!(error.to_string().contains("native session deadline exceeded"), "{error}");
    assert!(acquired.retirement.is_none());
    let frontcap_native::ios_session::StartupCleanup::AcquisitionNotStarted { worker } = acquired.ios_startup_cleanup else {
        panic!("original worker must prove pre-admission exit");
    };
    assert!(worker.has_exited().unwrap());
    assert!(!volume.session.join("target").exists());
    let result: frontcap_native::ios_session::SessionResult = load(&volume.session.join("result.json")).unwrap().unwrap();
    assert!(result.acquisition_not_started && result.retirement.is_none());
    save_atomic(&root.join("startup-error.json"), &error.to_string()).unwrap();
    volume.retire_before_acquisition(&root, worker, Instant::now() + Duration::from_secs(60), None).unwrap();
    let proof: BeforeAcquisitionCompletion = load(&root.join("native-session-volume-before-acquisition-retirement.json")).unwrap().unwrap();
    assert_eq!(proof.launch_id, launch); assert!(proof.worker.has_exited().unwrap());
    frontcap_native::storage_volume::inspect_retained(&root.join("native-session-volume"), &config, &proof.volume).unwrap();
    let mount = root.join("native-session-volume/worker/volume/mount");
    assert!(fs::read_dir(&mount).unwrap().next().is_none());
    assert_eq!(fs::metadata(&mount).unwrap().dev(), fs::metadata(&root).unwrap().dev());
    assert!(!root.join("native-session-volume-retirement.json").exists());
    assert!(!root.join("native-session-volume-failed-acquisition-retirement.json").exists());
    assert!(!root.join("native-status.json").exists());
}

#[test]
fn readiness_failure_preserves_explicit_pre_acquisition_outcome() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap(); let executable = root.path().join("session.py");
    let fixture = include_str!("../../tests/fixtures/native/uikit-session.py")
        .replace("if m['command']=='start':", "if m['command']=='start':\n   raise RuntimeError('refused before acquisition')")
        .replace("dict(error=None,retirement=dict(passed=True,errors=[],signal_error=None))",
            "dict(error='refused before acquisition',retirement=None,acquisition_not_started=True)");
    fs::write(&executable, fixture).unwrap(); fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let directory = root.path().join("session");
    let acquired = Session::acquire_with_budget(&executable, target(), &directory, Instant::now() + Duration::from_secs(10), None);
    assert!(acquired.result.is_err()); assert!(acquired.retirement.is_none());
    let frontcap_native::ios_session::StartupCleanup::AcquisitionNotStarted { worker } = acquired.ios_startup_cleanup else {
        panic!("readiness failure must retain the original pre-acquisition outcome");
    };
    assert!(worker.has_exited().unwrap()); assert!(!directory.join("target").exists());
}


#[test]
fn refused_launch_metadata_proves_no_writers_only_for_a_new_directory() {
    let root = tempfile::tempdir().unwrap(); let root = root.path().canonicalize().unwrap();
    let budget = WriteBudget::create(&root.join("budget"), LaunchId::generate(),
        frontcap_core::write_budget::Limits { ordinary: 1, retirement: 1024 * 1024 }).unwrap();
    let directory = root.join("session"); let executable = root.join("must-not-start");
    let acquired = Session::acquire_with_budget(&executable, target(), &directory,
        Instant::now() + Duration::from_secs(10), Some(&budget));
    let error = acquired.result.err().expect("launch metadata must exceed allowance");
    assert!(error.to_string().contains("write allowance exhausted"), "{error}");
    assert!(matches!(acquired.ios_startup_cleanup, frontcap_native::ios_session::StartupCleanup::NoWritersStarted));
    assert!(acquired.retirement.is_none());
    assert!(directory.join("session.lock").is_file());
    assert!(!directory.join("launch.json").exists() && !directory.join("worker.log").exists());
    assert_eq!(budget.charged().unwrap().ordinary, 0);
    let repeated = Session::acquire_with_budget(&executable, target(), &directory,
        Instant::now() + Duration::from_secs(10), Some(&budget));
    assert!(repeated.result.is_err());
    assert!(matches!(repeated.ios_startup_cleanup, frontcap_native::ios_session::StartupCleanup::Unproven));
}

#[tokio::test]
#[ignore = "requires fresh root and real macOS volume worker; no session or diagnostic worker is started"]
#[cfg(target_os = "macos")]
async fn metadata_allowance_refusal_retires_partial_session_volume() {
    use std::os::unix::fs::MetadataExt;
    let root = PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PROBE_ROOT").expect("fresh root"));
    fs::create_dir(&root).unwrap(); let root = root.canonicalize().unwrap();
    let config = Config {
        python: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PYTHON").expect("Python")),
        worker: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_WORKER").expect("volume worker")),
        size_mib: 256, reserve_mib: 8,
    };
    let launch = LaunchId::generate();
    let volume = SessionVolume::start(&root, &launch, &config, Instant::now() + Duration::from_secs(60), None, 0).unwrap();
    let budget = WriteBudget::create(&root.join("budget"), launch.clone(),
        frontcap_core::write_budget::Limits { ordinary: 1, retirement: 1024 * 1024 }).unwrap();
    let acquired = Session::acquire_with_budget(&root.join("must-not-start"), target(), &volume.session,
        Instant::now() + Duration::from_secs(10), Some(&budget));
    let error = acquired.result.err().expect("metadata refusal");
    assert!(error.to_string().contains("write allowance exhausted"));
    assert!(matches!(acquired.ios_startup_cleanup, frontcap_native::ios_session::StartupCleanup::NoWritersStarted));
    assert!(acquired.retirement.is_none());
    assert!(volume.session.join("session.lock").is_file());
    assert!(!volume.session.join("worker.log.diagnostics").exists());
    save_atomic(&root.join("startup-error.json"), &error.to_string()).unwrap();
    volume.retire_metadata_failure(&root, Instant::now() + Duration::from_secs(60), Some(&budget)).unwrap();
    let proof: UnstartedCompletion = load(&root.join("native-session-volume-metadata-failure-retirement.json")).unwrap().unwrap();
    assert_eq!(proof.launch_id, launch);
    frontcap_native::storage_volume::inspect_retained(&root.join("native-session-volume"), &config, &proof.volume).unwrap();
    let worker: ProcessIdentity = load(&root.join("native-session-volume/worker-process.json")).unwrap().unwrap();
    assert!(worker.has_exited().unwrap());
    let mount = root.join("native-session-volume/worker/volume/mount");
    assert!(fs::read_dir(&mount).unwrap().next().is_none());
    assert_eq!(fs::metadata(&mount).unwrap().dev(), fs::metadata(&root).unwrap().dev());
    assert_eq!(budget.charged().unwrap().ordinary, 0); assert!(budget.charged().unwrap().retirement > 0);
    assert!(!root.join("native-session-volume-retirement.json").exists());
    assert!(!root.join("native-status.json").exists());
}

#[tokio::test]
#[ignore = "requires fresh root, real volume and an explicitly faulted diagnostic executable"]
#[cfg(target_os = "macos")]
async fn diagnostic_startup_failure_retires_only_after_live_disposition() {
    use std::os::unix::fs::MetadataExt;
    assert!(std::env::var_os(frontcap_native::diagnostic_log::RECORDER_ENV).is_some());
    let expect_collector = std::env::var("FRONTCAP_EXPECT_COLLECTOR_EXIT").unwrap() == "true";
    let root = PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PROBE_ROOT").expect("fresh root"));
    fs::create_dir(&root).unwrap(); let root = root.canonicalize().unwrap();
    let config = Config {
        python: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PYTHON").expect("Python")),
        worker: PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_WORKER").expect("volume worker")),
        size_mib: 256, reserve_mib: 8,
    };
    let launch = LaunchId::generate();
    let volume = SessionVolume::start(&root, &launch, &config, Instant::now() + Duration::from_secs(60), None, 0).unwrap();
    // Refuse diagnostic configuration before collector spawn without breaking volume startup.
    let budget = (!expect_collector).then(|| WriteBudget::create(&root.join("diagnostic-budget"), launch.clone(),
        frontcap_core::write_budget::Limits { ordinary: 1024 * 1024, retirement: 1 }).unwrap());
    let acquired = Session::acquire_with_budget(&root.join("must-not-start"), target(), &volume.session,
        Instant::now() + Duration::from_secs(10), budget.as_ref());
    let error = acquired.result.err().expect("diagnostic failure must reject acquisition");
    assert!(acquired.retirement.is_none());
    let frontcap_native::ios_session::StartupCleanup::DiagnosticFailed { collector_pid } = acquired.ios_startup_cleanup else {
        panic!("diagnostic startup must supply original exit/no-spawn authority: {error}");
    };
    assert_eq!(collector_pid.is_some(), expect_collector);
    if let Some(pid) = collector_pid { assert!(ProcessIdentity::of(pid).unwrap().is_none()); }
    assert!(!volume.session.join("worker.json").exists());
    assert!(!volume.session.join("target").exists());
    save_atomic(&root.join("startup-error.json"), &error.to_string()).unwrap();
    volume.retire_diagnostic_failure(&root, collector_pid, Instant::now() + Duration::from_secs(60), None).unwrap();
    let proof: DiagnosticFailureCompletion = load(&root.join("native-session-volume-diagnostic-failure-retirement.json")).unwrap().unwrap();
    assert_eq!(proof.launch_id, launch); assert_eq!(proof.collector_pid, collector_pid);
    frontcap_native::storage_volume::inspect_retained(&root.join("native-session-volume"), &config, &proof.volume).unwrap();
    let worker: ProcessIdentity = load(&root.join("native-session-volume/worker-process.json")).unwrap().unwrap();
    assert!(worker.has_exited().unwrap());
    let mount = root.join("native-session-volume/worker/volume/mount");
    assert!(fs::read_dir(&mount).unwrap().next().is_none());
    assert_eq!(fs::metadata(&mount).unwrap().dev(), fs::metadata(&root).unwrap().dev());
    assert!(!root.join("native-session-volume-retirement.json").exists());
    assert!(!root.join("native-status.json").exists());
}

#[test]
#[ignore = "exits its own probe process after registration; requires fresh root, real volume and diagnostic worker"]
#[cfg(target_os = "macos")]
fn acquisition_watch_owner_exit_before_start() {
    use std::os::unix::fs::PermissionsExt;
    assert!(std::env::var_os(frontcap_native::diagnostic_log::RECORDER_ENV).is_some());
    let root = PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PROBE_ROOT").expect("fresh root"));
    fs::create_dir(&root).unwrap();let root = root.canonicalize().unwrap();
    let config = Config { python: std::env::var_os("FRONTCAP_VOLUME_PYTHON").unwrap().into(),
        worker: std::env::var_os("FRONTCAP_VOLUME_WORKER").unwrap().into(), size_mib: 256, reserve_mib: 8 };
    let executable = root.join("session.py");
    fs::write(&executable, format!("#!{}\nimport os,sys\nos.environ['FRONTCAP_PRE_ACQUISITION_WORKER']=sys.argv[1]\nexe={}\nos.execv(exe,[exe,'--exact','native::session_volume::tests::pre_acquisition_worker_entry','--nocapture'])\n",
        config.python.display(), serde_json::to_string(&std::env::current_exe().unwrap()).unwrap())).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let launch = LaunchId::generate();
    let mut volume = SessionVolume::start(&root, &launch, &config, Instant::now() + Duration::from_secs(60), None, 0).unwrap();
    let directory = volume.session.clone();
    let _ = Session::acquire_with_observer(&executable, target(), &directory,
        Instant::now() + Duration::from_secs(30), None, Some(&mut |session| {
            volume.watch_acquisition(session, Instant::now() + Duration::from_secs(10))?;
            assert!(!directory.join("target").exists());
            save_atomic(&root.join("registered-before-exit.json"), &(launch.clone(), session.worker_identity()?))?;
            // OS descriptor closure, bypassing every Rust Drop and explicit Stop/retire call.
            std::process::exit(0);
        }));
    panic!("probe must exit before Start dispatch");
}


#[tokio::test]
async fn acquisition_failure_preserves_empty_original_operation_history() {
    let temporary = tempfile::tempdir().unwrap();
    let workspace = crate::launcher::Workspace { root: temporary.path().to_path_buf() };
    let launch = LaunchId::generate();let root = workspace.runtime_dir(&launch);
    fs::create_dir_all(&root).unwrap();
    let spec = LaunchSpec { request_id: RequestId::new("launch").unwrap(), launch_id: launch.clone(),
        run_id: frontcap_core::ids::RunId::new("acquisition-history").unwrap(), target: target(),
        session_executable: root.join("missing-worker"), acquisition_budget_ms: 1000,
        min_free_bytes: Some(0), write_budget: None, session_root: None, session_volume: None, service: None };
    save_atomic(&spec_path(&root), &spec).unwrap();
    super::super::launch_owned(&root).await.unwrap_err();
    let path = runtime::journal_path(&root);let original = fs::read(&path).unwrap();
    assert!(original.is_empty());assert!(!root.join("native-status.json").exists());
    let stop = RequestId::new("stop").unwrap();
    assert!(matches!(crate::launcher::saved_result(&workspace, &launch, &stop).unwrap(), RequestResult::Unknown));
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(super::super::launch_owned(&root).await.is_err(), "failed acquisition cannot be replayed");
    fs::remove_file(&path).unwrap();
    assert!(crate::launcher::saved_result(&workspace, &launch, &stop).is_err(), "missing evidence cannot be guessed empty");
}


#[test]
#[ignore = "requires fresh root, real APFS volume and diagnostic worker; no target is acquired"]
#[cfg(target_os = "macos")]
fn ordinary_exhaustion_before_identity_retires_registered_volume() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    assert!(std::env::var_os(frontcap_native::diagnostic_log::RECORDER_ENV).is_some());
    let root = PathBuf::from(std::env::var_os("FRONTCAP_VOLUME_PROBE_ROOT").expect("fresh root"));
    fs::create_dir(&root).unwrap();let root = root.canonicalize().unwrap();
    let config = Config { python: std::env::var_os("FRONTCAP_VOLUME_PYTHON").unwrap().into(),
        worker: std::env::var_os("FRONTCAP_VOLUME_WORKER").unwrap().into(), size_mib: 256, reserve_mib: 8 };
    let launch = LaunchId::generate();
    let limits = frontcap_core::write_budget::Limits { ordinary: 1024 * 1024, retirement: 1024 * 1024 };
    let budget = WriteBudget::create(&root.join("budget"), launch.clone(), limits).unwrap();
    let mut volume = SessionVolume::start(&root, &launch, &config, Instant::now() + Duration::from_secs(60), None, 0).unwrap();
    let executable = root.join("session.py");
    fs::write(&executable, format!("#!{}\nimport os,sys\nos.environ['FRONTCAP_PRE_ACQUISITION_WORKER']=sys.argv[1]\nos.environ['FRONTCAP_PRE_ACQUISITION_EXHAUST_ORDINARY']='{}'\nexe={}\nos.execv(exe,[exe,'--exact','native::session_volume::tests::pre_acquisition_worker_entry','--nocapture'])\n",
        config.python.display(), limits.ordinary, serde_json::to_string(&std::env::current_exe().unwrap()).unwrap())).unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let directory = volume.session.clone();let mut watched = false;
    let acquired = Session::acquire_with_observer(&executable, target(), &directory,
        Instant::now() + Duration::from_secs(30), Some(&budget), Some(&mut |session| {
            volume.watch_acquisition(session, Instant::now() + Duration::from_secs(10))?;watched = true;
            assert_eq!(budget.charged()?.ordinary, limits.ordinary);
            budget.charge(Class::Ordinary, 1)
        }));
    assert!(watched);let error = acquired.result.err().expect("ordinary admission must fail");
    assert!(error.to_string().contains("write allowance exhausted"));
    let frontcap_native::ios_session::StartupCleanup::AcquisitionNotStarted { worker } = acquired.ios_startup_cleanup else { panic!() };
    assert!(worker.has_exited().unwrap());assert!(!directory.join("target").exists());
    save_atomic(&root.join("startup-error.json"), &error.to_string()).unwrap();
    volume.retire_before_acquisition(&root, worker, Instant::now() + Duration::from_secs(60), Some(&budget)).unwrap();
    let proof: BeforeAcquisitionCompletion = load(&root.join("native-session-volume-before-acquisition-retirement.json")).unwrap().unwrap();
    frontcap_native::storage_volume::inspect_retained(&root.join("native-session-volume"), &config, &proof.volume).unwrap();
    assert_eq!(proof.launch_id, launch);assert_eq!(budget.charged().unwrap().ordinary, limits.ordinary);
    let mount = root.join("native-session-volume/worker/volume/mount");
    assert_eq!(mount.metadata().unwrap().dev(), root.metadata().unwrap().dev());
    assert!(fs::read_dir(&mount).unwrap().next().is_none());
    assert!(!root.join("native-session-volume-retirement.json").exists());
}


#[test]
fn managed_simulator_volume_requires_separate_storage_before_acquisition() {
    let mut target = target();
    let target::Launch::Ios(value) = &mut target else { unreachable!() };
    value.xctest.simulator.data_volume = Some(Config { size_mib: 8192, ..config() });
    assert!(validate(None, None, &target, 0).is_err());
    let target::Launch::Ios(value) = &mut target else { unreachable!() };
    value.resource_lifetime_ms = Some(600_000);
    validate(None, None, &target, 0).unwrap();
    assert!(validate(Some(&config()), None, &target, 0).is_err());
    let parent = Config { size_mib: 16384, ..config() };
    assert!(validate(Some(&parent), None, &target, storage::DEFAULT_MIN_FREE_BYTES).is_err());
    let external = tempfile::tempdir().unwrap();
    let target::Launch::Ios(value) = &mut target else { unreachable!() };
    value.xctest.simulator.data_volume_root = Some(external.path().canonicalize().unwrap().join("fresh"));
    validate(Some(&config()), None, &target, 32 * 1024 * 1024).unwrap();
}
