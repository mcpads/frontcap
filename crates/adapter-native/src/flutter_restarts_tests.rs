use super::*;
use frontcap_core::native::flutter::{Owner, Platform};

use crate::flutter_ios_fixture::acquisition;

fn setup(root: &Path) -> (History, Ready, crate::flutter_ios_app::Acquired) {
    setup_with_budget(root, None)
}
fn setup_with_budget(root: &Path, budget: Option<&WriteBudget>) -> (History, Ready, crate::flutter_ios_app::Acquired) {
    setup_with_system(root, budget, false)
}
fn setup_with_system(root: &Path, budget: Option<&WriteBudget>, system: bool) -> (History, Ready, crate::flutter_ios_app::Acquired) {
    let (mut ready, app) = acquisition(root, &"a".repeat(48), 101);
    if system { ready.system_ui=Some(frontcap_core::native::ios_ui::UiIdentity {
        simulator:ready.owner.device.clone(),generation:"abcd0000-0000-0000-0000-000000000002".into(),pid:202}); }
    std::fs::create_dir_all(root.join("session/worker")).unwrap();
    save_atomic(&root.join("ready.json"), &ready.owner).unwrap();
    save_atomic(&root.join("session/worker/ready.json"), &ready).unwrap();
    save_atomic(&root.join("session/worker/application.json"), &app).unwrap();
    (
        History::create_with_budget(root, ready.clone(), Acquired::Ios(app.clone()), budget).unwrap(),
        ready,
        app,
    )
}
fn worker_result(root: &Path, number: u64, result: &Restarted) {
    let path = root
        .join("session/worker/commands")
        .join(number.to_string());
    std::fs::create_dir_all(&path).unwrap();
    save_atomic(&path.join("request.json"), &serde_json::json!({
        "operation":"restart_app", "generation":match &result.previous { Acquired::Ios(app) => &app.endpoint.generation, Acquired::Android(app) => &app.endpoint.generation }, "number":number, "timeout_ms":5000, "url":result.entry.as_ref().map(|entry| &entry.request.url)
    })).unwrap();
    save_atomic(&path.join("command.log"), result).unwrap();
}

#[test]
fn original_worker_result_is_readable_without_promoting_or_replaying_pending_restart() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut history, ready, app) = setup(root);
    let id = RequestId::new("restart").unwrap();
    history
        .dispatch(id.clone(), ready.owner.generation.clone(), 3, None)
        .unwrap();
    assert!(
        read_completion(root, &id, &ready.owner.generation, None)
            .unwrap()
            .is_none()
    );
    let (next, current) = acquisition(root, &"b".repeat(48), 102);
    let result = Restarted {
        previous: Acquired::Ios(app),
        current: Acquired::Ios(current),
        ready: next, entry: None,
    };
    worker_result(root, 3, &result);
    let original = std::fs::read(root.join("restarts/history.json")).unwrap();
    let replacement = read_completion(root, &id, &ready.owner.generation, None)
        .unwrap()
        .unwrap();
    assert_eq!(replacement.owner(), &result.ready.owner);
    assert_eq!(epochs(root).unwrap().len(), 1); // No live new owner is manufactured by recovery.
    assert!(history.ensure_resolved().is_err());
    assert!(
        history
            .dispatch(
                RequestId::new("retry").unwrap(),
                ready.owner.generation.clone(),
                4, None
            )
            .is_err()
    );
    assert_eq!(
        std::fs::read(root.join("restarts/history.json")).unwrap(),
        original
    );
    let mut changed = result.clone();
    let Acquired::Ios(changed_app) = &mut changed.current else { unreachable!() };
    changed_app.containers.data_inode += 1;
    worker_result(root, 3, &changed);
    assert!(read_completion(root, &id, &ready.owner.generation, None).is_err());
    assert!(history.validate_completion(&changed).is_err());
    assert!(read_completion(root, &id, &"c".repeat(48), None).is_err());
}

