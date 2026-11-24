use super::*;
use frontcap_core::native::{android_lifecycle::{Installation as AndroidInstallation,Process}, external_entry::Target,flutter::Platform};

fn fixture(root:&Path)->(Ready,Acquired,History) {
    fixture_with_budget(root,None)
}
fn fixture_with_budget(root:&Path,budget:Option<&WriteBudget>)->(Ready,Acquired,History) {
    let ready=Ready {owner:Owner {platform:Platform::Android,device:"emulator-5580".into(),generation:"a".repeat(48),source_sha256:"f".repeat(64)},
        endpoint:crate::flutter_bridge::Endpoint {port:30000,token:"b".repeat(48)},system_ui:None,
        external_url_target:Some(Target::Android {serial:"emulator-5580".into(),package:"org.example.client".into()})};
    let app=Acquired::Android(crate::flutter_android_app::Acquired {serial:ready.owner.device.clone(),package:"org.example.client".into(),activity:"org.example.client.Main".into(),
        installation:AndroidInstallation {package:"org.example.client".into(),apk:"/data/app/package/base.apk".into(),apk_sha256:"c".repeat(64),
            data:"/data/user/0/org.example.client".into(),data_device:1,data_inode:42},
        process:Process {pid:100,boot_id:"abcd0000-0000-0000-0000-000000000001".into(),start_ticks:1000,command:"org.example.client".into()},
        endpoint:crate::flutter_ios_app::Endpoint {generation:ready.owner.generation.clone(),token:ready.endpoint.token.clone(),port:23456,scenario:"session".into()},
        forwarding:crate::flutter_android_app::Forwarding {serial:ready.owner.device.clone(),host_port:30000,device_port:23456}});
    std::fs::create_dir_all(root.join("session/worker")).unwrap();
    save_atomic(&root.join("ready.json"),&ready.owner).unwrap();save_atomic(&root.join("session/worker/ready.json"),&ready).unwrap();
    save_atomic(&root.join("session/worker/application.json"),&app).unwrap();
    drop(crate::flutter_restarts::History::create(root,ready.clone(),app.clone()).unwrap());
    let history=History::create_with_budget(root,ready.owner.clone(),budget).unwrap();(ready,app,history)
}
fn record(root:&Path,number:u64,ready:&Ready,app:&Acquired,state:State)->Changed {
    let (before,after)=if state==State::Background {("org.example.client/org.example.client.Main","dev.launcher/dev.launcher.Home")}
        else {("dev.launcher/dev.launcher.Home","org.example.client/org.example.client.Main")};
    let directory=root.join("session/worker/commands").join(number.to_string());let evidence=directory.join("app-state");
    std::fs::create_dir_all(&evidence).unwrap();
    save_atomic(&directory.join("request.json"),&serde_json::json!({"number":number,"timeout_ms":5000,"operation":"set_app_state","generation":ready.owner.generation,"state":state})).unwrap();
    let native=Native::Android {focused_before:before.into(),focused_after:after.into()};
    save_atomic(&evidence.join("before.json"),app).unwrap();save_atomic(&evidence.join("after.json"),app).unwrap();save_atomic(&evidence.join("native.json"),&native).unwrap();
    for (name,focus) in [("window-before.txt",before),("window-after.txt",after)] {std::fs::write(evidence.join(name),format!("mCurrentFocus=Window{{1 u0 {focus}}}")).unwrap();}
    Changed {before:app.clone(),after:app.clone(),native,ready:ready.clone(),state}
}
#[test]
fn flutter_host_original_result_is_read_only_and_missing_or_changed_proof_stays_unresolved(){
    let temp=tempfile::tempdir().unwrap();let root=temp.path();let (ready,app,mut history)=fixture(root);
    let id=RequestId::new("home").unwrap();history.dispatch(id.clone(),ready.clone(),app.clone(),State::Background,1,None).unwrap();
    assert!(read_completion(root,&ready.owner,&id,State::Background).unwrap().is_none());
    let result=record(root,1,&ready,&app,State::Background);
    let original=std::fs::read(root.join("app-state/history.json")).unwrap();
    read_completion(root,&ready.owner,&id,State::Background).unwrap().unwrap().validate().unwrap();
    assert_eq!(std::fs::read(root.join("app-state/history.json")).unwrap(),original);
    assert!(history.ensure_resolved().is_err());
    assert!(history.dispatch(RequestId::new("repeat").unwrap(),ready.clone(),app.clone(),State::Background,2,None).is_err());
    let Acquired::Android(mut replaced)=app.clone() else {unreachable!()};replaced.process.start_ticks+=1;
    let evidence=root.join("session/worker/commands/1/app-state");save_atomic(&evidence.join("after.json"),&Acquired::Android(replaced)).unwrap();
    assert!(read_completion(root,&ready.owner,&id,State::Background).is_err());
    save_atomic(&evidence.join("after.json"),&app).unwrap();std::fs::remove_file(evidence.join("after.json")).unwrap();
    assert!(read_completion(root,&ready.owner,&id,State::Background).unwrap().is_none());
    save_atomic(&evidence.join("after.json"),&app).unwrap();
    history.complete(&id,result).unwrap();assert!(history.ensure_foreground(&ready.owner).is_err());
    std::fs::write(evidence.join("window-after.txt"),"mCurrentFocus=Window{1 u0 dev.other/.Other}").unwrap();
    assert!(read_completion(root,&ready.owner,&id,State::Background).is_err());
}
#[test]
fn flutter_host_return_requires_opposite_state_and_preserves_the_generation(){
    let temp=tempfile::tempdir().unwrap();let root=temp.path();let (ready,app,mut history)=fixture(root);
    history.ensure_foreground(&ready.owner).unwrap();
    assert!(history.validate_request(&RequestId::new("return-too-early").unwrap(),&ready,State::Foreground).is_err());
    for (index,state) in [State::Background,State::Foreground].into_iter().enumerate(){
        let id=RequestId::new(format!("transition-{index}")).unwrap();let number=index as u64+1;
        history.dispatch(id.clone(),ready.clone(),app.clone(),state,number,None).unwrap();
        let result=record(root,number,&ready,&app,state);let done=history.complete(&id,result).unwrap();
        assert_eq!(done.owner(),&ready.owner);
    }
    history.ensure_foreground(&ready.owner).unwrap();
}

