use super::*;
use crate::device::tests::Fixture;
use std::os::unix::fs::PermissionsExt;

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(8)
}
fn wait_for(path: &Path) {
    let end = deadline();
    while !path.exists() {
        assert!(Instant::now() < end, "missing {}", path.display());
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn wrapper(root: &Path) -> PathBuf {
    let script = root.join("session-worker");
    std::fs::write(&script, format!("#!/usr/bin/env python3\nimport os,sys\nos.environ['FRONTCAP_SESSION_TEST_DIRECTORY']=sys.argv[1]\nexe={}\nos.execv(exe,[exe,'--exact','session::tests::worker_entry','--nocapture'])\n",serde_json::to_string(&std::env::current_exe().unwrap()).unwrap())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    script
}
fn start(f: &Fixture) -> Session {
    Session::start(
        &wrapper(f.root()),
        f.config.clone(),
        &f.root().join("session"),
        deadline(),
    )
    .unwrap()
}
fn retired(root: &Path) -> SessionResult {
    let end = deadline();
    loop {
        if let Recovery::Finished { result, .. } = inspect(&root.join("session")).unwrap() {
            assert!(result.retirement.as_ref().unwrap().passed, "{result:?}");
            let identity: ProcessIdentity =
                load(&root.join("session/device/emulator-process.json"))
                    .unwrap()
                    .unwrap();
            assert!(identity.has_exited().unwrap());
            let host: serde_json::Value = load(&root.join("host/native-session.json"))
                .unwrap()
                .unwrap();
            assert_eq!(host["retired"], true);
            return result;
        }
        assert!(Instant::now() < end, "session failed to retire");
        std::thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn worker_entry() {
    let Ok(directory) = std::env::var("FRONTCAP_SESSION_TEST_DIRECTORY") else {
        return;
    };
    let directory = PathBuf::from(directory);
    run_with(&directory, |config, path, deadline, cancel| {
        Device::acquire_at(
            config,
            path,
            deadline,
            cancel,
            &directory.parent().unwrap().join("host"),
        )
    })
    .unwrap();
}

#[test]
fn view_host_transition_preserves_process_blocks_background_work_and_recovers_original_evidence() {
    use frontcap_core::native::{app_state::State, android_tap::Tap, android_views::Observation};
    let f = Fixture::view(false);
    f.record_view_lifecycle("ok");
    let path = f.root().join("source/views-before.json");
    let mut observed: Observation = load(&path).unwrap().unwrap();
    if let Observation::Observed {catalog, ..} = &mut observed { catalog.targets[0].tap_point_screen_px = Some([0,0]); }
    save_atomic(&path, &observed).unwrap();
    let budget=frontcap_core::write_budget::WriteBudget::create(&f.root().join("budget"),
        frontcap_core::ids::LaunchId::new("g-visibility").unwrap(),
        frontcap_core::write_budget::Limits {ordinary:16*1024*1024,retirement:1024*1024}).unwrap();
    let mut session = Session::start_with_budget(&wrapper(f.root()),f.config.clone(),
        &f.root().join("session"),deadline(),Some(&budget)).unwrap();
    let cancel = AtomicBool::new(false);
    let owner = session.wait_view_ready(deadline(), &cancel).unwrap();
    assert!(session.view_lifecycle_available().unwrap());
    let request_state:serde_json::Value=load(&f.root().join("session/requests/requests.json")).unwrap().unwrap();
    assert_eq!(request_state["budget_sha256"],budget.binding_sha256().unwrap());
    let root = f.root().join("session");
    let id = |s: &str| RequestId::new(s).unwrap();
    let ViewReply::Completed {..} = session.execute_view_operation(id("advance"), Operation::Advance {by_us:1234}, deadline(), &cancel).unwrap() else {panic!()};
    let ViewReply::Observed {capture} = session.observe_views(id("before"), deadline(), &cancel).unwrap() else {panic!()};
    let input = Operation::TapView {tap: Tap {observation:id("before"), source_observation:capture.after_request,
        reference:capture.catalog.targets[0].token.clone(), now_ns:capture.now_ns, point:[0,0]}};
    session.validate_view_operation(&id("input"), &input).unwrap();
    assert!(session.validate_view_state(&id("wrong"), "abcd0000-0000-0000-0000-000000000099", State::Background).is_err());
    assert!(session.validate_view_state(&id("same"), &owner.generation, State::Foreground).is_err());
    assert!(session.validate_view_state(&id("advance"), &owner.generation, State::Background).is_err());
    let crate::android_app_state::Reply::Completed {transition: background} = session.set_view_state(id("background"), owner.generation.clone(), State::Background, deadline(), &cancel).unwrap() else {panic!()};
    assert_eq!(background.process_before, background.process_after);
    assert_eq!(background.now_ns, None);
    assert!(session.validate_view_foreground().is_err());
    assert!(session.validate_view_operation(&id("read"), &Operation::ReadClock {}).is_err());
    assert!(session.validate_view_restart(&id("restart"), &owner.generation, None).is_err());
    assert!(session.validate_view_state(&id("again"), &owner.generation, State::Background).is_err());
    let crate::android_app_state::Reply::Completed {transition: foreground} = session.set_view_state(id("foreground"), owner.generation.clone(), State::Foreground, deadline(), &cancel).unwrap() else {panic!()};
    assert_eq!(foreground.now_ns, Some(1_234_000));
    assert!(session.validate_view_operation(&id("input"), &input).is_err());
    session.observe_views(id("fresh"), deadline(), &cancel).unwrap();
    let latest = std::fs::read(root.join("latest-view.json")).unwrap();
    let crate::android_app_state::Reply::Completed {transition: original} = session.set_view_state(id("background"), owner.generation.clone(), State::Background, deadline(), &cancel).unwrap() else {panic!()};
    assert_eq!(original, background);
    assert_eq!(std::fs::read(root.join("latest-view.json")).unwrap(), latest);
    let charged=budget.charged().unwrap();assert!(charged.ordinary>0);
    session.stop(deadline()).unwrap();
    retired(f.root());
    let history_path = root.join("app-state/history.json");
    let mut pending: serde_json::Value = load(&history_path).unwrap().unwrap();
    pending["entries"].as_array_mut().unwrap().truncate(1);
    pending["entries"][0]["completion"] = serde_json::Value::Null;
    save_atomic(&history_path, &pending).unwrap();
    let history = std::fs::read(&history_path).unwrap();
    assert_eq!(crate::android_app_state::read_completion(&root, &id("background"), &owner.generation, State::Background).unwrap(), Some(*background));
    assert_eq!(std::fs::read(&history_path).unwrap(), history);
    assert!(crate::android_app_state::require_foreground(&root).is_err());
    let proof_path = root.join("device/view-app-state/background/receipt.json");
    let proof: serde_json::Value = load(&proof_path).unwrap().unwrap();
    let number = proof["samples"]["after"][0].as_u64().unwrap();
    let raw = root.join(format!("device/command-{number}/exchange.json"));
    std::fs::write(raw, b"changed host source").unwrap();
    assert!(crate::android_app_state::read_completion(&root, &id("background"), &owner.generation, State::Background).is_err());
    assert_eq!(std::fs::read_to_string(f.root().join("host-effects.jsonl")).unwrap(), "background\nforeground\n");
}

#[test]
fn view_host_transition_fences_lost_effect_and_replaced_process_without_replay() {
    use frontcap_core::native::app_state::State;
    for mode in ["lost", "replaced", "between"] {
        let f = Fixture::view(false);
        f.record_view_lifecycle(mode);
        let mut session = start(&f);
        let cancel = AtomicBool::new(false);
        let owner = session.wait_view_ready(deadline(), &cancel).unwrap();
        let (id, state) = if mode == "between" {
            assert!(matches!(session.set_view_state(RequestId::new("home").unwrap(), owner.generation.clone(), State::Background, deadline(), &cancel).unwrap(), crate::android_app_state::Reply::Completed {..}));
            std::fs::write(f.root().join("replace-return"), b"").unwrap();
            (RequestId::new("return").unwrap(), State::Foreground)
        } else { (RequestId::new("home").unwrap(), State::Background) };
        assert!(matches!(session.set_view_state(id.clone(), owner.generation.clone(), state, deadline(), &cancel).unwrap(), crate::android_app_state::Reply::Failed {..}));
        retired(f.root());
        let root = f.root().join("session");
        assert!(crate::android_app_state::read_completion(&root, &id, &owner.generation, state).unwrap().is_none());
        assert!(session.validate_view_state(&id, &owner.generation, state).is_err());
        assert!(session.validate_view_state(&RequestId::new("retry").unwrap(), &owner.generation, state).is_err());
        assert!(session.validate_view_foreground().is_err());
        assert_eq!(std::fs::read_to_string(f.root().join("host-effects.jsonl")).unwrap(), "background\n");
    }
}

#[test]
fn view_restart_keeps_original_results_and_fresh_input_in_separate_app_epochs() {
    use frontcap_core::native::{android_tap::Tap, android_views::Observation, external_entry::{Request,Target}};
    let f = Fixture::view(false); f.record_view_restarts(); f.record_url_dispatches(false);
    let path = f.root().join("source/views-before.json");
    let mut observed: Observation = load(&path).unwrap().unwrap();
    if let Observation::Observed {catalog, ..} = &mut observed {catalog.targets[0].tap_point_screen_px=Some([0,0]);}
    save_atomic(&path, &observed).unwrap();
    let mut session = start(&f); let cancel=AtomicBool::new(false);
    let owner=session.wait_view_ready(deadline(), &cancel).unwrap();
    let root=f.root().join("session");
    let request=|s: &str|RequestId::new(s).unwrap();
    let clock=Operation::Advance {by_us:1000};
    let ViewReply::Completed {completion: old_clock}=session.execute_view_operation(request("old-clock"),clock.clone(),deadline(),&cancel).unwrap() else {panic!()};
    let url=Request {target:Target::Android {serial:owner.serial.clone(),package:owner.package.clone()},generation:owner.generation.clone(),url:"client-test://item/1".into()};
    let crate::external_entry::Reply::Accepted {acceptance}=session.dispatch_view_url(request("old-url"),url.clone(),deadline(),&cancel).unwrap() else {panic!()};
    let ViewReply::Observed {capture: old}=session.observe_views(request("old-basis"),deadline(),&cancel).unwrap() else {panic!()};
    let tap=|name:&str,c:&frontcap_core::native::android_capture::Capture| Operation::TapView {tap:Tap {
        observation:request(name),source_observation:c.after_request.clone(),reference:c.catalog.targets[0].token.clone(),now_ns:c.now_ns,point:[0,0]}};
    let old_input=tap("old-basis",&old);
    session.validate_view_operation(&request("unused-input"),&old_input).unwrap();
    assert!(session.validate_view_restart(&request("old-clock"),&owner.generation,None).is_err());
    assert!(session.validate_view_restart(&request("restart"),"abcd0000-0000-0000-0000-000000000099",None).is_err());
    let cold_url="client-test://item/1";
    let crate::android_restarts::Reply::Completed {replacement}=session.restart_views(request("restart"),owner.generation.clone(),Some(cold_url.to_owned()),deadline(),&cancel).unwrap() else {panic!()};
    assert_ne!(replacement.owner.generation,owner.generation);
    assert!(session.validate_view_operation(&request("unused-input"),&old_input).is_err());
    let ViewReply::Observed {capture: fresh}=session.observe_views(request("new-basis"),deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(fresh.now_ns,0);assert_eq!(fresh.pid,replacement.owner.pid);
    let fresh_input=tap("new-basis",&fresh);
    let crate::android_restarts::Reply::Completed {replacement:cached}=session.restart_views(request("restart"),owner.generation.clone(),Some(cold_url.to_owned()),deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(cached,replacement);
    assert_eq!(replacement.entry.as_ref().unwrap().request.url,cold_url);
    assert!(session.validate_view_restart(&request("restart"),&owner.generation,None).is_err());
    assert!(session.validate_view_restart(&request("restart"),&owner.generation,Some("client-test://item/2")).is_err());
    let ViewReply::Observed {capture:historical}=session.observe_views(request("old-basis"),deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(historical,old);
    let ViewReply::Completed {completion: historical}=session.execute_view_operation(request("old-clock"),clock,deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(historical,old_clock);
    let crate::external_entry::Reply::Accepted {acceptance:historical}=session.dispatch_view_url(request("old-url"),url,deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(historical,acceptance);
    session.validate_view_operation(&request("new-input"),&fresh_input).unwrap();
    let ViewReply::Completed {completion: new_clock}=session.execute_view_operation(request("new-clock"),Operation::ReadClock{},deadline(),&cancel).unwrap() else {panic!()};
    assert!(matches!(new_clock,Completion::Clock {request,now_ns:0,..} if request.request_id==1));
    let ViewReply::Completed {completion}=session.execute_view_operation(request("new-input"),fresh_input,deadline(),&cancel).unwrap() else {panic!()};
    assert!(matches!(completion,Completion::ViewTap{receipt,..} if receipt.pid==replacement.owner.pid && receipt.request.request_id==1));
    assert_eq!(std::fs::read_to_string(f.root().join("replacement-effects.jsonl")).unwrap(),"stop\nstart\n");
    session.stop(deadline()).unwrap(); retired(f.root());
    assert_eq!(read_view_observation(&root,&request("old-basis")).unwrap(),Some(old));
    assert_eq!(read_view_observation(&root,&request("new-basis")).unwrap(),Some(fresh));
    // Simulate an uncommitted history publication: the original device proof is sufficient for
    // result recovery, but must not publish a new epoch or grant process ownership.
    let history=root.join("restarts/history.json");
    let mut pending:serde_json::Value=load(&history).unwrap().unwrap();pending["entries"][0]["completion"]=serde_json::Value::Null;
    save_atomic(&history,&pending).unwrap();let bytes=std::fs::read(&history).unwrap();
    assert_eq!(crate::android_restarts::read_completion(&root,&request("restart"),&owner.generation,Some(cold_url)).unwrap(),Some(*replacement));
    assert!(crate::android_restarts::read_completion(&root,&request("restart"),&owner.generation,None).is_err());
    assert!(crate::android_restarts::current(&root).is_err());
    assert_eq!(std::fs::read(&history).unwrap(),bytes);
    let receipt=root.join("device/view-restarts/restart/receipt.json");
    let proof:serde_json::Value=load(&receipt).unwrap().unwrap();
    let command=proof["commands"].as_object().unwrap().keys().find(|p|p.ends_with("exchange.json")).unwrap();
    std::fs::write(root.join("device").join(command),b"changed receipt").unwrap();
    assert!(crate::android_restarts::read_completion(&root,&request("restart"),&owner.generation,Some(cold_url)).is_err());
}

#[test]
fn external_entry_invalidates_old_input_but_receipt_lookup_preserves_new_observation() {
    use frontcap_core::native::{android_tap::Tap, android_views::Observation, external_entry::{Request,Target}};
    let f = Fixture::view(false);f.record_url_dispatches(false);
    let path = f.root().join("source/views-before.json");
    let mut observed: Observation = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    if let Observation::Observed { catalog, .. } = &mut observed { catalog.targets[0].tap_point_screen_px = Some([0,0]); }
    std::fs::write(path, serde_json::to_vec(&observed).unwrap()).unwrap();
    let mut session = start(&f);let cancel = AtomicBool::new(false);
    let owner = session.wait_view_ready(deadline(), &cancel).unwrap();
    let basis = RequestId::new("before-link").unwrap();
    let ViewReply::Observed { capture } = session.observe_views(basis.clone(), deadline(), &cancel).unwrap() else { panic!() };
    let tap = Operation::TapView { tap: Tap { observation: basis, source_observation: capture.after_request,
        reference: capture.catalog.targets[0].token.clone(), now_ns: capture.now_ns, point: [0,0] } };
    let tap_id = RequestId::new("tap").unwrap();session.validate_view_operation(&tap_id, &tap).unwrap();
    let entry = Request { target: Target::Android { serial: owner.serial, package: owner.package },
        generation: owner.generation, url: "client-test://item/1".into() };
    let id = RequestId::new("link").unwrap();
    let crate::external_entry::Reply::Accepted { acceptance } = session.dispatch_view_url(id.clone(), entry.clone(), deadline(), &cancel).unwrap() else { panic!() };
    assert!(session.validate_view_operation(&tap_id, &tap).is_err(), "pre-link input stayed actionable");
    let basis = RequestId::new("after-link").unwrap();
    let ViewReply::Observed { capture } = session.observe_views(basis.clone(), deadline(), &cancel).unwrap() else { panic!() };
    let fresh = Operation::TapView { tap: Tap { observation: basis, source_observation: capture.after_request,
        reference: capture.catalog.targets[0].token.clone(), now_ns: capture.now_ns, point: [0,0] } };
    let crate::external_entry::Reply::Accepted { acceptance: cached } = session.dispatch_view_url(id.clone(), entry.clone(), deadline(), &cancel).unwrap() else { panic!() };
    assert_eq!(cached, acceptance);session.validate_view_operation(&tap_id, &fresh).unwrap();
    assert_eq!(std::fs::read_to_string(f.root().join("entry-effects.jsonl")).unwrap().lines().count(), 1);
    session.stop(deadline()).unwrap();retired(f.root());
    assert_eq!(crate::external_entry::read_accepted(&f.root().join("session/device/external-entry"), &id, &entry).unwrap(), Some(acceptance));
}
#[test]
fn controller_entry() {
    let Ok(root) = std::env::var("FRONTCAP_SESSION_CONTROLLER_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let phase = std::env::var("FRONTCAP_SESSION_CONTROLLER_PHASE").unwrap();
    let config: AndroidLaunch = load(&root.join("config.json")).unwrap().unwrap();
    let mut session = Session::start(
        &root.join("session-worker"),
        config,
        &root.join("session"),
        deadline(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    if phase != "acquisition" {
        session.wait_view_ready(deadline(), &cancel).unwrap();
        if phase == "dispatch" {
            std::fs::write(root.join("block"), b"block input").unwrap();
            let _ = session.execute_view_operation(
                RequestId::new("advance").unwrap(),
                Operation::Advance { by_us: 1000 },
                deadline(),
                &cancel,
            );
            panic!("controller should be killed during dispatch");
        }
        std::fs::write(root.join("idle-ready"), b"ready").unwrap();
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
#[test]
fn controller_process_group_loss_during_acquisition_idle_and_dispatch_retires_owned_target() {
    for phase in ["acquisition", "idle", "dispatch"] {
        let f = Fixture::view(false);
        wrapper(f.root());
        save_atomic(&f.root().join("config.json"), &f.config).unwrap();
        if phase == "acquisition" {
            std::fs::write(f.root().join("boot-block"), b"block boot").unwrap();
        }
        let mut controller = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "session::tests::controller_entry", "--nocapture"])
            .env("FRONTCAP_SESSION_CONTROLLER_ROOT", f.root())
            .env("FRONTCAP_SESSION_CONTROLLER_PHASE", phase)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_for(&f.root().join(match phase {
            "acquisition" => "boot-polled",
            "idle" => "idle-ready",
            _ => "advance-blocked",
        }));
        let owner: ProcessIdentity = load(&f.root().join("session/worker.json"))
            .unwrap()
            .unwrap();
        assert_ne!(owner.pid, controller.id());
        // The waitable direct controller pins the group identity while we signal it.
        assert_eq!(
            unsafe { libc::kill(-(controller.id() as i32), libc::SIGKILL) },
            0
        );
        controller.wait().unwrap();
        let result = retired(f.root());
        assert!(result.error.is_some());
        if phase == "acquisition" {
            assert!(!f.root().join("session/ready.json").exists());
        }
        if phase == "dispatch" {
            let request: serde_json::Value = load(&f.root().join("session/requests/requests.json"))
                .unwrap()
                .unwrap();
            assert_eq!(
                request["bindings"]["advance"]["progress"]["state"],
                "dispatched"
            );
            let effects = std::fs::read_to_string(f.root().join("clock-effects.jsonl")).unwrap();
            assert_eq!(effects.lines().count(), 1);
            assert!(!f.root().join("tap-effects.jsonl").exists());
            let exchange: crate::process::Exchange =
                crate::process::load_exchange(&f.root().join("session/commands/1/exchange/exchange.json"))
                    .unwrap()
                    .unwrap();
            assert!(exchange.failure.unwrap().contains("cancelled"));
            assert!(exchange.cleanup_failure.is_none());
        }
    }
}
#[test]
fn expired_acquisition_never_launches_and_restarting_recorded_worker_is_refused() {
    let f = Fixture::view(false);
    assert!(
        Session::start(
            &wrapper(f.root()),
            f.config.clone(),
            &f.root().join("session"),
            Instant::now()
        )
        .is_err()
    );
    assert!(!f.root().join("session/device").exists());
    // Allow the launched owner to finish its no-authority outcome before inspecting the record.
    let end = deadline();
    loop {
        if matches!(
            inspect(&f.root().join("session")).unwrap(),
            Recovery::Finished { .. }
        ) {
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        run_with(&f.root().join("session"), |_, _, _, _| panic!(
            "must not acquire"
        ))
        .is_err()
    );
}

#[test]
fn session_worker_death_keeps_unknown_target_fenced_without_pid_adoption() {
    let f = Fixture::view(false);
    let mut session = start(&f);
    session
        .wait_view_ready(deadline(), &AtomicBool::new(false))
        .unwrap();
    let device: ProcessIdentity = load(&f.root().join("session/device/emulator-process.json"))
        .unwrap()
        .unwrap();
    session.child.as_mut().unwrap().kill().unwrap();
    session.child.as_mut().unwrap().wait().unwrap();
    assert!(matches!(
        inspect(&f.root().join("session")).unwrap(),
        Recovery::Unresolved
    ));
    assert!(
        !device.has_exited().unwrap(),
        "test requires surviving target after owner death"
    );
    let mut replacement = Session::start(
        &wrapper(f.root()),
        f.config.clone(),
        &f.root().join("replacement"),
        deadline(),
    )
    .unwrap();
    let error = replacement
        .wait_view_ready(deadline(), &AtomicBool::new(false))
        .unwrap_err();
    assert!(error.to_string().contains("unretired"), "{error}");
    assert!(!f.root().join("replacement/device").exists());
    replacement.stop(deadline()).unwrap();
    // Only the fixture's private protocol retires this orphan, never a recorded PID signal.
    std::fs::write(f.root().join("retire-fake"), b"retire").unwrap();
    let end = deadline();
    while !device.has_exited().unwrap() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(5));
    }
    let host: serde_json::Value = load(&f.root().join("host/native-session.json"))
        .unwrap()
        .unwrap();
    assert_eq!(
        host["retired"], false,
        "the adapter must not silently clear an unknown owner fence"
    );
}

#[test]
fn request_store_creation_failure_after_acquisition_still_retires_device() {
    for views in [false,true] {
        let f = if views {Fixture::view(false)} else {Fixture::view(false)};
        std::fs::write(f.root().join("boot-block"), b"block boot").unwrap();
        let mut session = start(&f);
        wait_for(&f.root().join("boot-polled"));
        std::fs::create_dir(f.root().join("session/requests")).unwrap();
        std::fs::remove_file(f.root().join("boot-block")).unwrap();
        let cancel=AtomicBool::new(false);
        if views {assert!(session.wait_view_ready(deadline(),&cancel).is_err());}
        else {assert!(session.wait_view_ready(deadline(),&cancel).is_err());}
        let Recovery::Finished { result, .. } = session.stop(deadline()).unwrap() else {panic!("missing failure evidence")};
        assert!(result.error.is_some());
        assert!(result.retirement.as_ref().unwrap().passed);
        assert!(!f.root().join(if views {"session/view-ready.json"} else {"session/ready.json"}).exists());
        retired(f.root());
    }
}



#[test]
fn view_session_retries_published_capture_without_reading_again_and_recovers_after_disconnect() {
    let f=Fixture::view(false);
    let mut session=start(&f);
    let cancel=AtomicBool::new(false);
    let ready=session.wait_view_ready(deadline(),&cancel).unwrap();
    assert_eq!(ready.package,"org.example.client");
    let id=RequestId::new("view-first").unwrap();
    let ViewReply::Observed {capture}=session.observe_views(id.clone(),deadline(),&cancel).unwrap() else {panic!("missing capture")};
    let calls=std::fs::read(f.root().join("adb-calls.jsonl")).unwrap();
    let ViewReply::Observed {capture:replayed}=session.observe_views(id.clone(),deadline(),&cancel).unwrap() else {panic!("missing retained capture")};
    assert_eq!(capture,replayed);
    assert_eq!(std::fs::read(f.root().join("adb-calls.jsonl")).unwrap(),calls);
    drop(session);
    assert!(retired(f.root()).error.is_some());
    assert_eq!(read_view_observation(&f.root().join("session"),&id).unwrap(),Some(capture));
    assert_eq!(std::fs::read(f.root().join("adb-calls.jsonl")).unwrap(),calls);
    std::fs::write(f.root().join("session/views/view-first/collection/capture/screen.png"),b"changed").unwrap();
    assert!(read_view_observation(&f.root().join("session"),&id).is_err());
}

#[test]
fn view_session_preserves_failed_capture_and_retires_without_synthesizing_a_result() {
    let f=Fixture::view(false);
    let mut session=start(&f);
    let cancel=AtomicBool::new(false);
    session.wait_view_ready(deadline(),&cancel).unwrap();
    std::fs::write(f.root().join("source/window-before.txt"),b"mCurrentFocus=Window{abc u0 dev.other/.Main}\n").unwrap();
    let id=RequestId::new("view-failed").unwrap();
    assert!(matches!(session.observe_views(id.clone(),deadline(),&cancel).unwrap(),ViewReply::Failed {..}));
    assert!(retired(f.root()).error.is_some());
    let calls=std::fs::read(f.root().join("adb-calls.jsonl")).unwrap();
    assert!(read_view_observation(&f.root().join("session"),&id).unwrap().is_none());
    assert!(f.root().join("session/views/view-failed/collection/capture/progress.json").is_file());
    assert_eq!(std::fs::read(f.root().join("adb-calls.jsonl")).unwrap(),calls);
}

#[test]
fn view_clock_requests_advance_once_and_feed_fresh_captures() {
    let f=Fixture::view(false);let mut session=start(&f);let cancel=AtomicBool::new(false);
    let owner=session.wait_view_ready(deadline(),&cancel).unwrap();
    let id=RequestId::new("clock-step").unwrap();
    let operation=Operation::Advance {by_us:1000};
    let ViewReply::Completed {completion:first}=session.execute_view_operation(id.clone(),operation.clone(),deadline(),&cancel).unwrap() else {panic!("missing clock completion")};
    assert!(matches!(first,Completion::Clock {now_ns:1000000,applied:true,..}));
    let effects=std::fs::read(f.root().join("clock-effects.jsonl")).unwrap();
    let ViewReply::Completed {completion:retry}=session.execute_view_operation(id.clone(),operation.clone(),deadline(),&cancel).unwrap() else {panic!("missing cached clock")};
    assert_eq!(first,retry);assert_eq!(std::fs::read(f.root().join("clock-effects.jsonl")).unwrap(),effects);
    let ViewReply::Observed {capture}=session.observe_views(RequestId::new("after-clock").unwrap(),deadline(),&cancel).unwrap() else {panic!("missing capture")};
    assert_eq!(capture.now_ns,1000000);
    drop(session);assert!(retired(f.root()).retirement.unwrap().passed);
    let recovered=RequestStore::read_completion(&f.root().join("session/requests"),&owner.identity(),&owner.generation,&owner.source_sha256,&id,&operation).unwrap().unwrap();
    crate::receipt::verify(&f.root().join("session/device/view-clock/receipts"),&recovered).unwrap();
    assert_eq!(recovered,first);
}

#[test]
fn view_edit_lost_bridge_reply_is_reconciled_once_and_recovered_offline() {
    use frontcap_core::native::{android_edit::Edit,android_views::{Observation,Role}};
    let f=Fixture::view(false);
    let path=f.root().join("source/views-before.json");
    let mut observed:Observation=serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    if let Observation::Observed {catalog,..}=&mut observed {
        let field=&mut catalog.targets[0];field.role=Role::TextField;field.focused=true;
        field.edit_limit_utf16=Some(16384);field.text=Some("한글😀abc".into());
    }
    std::fs::write(path,serde_json::to_vec(&observed).unwrap()).unwrap();
    let mut session=start(&f);let cancel=AtomicBool::new(false);
    let owner=session.wait_view_ready(deadline(),&cancel).unwrap();
    let observation=RequestId::new("edit-basis").unwrap();
    let ViewReply::Observed {capture}=session.observe_views(observation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    let edit=Edit {observation,source_observation:capture.after_request.clone(),reference:capture.catalog.targets[0].token.clone(),
        now_ns:capture.now_ns,before:"한글😀abc".into(),start:4,end:7,text:"수정".into()};
    let id=RequestId::new("edit-once").unwrap();let operation=Operation::ReplaceViewText {edit:edit.clone()};
    let mut invalid=edit.clone();invalid.start=3;
    assert!(session.validate_view_operation(&id,&Operation::ReplaceViewText {edit:invalid}).is_err());
    assert!(!f.root().join("edit-effects.jsonl").exists());
    session.validate_view_operation(&id,&operation).unwrap();
    std::fs::write(f.root().join("edit-lost-reply"),b"lose acknowledgement after publishing").unwrap();
    let ViewReply::Completed {completion:first}=session.execute_view_operation(id.clone(),operation.clone(),deadline(),&cancel).unwrap() else {panic!("missing edit")};
    assert!(matches!(&first,Completion::ViewText {receipt,..} if receipt.after.as_deref()==Some("한글😀수정")));
    let effects=std::fs::read(f.root().join("edit-effects.jsonl")).unwrap();
    let ViewReply::Completed {completion:retry}=session.execute_view_operation(id.clone(),operation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(first,retry);assert_eq!(std::fs::read(f.root().join("edit-effects.jsonl")).unwrap(),effects);
    assert!(session.validate_view_operation(&RequestId::new("stale-new-id").unwrap(),&operation).is_err());
    let ViewReply::Observed {capture:after}=session.observe_views(RequestId::new("after-edit").unwrap(),deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(after.catalog.targets[0].text.as_deref(),Some("한글😀수정"));
    drop(session);assert!(retired(f.root()).retirement.unwrap().passed);
    let recovered=RequestStore::read_completion(&f.root().join("session/requests"),&owner.identity(),&owner.generation,&owner.source_sha256,&id,&operation).unwrap().unwrap();
    crate::receipt::verify(&f.root().join("session/device/view-edit/receipts"),&recovered).unwrap();assert_eq!(recovered,first);
}


#[test]
fn view_tap_lost_reply_and_rejection_keep_exact_results_without_another_touch() {
    use frontcap_core::native::{android_tap::{Tap,State},android_views::Observation};
    let f=Fixture::view(false);let path=f.root().join("source/views-before.json");
    let mut observed:Observation=serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    if let Observation::Observed {catalog,..}=&mut observed {catalog.targets[0].tap_point_screen_px=Some([0,0]);}
    std::fs::write(path,serde_json::to_vec(&observed).unwrap()).unwrap();
    let mut session=start(&f);let cancel=AtomicBool::new(false);
    let owner=session.wait_view_ready(deadline(),&cancel).unwrap();
    let observation=RequestId::new("tap-basis").unwrap();
    let ViewReply::Observed {capture}=session.observe_views(observation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    let mut tap=Tap {observation,source_observation:capture.after_request.clone(),reference:capture.catalog.targets[0].token.clone(),now_ns:0,point:[0,0]};
    let id=RequestId::new("tap-once").unwrap();let mut operation=Operation::TapView {tap:tap.clone()};
    let mut invalid=tap.clone();invalid.point=[1,0];
    assert!(session.validate_view_operation(&id,&Operation::TapView {tap:invalid}).is_err());
    assert!(!f.root().join("tap-effects.jsonl").exists());
    let ViewReply::Completed {..}=session.execute_view_operation(RequestId::new("before-tap-clock").unwrap(),Operation::Advance {by_us:1},deadline(),&cancel).unwrap() else {panic!()};
    assert!(session.validate_view_operation(&id,&operation).is_err());
    let observation=RequestId::new("after-clock-basis").unwrap();
    let ViewReply::Observed {capture}=session.observe_views(observation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    tap.observation=observation;tap.source_observation=capture.after_request;tap.now_ns=capture.now_ns;
    operation=Operation::TapView {tap:tap.clone()};
    std::fs::write(f.root().join("tap-lost-reply"),b"lose adb reply after touch").unwrap();
    let ViewReply::Completed {completion:first}=session.execute_view_operation(id.clone(),operation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert!(matches!(&first,Completion::ViewTap {receipt,..} if receipt.state==State::Completed));
    let effects=std::fs::read(f.root().join("tap-effects.jsonl")).unwrap();
    let ViewReply::Completed {completion:retry}=session.execute_view_operation(id.clone(),operation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(first,retry);assert_eq!(std::fs::read(f.root().join("tap-effects.jsonl")).unwrap(),effects);
    assert!(session.validate_view_operation(&RequestId::new("stale-tap").unwrap(),&operation).is_err());
    let observation=RequestId::new("after-tap").unwrap();
    let ViewReply::Observed {capture}=session.observe_views(observation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert!(capture.catalog.targets[0].selected);
    std::fs::write(f.root().join("tap-rejected"),b"source changed before delivery").unwrap();
    let rejected=Operation::TapView {tap:Tap {observation,source_observation:capture.after_request.clone(),..tap}};
    let rejected_id=RequestId::new("rejected-tap").unwrap();
    let ViewReply::Completed {completion:no_effect}=session.execute_view_operation(rejected_id.clone(),rejected.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert!(matches!(&no_effect,Completion::ViewTap {receipt,..} if receipt.state==State::Rejected && receipt.request.request_id==2));
    let ViewReply::Completed {completion:retry}=session.execute_view_operation(rejected_id.clone(),rejected.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(no_effect,retry);assert_eq!(std::fs::read(f.root().join("tap-effects.jsonl")).unwrap(),effects);
    drop(session);assert!(retired(f.root()).retirement.unwrap().passed);
    for (id,op,expected) in [(id,operation,first),(rejected_id,rejected,no_effect)] {
        let recovered=RequestStore::read_completion(&f.root().join("session/requests"),&owner.identity(),&owner.generation,&owner.source_sha256,&id,&op).unwrap().unwrap();
        crate::receipt::verify(&f.root().join("session/device/view-tap/receipts"),&recovered).unwrap();assert_eq!(recovered,expected);
    }
}


#[test]
fn view_source_change_returns_retained_unavailable_and_allows_a_new_read_in_the_same_session() {
    let f=Fixture::view(false);let mut session=start(&f);let cancel=AtomicBool::new(false);
    session.wait_view_ready(deadline(),&cancel).unwrap();
    let previous=RequestId::new("before-change").unwrap();
    let ViewReply::Observed {..}=session.observe_views(previous,deadline(),&cancel).unwrap() else {panic!()};
    std::fs::write(f.root().join("change-on-screen"),b"IME change").unwrap();
    let id=RequestId::new("during-change").unwrap();
    let ViewReply::Unavailable {observation}=session.observe_views(id.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert!(read_view_observation(&f.root().join("session"),&id).is_err());
    let marker:Option<Option<RequestId>>=load(&f.root().join("session/latest-view.json")).unwrap();assert_eq!(marker,Some(None));
    let ViewReply::Unavailable {observation:retry}=session.observe_views(id.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(observation,retry);
    let fresh=RequestId::new("after-change").unwrap();
    let ViewReply::Observed {capture}=session.observe_views(fresh,deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(capture.catalog.ime_visible,Some(true));
    drop(session);assert!(retired(f.root()).retirement.unwrap().passed);
    assert_eq!(read_view_result(&f.root().join("session"),&id).unwrap(),Some(frontcap_core::native::android_capture::Collection::Unavailable(observation)));
}


#[test]
fn view_scroll_recovery_keeps_independent_sequences_and_rejects_consumed_observations() {
    use frontcap_core::native::{android_scroll::{Scroll,Geometry,Gesture,Direction},android_tap::{Tap,State},android_views::{Observation,Role}};
    let f=Fixture::view(false);let path=f.root().join("source/views-before.json");
    let mut observed:Observation=serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let gesture=Gesture {direction:Direction::Down,start_px:[0,1],end_px:[0,0]};
    if let Observation::Observed {catalog,..}=&mut observed {
        let mut button=catalog.targets[0].clone();button.token="abcd0000-0000-0000-0000-000000000004".into();button.tap_point_screen_px=Some([0,0]);
        let view=&mut catalog.targets[0];view.role=Role::ScrollView;view.class_name="android.widget.ScrollView".into();
        view.layout_bounds_screen_px=[0,0,2,2];view.scroll=Some(Geometry {offset_px:[0,0],maximum_px:[0,100],viewport_px:[2,2],gestures:vec![gesture.clone()]});
        catalog.targets.push(button);
    }
    std::fs::write(path,serde_json::to_vec(&observed).unwrap()).unwrap();
    let mut png=Vec::new();{
        let mut encoder=png::Encoder::new(&mut png,2,2);encoder.set_color(png::ColorType::Rgb);encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(&[0;12]).unwrap();
    }
    std::fs::write(f.root().join("source/screen.png"),png).unwrap();
    let mut session=start(&f);let cancel=AtomicBool::new(false);let owner=session.wait_view_ready(deadline(),&cancel).unwrap();
    let observation=RequestId::new("scroll-basis").unwrap();
    let ViewReply::Observed {capture}=session.observe_views(observation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    let scroll=Scroll {observation,source_observation:capture.after_request.clone(),reference:capture.catalog.targets[0].token.clone(),
        now_ns:0,gesture,before_offset_px:[0,0]};
    let id=RequestId::new("scroll-once").unwrap();let operation=Operation::ScrollView {scroll:scroll.clone()};
    let mut invalid=scroll.clone();invalid.gesture.end_px=[1,0];
    assert!(session.validate_view_operation(&id,&Operation::ScrollView {scroll:invalid}).is_err());
    assert!(!f.root().join("scroll-effects.jsonl").exists());
    std::fs::write(f.root().join("scroll-lost-reply"),b"lose after movement").unwrap();
    let ViewReply::Completed {completion:first}=session.execute_view_operation(id.clone(),operation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert!(matches!(&first,Completion::ViewScroll {receipt,..} if receipt.request.request_id==1 && receipt.after_offset_px==Some([0,50])));
    let effects=std::fs::read(f.root().join("scroll-effects.jsonl")).unwrap();
    let ViewReply::Completed {completion:retry}=session.execute_view_operation(id.clone(),operation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(first,retry);assert_eq!(std::fs::read(f.root().join("scroll-effects.jsonl")).unwrap(),effects);
    assert!(session.validate_view_operation(&RequestId::new("stale-scroll").unwrap(),&operation).is_err());
    let observation=RequestId::new("after-scroll").unwrap();
    let ViewReply::Observed {capture}=session.observe_views(observation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    let tap=Tap {observation,source_observation:capture.after_request,reference:capture.catalog.targets[1].token.clone(),now_ns:0,point:[0,0]};
    let ViewReply::Completed {completion}=session.execute_view_operation(RequestId::new("tap-after-scroll").unwrap(),Operation::TapView {tap},deadline(),&cancel).unwrap() else {panic!()};
    assert!(matches!(completion,Completion::ViewTap {receipt,..} if receipt.request.request_id==1));
    let observation=RequestId::new("next-scroll-basis").unwrap();
    let ViewReply::Observed {capture}=session.observe_views(observation.clone(),deadline(),&cancel).unwrap() else {panic!()};
    let rejected=Operation::ScrollView {scroll:Scroll {observation,source_observation:capture.after_request,before_offset_px:[0,50],..scroll}};
    let rejected_id=RequestId::new("scroll-rejected").unwrap();std::fs::write(f.root().join("scroll-rejected"),b"no effect").unwrap();
    let ViewReply::Completed {completion:no_effect}=session.execute_view_operation(rejected_id.clone(),rejected.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert!(matches!(&no_effect,Completion::ViewScroll {receipt,..} if receipt.state==State::Rejected && receipt.request.request_id==2));
    let ViewReply::Completed {completion:retry}=session.execute_view_operation(rejected_id.clone(),rejected.clone(),deadline(),&cancel).unwrap() else {panic!()};
    assert_eq!(retry,no_effect);assert_eq!(std::fs::read(f.root().join("scroll-effects.jsonl")).unwrap(),effects);
    drop(session);assert!(retired(f.root()).retirement.unwrap().passed);
    for (id,op,expected) in [(id,operation,first),(rejected_id,rejected,no_effect)] {
        let recovered=RequestStore::read_completion(&f.root().join("session/requests"),&owner.identity(),&owner.generation,&owner.source_sha256,&id,&op).unwrap().unwrap();
        crate::receipt::verify(&f.root().join("session/device/view-scroll/receipts"),&recovered).unwrap();assert_eq!(recovered,expected);
    }
}

#[test]
fn exhausted_android_session_preserves_requests_and_can_retire() {
    use frontcap_core::{ids::LaunchId,write_budget::{Class,Limits,WriteBudget}};
    let f=Fixture::view(false);
    let budget=WriteBudget::create(&f.root().join("budget"),LaunchId::new("g-exhausted").unwrap(),
        Limits {ordinary:16*1024*1024,retirement:1024*1024}).unwrap();
    let mut session=Session::start_with_budget(&wrapper(f.root()),f.config.clone(),
        &f.root().join("session"),deadline(),Some(&budget)).unwrap();
    let cancel=AtomicBool::new(false);session.wait_view_ready(deadline(),&cancel).unwrap();
    let first=RequestId::new("first").unwrap();
    let answer=session.execute_view_operation(first.clone(),Operation::ReadClock {},deadline(),&cancel).unwrap();
    assert!(matches!(answer,ViewReply::Completed {..}));
    let path=f.root().join("session/requests/requests.json");let original=std::fs::read(&path).unwrap();
    budget.charge(Class::Ordinary,16*1024*1024-budget.charged().unwrap().ordinary).unwrap();
    let refused=session.execute_view_operation(RequestId::new("refused").unwrap(),Operation::ReadClock {},deadline(),&cancel);
    assert!(!matches!(refused,Ok(ViewReply::Completed {..})),"{refused:?}");
    assert_eq!(std::fs::read(&path).unwrap(),original);
    session.stop(deadline()).unwrap();retired(f.root());
    assert_eq!(std::fs::read(&path).unwrap(),original);
    assert_eq!(budget.charged().unwrap().ordinary,16*1024*1024);
}