#[test]
fn replacement_epochs_preserve_original_owners_and_reject_reused_generations() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let (mut history, initial, mut app) = setup(root);
    let mut ready = initial.clone();
    for (index, generation) in ["b".repeat(48), "c".repeat(48)].into_iter().enumerate() {
        let id = RequestId::new(format!("restart-{index}")).unwrap();
        history
            .dispatch(id.clone(), ready.owner.generation.clone(), index as u64 + 1, None)
            .unwrap();
        let (next, current) = acquisition(root, &generation, 102 + index as u32);
        let result = Restarted {
            previous: Acquired::Ios(app.clone()),
            current: Acquired::Ios(current),
            ready: next, entry: None,
        };
        worker_result(root, index as u64 + 1, &result);
        let directory = epoch_directory(root, &generation);
        std::fs::create_dir_all(&directory).unwrap();
        save_atomic(&directory.join("ready.json"), &result.ready.owner).unwrap();
        history.complete(&id, result.clone()).unwrap();
        history.ensure_resolved().unwrap();
        let Acquired::Ios(current) = result.current else { unreachable!() };
        app = current;
        ready = result.ready;
    }
    let all = epochs(root).unwrap();
    assert_eq!(
        all.iter()
            .map(|e| e.0.owner.generation.clone())
            .collect::<Vec<_>>(),
        vec!["a".repeat(48), "b".repeat(48), "c".repeat(48)]
    );
    assert_eq!(
        required::<Owner>(&root.join("ready.json")).unwrap(),
        initial.owner
    );
    let original = read_completion(
        root,
        &RequestId::new("restart-0").unwrap(),
        &initial.owner.generation, None,
    )
    .unwrap()
    .unwrap();
    assert_eq!(original.owner().generation, "b".repeat(48));
    let (old, acquired) = acquisition(root, &initial.owner.generation, 104);
    assert!(
        history
            .validate_completion(&Restarted {
                previous: Acquired::Ios(app),
                current: Acquired::Ios(acquired),
                ready: old, entry: None
            })
            .is_err()
    );
    let path = root.join("session/worker/commands/1/command.log");
    std::fs::remove_file(path).unwrap();
    assert!(epochs(root).is_err());
}

#[test]
fn android_replacement_binds_guest_birth_installation_and_forwarded_endpoint() {
    use crate::flutter_android_app::{Acquired as App, Forwarding};
    use frontcap_core::native::{android_lifecycle::{Installation, Process}, external_entry::Target};
    let temp = tempfile::tempdir().unwrap(); let root = temp.path();
    let acquire = |generation: &str, pid: u32, port: u16| {
        let ready = Ready { owner: Owner { platform: Platform::Android, device: "emulator-5580".into(),
            generation: generation.repeat(48), source_sha256: "f".repeat(64) },
            endpoint: crate::flutter_bridge::Endpoint { port, token: "a".repeat(48) }, system_ui: None,
            external_url_target: Some(Target::Android { serial: "emulator-5580".into(), package: "org.example.client".into() }) };
        let app = App { serial: "emulator-5580".into(), package: "org.example.client".into(), activity: "org.example.client.Main".into(),
            installation: Installation { package: "org.example.client".into(), apk: "/data/app/package/base.apk".into(),
                apk_sha256: "b".repeat(64), data: "/data/user/0/org.example.client".into(), data_device: 1, data_inode: 42 },
            process: Process { pid, boot_id: "abcd0000-0000-0000-0000-000000000001".into(), start_ticks: pid as u64, command: "org.example.client".into() },
            endpoint: crate::flutter_ios_app::Endpoint { generation: ready.owner.generation.clone(),token:ready.endpoint.token.clone(),port:23456,scenario:"session".into() },
            forwarding: Forwarding { serial: "emulator-5580".into(),host_port:port,device_port:23456 } };
        (ready, app)
    };
    let (before, app) = acquire("a", 100, 30000);
    std::fs::create_dir_all(root.join("session/worker")).unwrap();
    save_atomic(&root.join("ready.json"), &before.owner).unwrap();
    save_atomic(&root.join("session/worker/ready.json"), &before).unwrap();
    save_atomic(&root.join("session/worker/application.json"), &app).unwrap();
    let mut history = History::create(root, before.clone(), Acquired::Android(app.clone())).unwrap();
    let url = "client-test://item/1";
    let id = RequestId::new("restart").unwrap();history.dispatch(id.clone(),before.owner.generation.clone(),1, Some(url.into())).unwrap();
    let (after, current) = acquire("b", 101, 30001);
    let result = Restarted { previous:Acquired::Android(app.clone()),current:Acquired::Android(current.clone()),ready:after.clone(), entry: Some(frontcap_core::native::external_entry::Accepted::new(
        frontcap_core::native::flutter_lifecycle::restart_entry(&before.owner,before.external_url_target.as_ref(),Some(url)).unwrap().unwrap())) };
    worker_result(root,1,&result);
    let original = std::fs::read(root.join("restarts/history.json")).unwrap();
    assert_eq!(read_completion(root,&id,&before.owner.generation, Some(url)).unwrap().unwrap().owner(),&after.owner);
    assert!(read_completion(root,&id,&before.owner.generation, None).is_err());
    assert!(read_completion(root,&id,&before.owner.generation, Some("client-test://item/2")).is_err());
    let mut wrong=result.clone();wrong.entry.as_mut().unwrap().request.url="client-test://item/2".into();
    assert!(history.validate_completion(&wrong).is_err());
    wrong=result.clone();wrong.entry.as_mut().unwrap().request.generation=after.owner.generation.clone();
    assert!(history.validate_completion(&wrong).is_err());
    assert_eq!(std::fs::read(root.join("restarts/history.json")).unwrap(),original);
    assert_eq!(epochs(root).unwrap().len(),1);
    for change in ["inode","apk","boot","forward","process"] {
        let mut altered=current.clone();
        match change {
            "inode" => altered.installation.data_inode+=1,
            "apk" => altered.installation.apk_sha256="c".repeat(64),
            "boot" => altered.process.boot_id="abcd0000-0000-0000-0000-000000000002".into(),
            "forward" => altered.forwarding.device_port+=1,
            "process" => altered.process=app.process.clone(),
            _ => unreachable!(),
        }
        assert!(Restarted {previous:result.previous.clone(),current:Acquired::Android(altered),ready:after.clone(), entry: None}
            .validate(&Acquired::Android(app.clone()),&before).is_err(),"{change}");
    }
    let epoch=epoch_directory(root,&after.owner.generation);std::fs::create_dir_all(&epoch).unwrap();
    save_atomic(&epoch.join("ready.json"),&after.owner).unwrap();history.complete(&id,result).unwrap();
    assert_eq!(epochs(root).unwrap().len(),2);
}

