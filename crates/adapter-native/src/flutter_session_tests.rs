use super::*;
fn launch(root: &Path) -> Launch {
    let script = root.join("fixture.py");
    std::fs::write(&script,r#"import fcntl,json,pathlib,sys
config=json.loads(pathlib.Path(sys.argv[1]).read_text());out=pathlib.Path(sys.argv[2]);out.mkdir()
with (out/'session.lock').open('w') as lock:
 fcntl.flock(lock,fcntl.LOCK_EX)
 assert json.loads(sys.stdin.readline())=={'command':'start'}
 value=dict(owner=dict(platform=config['platform'],device='fixture-device',generation='a'*48,source_sha256=config['source_sha256']),endpoint=dict(port=12345,token='b'*48))
 temp=out/'ready.tmp';temp.write_text(json.dumps(value));temp.replace(out/'ready.json')
 line=sys.stdin.readline();state='stopped' if line else 'interrupted'
 value=dict(state=state,retirement=dict(passed=True,errors=[],signal_error=None),error=None)
 temp=out/'result.tmp';temp.write_text(json.dumps(value));temp.replace(out/'result.json')
"#).unwrap();
    let python = std::env::var_os("FRONTCAP_TEST_PYTHON")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/bin/python3"));
    Launch {
        python,
        worker_script: script,
        config: Config {
            flutter: Some(root.join("flutter")),
            project: Some(root.into()),
            platform: Platform::Ios,
            source_sha256: "c".repeat(64),
            acquisition_timeout_s: 30,
            lifetime_s: 30,
            android_sdk: None,
            avd: None,
            runtime: None,
            app_defines: Default::default(),
            system_ui: None,
            installed_app: None,
            initial_data: None,
            data_volume: None,
            resource_executable: None, android_resources: None,
        },
    }
}
#[test]
#[cfg(target_os = "macos")]
fn android_recovery_requires_original_ports_volume_and_private_server() {
    let temp = tempfile::tempdir().unwrap(); let root = temp.path();
    let worker = root.join("worker"); let group = worker.join("resource-group");
    std::fs::create_dir_all(&group).unwrap(); File::create_new(worker.join("session.lock")).unwrap();
    let volume = serde_json::json!({"python":"/python","worker":"/volume","size_mib":4096,"reserve_mib":64});
    let config = serde_json::json!({"platform":"android","source_sha256":"a".repeat(64),
        "acquisition_timeout_s":120,"lifetime_s":60,"android_sdk":"/sdk","avd":"Phone","resource_executable":"/owner",
        "installed_app":{"product":"/client.apk","apk_sha256":"a".repeat(64),"package":"dev.client","activity":"dev.client.Main"},
        "android_resources":{"avd_home":"/avd","avd_inventory":{"files":{},"directories":[]},
            "port":5580,"adb_server_port":15037,"scratch_volume":volume}});
    save_atomic(&root.join("config.json"), &config).unwrap();
    let original = serde_json::json!({"host":root,"target":worker.join("resource-target"),"evidence_root":worker,
        "resource_executable":"/owner","resource_arguments":["--resource-process"],"ports":[5580,5581],
        "deadline_ns":1,"scratch_volume":volume});
    save_atomic(&group.join("config.json"), &original).unwrap();
    assert!(matches!(inspect(root).unwrap(), Recovery::Unresolved));
    for (key, value) in [("ports", serde_json::json!([5582,5583])),
        ("scratch_volume_root", serde_json::json!(root.join("foreign"))),
        ("scratch_volume", serde_json::json!(null)), ("temporary_storage_root", serde_json::json!("/foreign"))] {
        let mut changed = original.clone(); changed[key] = value;
        save_atomic(&group.join("config.json"), &changed).unwrap();
        assert!(inspect(root).is_err());
    }
    save_atomic(&group.join("config.json"), &original).unwrap();
    save_atomic(&group.join("adb-server.json"), &serde_json::json!({"id":"adb","port":15038})).unwrap();
    assert!(inspect(root).err().unwrap().to_string().contains("private server"));
    assert!(!worker.join("result.json").exists());
}

#[test]
fn installed_product_starts_without_build_sdk_or_source_paths() {
    let temp = tempfile::tempdir().unwrap();
    let mut input = launch(temp.path());
    input.config.flutter = None;
    input.config.project = None;
    let product = temp.path().join("Client.app");
    std::fs::create_dir(&product).unwrap();
    let info = product.join("Info.plist");
    std::fs::write(&info, b"frozen fixture product").unwrap();
    input.config.installed_app = Some(crate::flutter_app::Launch::Ios(crate::flutter_ios_app::Launch {
        product,
        inventory: crate::flutter_ios_app::Inventory {
            files: [("Info.plist".into(), crate::products::read_file(&info, None).unwrap())].into(),
            directories: Default::default(),
        },
    }));
    let dir = temp.path().join("session");
    let budget = frontcap_core::write_budget::WriteBudget::create(&temp.path().join("budget"),
        frontcap_core::ids::LaunchId::generate(), frontcap_core::write_budget::Limits { ordinary: 1 << 20, retirement: 1 << 20 }).unwrap();
    let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tools");
    let worker = std::fs::read_to_string(&input.worker_script).unwrap();
    std::fs::write(&input.worker_script, format!("import sys\nsys.path.insert(0, {})\nfrom native_write_budget import charge_payload\ncharge_payload(3)\n{}",
        serde_json::to_string(&tools.to_string_lossy()).unwrap(), worker)).unwrap();
    let mut session = Session::start_with_budget(input, &dir, Instant::now() + Duration::from_secs(5), Some(&budget)).unwrap();
    session.wait_ready(Instant::now() + Duration::from_secs(5), &AtomicBool::new(false)).unwrap();
    let Recovery::Finished(result) = session.stop(Instant::now() + Duration::from_secs(5)).unwrap() else { panic!() };
    assert!(result.retirement.passed);
    let spent = budget.charged().unwrap();
    assert!(spent.ordinary > 3 && spent.retirement > 0);
}
#[test]
fn source_execution_rejects_missing_build_paths_before_starting_worker() {
    let temp = tempfile::tempdir().unwrap();
    for missing_sdk in [true, false] {
        let mut input = launch(temp.path());
        if missing_sdk { input.config.flutter = None; } else { input.config.project = None; }
        let dir = temp.path().join("session");
        assert!(Session::start(input, &dir, Instant::now() + Duration::from_secs(5)).is_err());
        assert!(!dir.exists());
    }
}
#[test]
fn explicit_stop_retires_the_live_owned_worker() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let mut session = Session::start(
        launch(temp.path()),
        &dir,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    session
        .wait_ready(
            Instant::now() + Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(matches!(inspect(&dir).unwrap(), Recovery::Active));
    let Recovery::Finished(result) = session
        .stop(Instant::now() + Duration::from_secs(5))
        .unwrap()
    else {
        panic!()
    };
    assert!(matches!(result.state, State::Stopped));
    assert!(result.retirement.passed);
}
#[test]
fn dropping_authority_closes_pipe_without_adopting_saved_pid() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let mut session = Session::start(
        launch(temp.path()),
        &dir,
        Instant::now() + Duration::from_secs(5),
    )
    .unwrap();
    session
        .wait_ready(
            Instant::now() + Duration::from_secs(5),
            &AtomicBool::new(false),
        )
        .unwrap();
    drop(session);
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Recovery::Finished(result) = inspect(&dir).unwrap() {
            assert!(matches!(result.state, State::Interrupted));
            assert!(result.retirement.passed);
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(10));
    }
}