#[test]
fn visibility_budget_preserves_unpublished_results_and_cached_completions() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    for exhaustion in [0,1,2] {
        let temp=tempfile::tempdir().unwrap();let root=temp.path();
        let budget=WriteBudget::create(&root.join("budget"),LaunchId::new("g-visibility").unwrap(),
            Limits {ordinary:65536,retirement:4096}).unwrap();
        let (ready,app,mut history)=fixture_with_budget(root,Some(&budget));
        let path=root.join("app-state/history.json");let id=RequestId::new("home").unwrap();
        assert_eq!(budget.charged().unwrap().ordinary,std::fs::metadata(&path).unwrap().len());
        if exhaustion==0 {
            budget.charge(Class::Ordinary,65536-budget.charged().unwrap().ordinary).unwrap();
            let before=std::fs::read(&path).unwrap();
            assert!(history.dispatch(id.clone(),ready.clone(),app.clone(),State::Background,1,None).is_err());
            assert!(!history.contains(&id));history.ensure_foreground(&ready.owner).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(),before);
            continue;
        }
        history.dispatch(id.clone(),ready.clone(),app.clone(),State::Background,1,None).unwrap();
        let changed=record(root,1,&ready,&app,State::Background);
        if exhaustion==2 {history.complete(&id,changed.clone()).unwrap();}
        budget.charge(Class::Ordinary,65536-budget.charged().unwrap().ordinary).unwrap();
        let before=std::fs::read(&path).unwrap();let charged=budget.charged().unwrap();
        let result=history.complete(&id,changed);
        if exhaustion==1 {assert!(result.is_err());assert!(history.ensure_resolved().is_err());}
        else {result.unwrap();history.ensure_resolved().unwrap();}
        assert!(read_completion(root,&ready.owner,&id,State::Background).unwrap().is_some());
        assert_eq!(std::fs::read(&path).unwrap(),before);
        assert_eq!(budget.charged().unwrap(),charged);assert_eq!(charged.retirement,0);
    }
}