#[test]
fn exhausted_restart_budget_preserves_original_worker_result_without_new_epoch() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    let temp=tempfile::tempdir().unwrap();let root=temp.path();
    let budget=WriteBudget::create(&root.join("budget"),LaunchId::new("g-restart").unwrap(),
        Limits {ordinary:65536,retirement:4096}).unwrap();
    let (mut history,ready,app)=setup_with_budget(root,Some(&budget));
    let path=root.join("restarts/history.json");
    let initial=std::fs::metadata(&path).unwrap().len();
    assert_eq!(budget.charged().unwrap().ordinary,initial);
    let id=RequestId::new("restart").unwrap();
    history.dispatch(id.clone(),ready.owner.generation.clone(),3,None).unwrap();
    assert_eq!(budget.charged().unwrap().ordinary,initial+std::fs::metadata(&path).unwrap().len());
    let (next,current)=acquisition(root,&"b".repeat(48),102);
    let result=Restarted {previous:Acquired::Ios(app),current:Acquired::Ios(current),ready:next,entry:None};
    worker_result(root,3,&result);
    budget.charge(Class::Ordinary,65536-budget.charged().unwrap().ordinary).unwrap();
    let before=std::fs::read(&path).unwrap();let charged=budget.charged().unwrap();
    assert!(history.complete(&id,result.clone()).is_err());
    assert!(history.ensure_resolved().is_err());
    assert!(history.dispatch(RequestId::new("repeat").unwrap(),ready.owner.generation.clone(),4,None).is_err());
    assert_eq!(read_completion(root,&id,&ready.owner.generation,None).unwrap().unwrap().owner(),&result.ready.owner);
    assert_eq!(epochs(root).unwrap().len(),1);
    assert_eq!(std::fs::read(&path).unwrap(),before);
    assert_eq!(budget.charged().unwrap(),charged);assert_eq!(charged.retirement,0);
}

#[test]
fn restart_and_cold_entry_refusal_create_no_dispatch_authority() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    for cold in [false,true] {
        let temp=tempfile::tempdir().unwrap();let root=temp.path();
        let budget=WriteBudget::create(&root.join("budget"),LaunchId::new("g-admission").unwrap(),
            Limits {ordinary:65536,retirement:4096}).unwrap();
        let (mut history,ready,_)=setup_with_system(root,Some(&budget),cold);
        // Exercise the same valid request with an independent allowance before testing refusal.
        let check=tempfile::tempdir().unwrap();let (mut control,control_ready,_)=setup_with_system(check.path(),None,cold);
        let id=RequestId::new("entry").unwrap();
        let dispatch=|h:&mut History,generation:String| {
            if cold {h.dispatch_entry(id.clone(),generation,"client-test://item/1".into(),3,"a".repeat(64))}
            else {h.dispatch(id.clone(),generation,3,None)}
        };
        dispatch(&mut control,control_ready.owner.generation).unwrap();
        budget.charge(Class::Ordinary,65536-budget.charged().unwrap().ordinary).unwrap();
        let path=root.join("restarts/history.json");let before=std::fs::read(&path).unwrap();
        assert!(dispatch(&mut history,ready.owner.generation).is_err());
        assert!(!history.contains(&id));history.ensure_resolved().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(),before);
        assert_eq!(budget.charged().unwrap().retirement,0);
    }
}
