use super::*;
use frontcap_core::{canonical::sha256_bytes_hex, native::{ios_host::{Ready, Completion}, ios_ui::{UiOutcome, UiRequest}, observation::Artifact}};

#[test]
fn native_system_recovers_missing_worker_publication_without_live_runner_or_store_writes() {
    let (fixture, mut app) = crate::simulator_app::tests::setup("normal");
    app.script=None;
    let root=fixture.root.path().canonicalize().unwrap();
    let products=root.join("products");
    let names=crate::products::ProductNames {app:"SystemUI".into(),tests:"SystemUITests".into()};
    crate::products::tests::named_fixture(&products,&names);
    let manifest=crate::products::ProductsManifest::capture_role(&products,"Consumer.xctestrun",names,crate::products::ProductRole::ExternalRunner).unwrap();
    let config=Config {result_bundle_volume:None,runner:crate::ios_system_runner::Launch {xcodebuild:"/missing/xcodebuild".into(),products,manifest},python:"/missing/python".into(),script:"/missing/bridge.py".into()};
    let owner=Owner {simulator:"ABCD0000-0000-0000-0000-000000000001".into(),process:frontcap_core::process::ProcessIdentity {pid:43,started:"original app".into()},attachment_sha256:app.attachment_sha256()};
    let runner=UiIdentity {simulator:owner.simulator.clone(),generation:"abcd0000-0000-0000-0000-000000000002".into(),pid:42};
    let ready=UiReady {app_restart:false,app_entry:false,app_lifecycle:false,system_alerts:true,system_only:true,host_app_state:true,port:1234,
        identity:runner.clone(),descriptor:"/missing/retired/ui-control.json".into(),process:frontcap_core::process::ProcessIdentity {pid:42,started:"original runner".into()}};
    let host=root.join("target/host");std::fs::create_dir_all(&host).unwrap();
    let system=host.join("system-ui");let mut store=Store::create(&system,owner.clone(),runner.clone()).unwrap();
    save_atomic(&system.join("config.json"),&config).unwrap();save_atomic(&system.join("ready.json"),&ready).unwrap();
    save_atomic(&system.join("initialized.json"),&UiReply::Ready {identity:runner.clone(),source_sha256:fixture.config.source_sha256.clone()}).unwrap();
    let id=RequestId::new("alerts").unwrap();let request=store.dispatch_ui(id.clone(),&UiOperation::ObserveSystemAlerts {}).unwrap();
    let catalog:frontcap_core::native::ios_system_ui::Catalog=serde_json::from_str(include_str!("../../core/tests/fixtures/ios-system-ui.json")).unwrap();
    let client=system.join("client");let mut artifacts=vec![];
    for name in ["screen.png","hierarchy.txt"] {
        let path=format!("observations/{}/{name}",catalog.observation);
        std::fs::create_dir_all(client.join(&path).parent().unwrap()).unwrap();std::fs::write(client.join(&path),b"fixture").unwrap();
        artifacts.push(Artifact {path,bytes:7,sha256:sha256_bytes_hex(b"fixture")});
    }
    let bytes=serde_json::to_vec(&serde_json::json!({"request":request,"generation":runner.generation,"pid":runner.pid,"port":1234,"last_id":1,"status":"ok","result":{"state":"system_observed","catalog":catalog,"artifacts":artifacts}})).unwrap();
    let digest=sha256_bytes_hex(&bytes);std::fs::create_dir(client.join("receipts")).unwrap();std::fs::write(client.join("receipts").join(format!("{digest}.json")),bytes).unwrap();
    let ui=UiCompletion {generation:runner.generation.clone(),pid:runner.pid,last_id:1,request:UiRequest {..request},receipt_sha256:digest,outcome:UiOutcome::SystemAlertsObserved {catalog,artifacts}};
    std::fs::create_dir(system.join("results")).unwrap();save_atomic(&system.join("results/1.json"),&UiReply::Completed {completion:Box::new(ui.clone())}).unwrap();
    let host_config=super::super::Config {lifetime_ms: 900_000, simulator:fixture.config.clone(),app:app.clone(),contacts:false,system_ui:Some(config.clone())};
    save_atomic(&root.join("launch.json"),&serde_json::json!({"host":true,"deadline_ns":1,"config":crate::webview_session::Config {lifetime_ms: 900_000, simulator:fixture.config,app,contacts:false,system_ui:Some(config),lifecycle:None,service_url_environment:None}})).unwrap();
    save_atomic(&root.join("host-ready.json"),&Ready {owner:owner.clone(),contacts:None,pages:false,app_lifecycle:true,system_ui:Some(runner)}).unwrap();
    std::fs::create_dir(root.join("target/simulator")).unwrap();save_atomic(&root.join("target/simulator/simulator.json"),&owner.simulator).unwrap();
    std::fs::create_dir(root.join("target/app")).unwrap();save_atomic(&root.join("target/app/process.json"),&owner.process).unwrap();
    let operation=Operation::ObserveSystemAlerts;std::fs::create_dir_all(root.join("host-requests/alerts")).unwrap();
    save_atomic(&root.join("host-requests/alerts/request.json"),&serde_json::json!({"owner":owner,"operation":operation})).unwrap();
    let before=std::fs::read(system.join("requests.json")).unwrap();
    let answer=crate::ios_host_session::read_result(&root,&host_config,&id,&operation).unwrap().unwrap();
    let crate::webview_session::Reply::Completed {completion} = answer else {panic!()};
    assert!(matches!(*completion,crate::webview_session::Completion::Host {completion:Completion::SystemUi {completion,directory}} if *completion==ui && directory==client));
    assert_eq!(std::fs::read(system.join("requests.json")).unwrap(),before);
    assert!(!root.join("host-requests/alerts/result.json").exists());assert!(store.ensure_resolved().is_err());
    let UiOutcome::SystemAlertsObserved {artifacts,..} = &ui.outcome else {panic!()};
    std::fs::write(client.join(&artifacts[0].path),b"changed").unwrap();
    assert!(crate::ios_host_session::read_result(&root,&host_config,&id,&operation).is_err());
    // The same independent runner supports host visibility. A completed native effect alone
    // cannot repair a missing post-effect process sample.
    std::fs::write(client.join(&artifacts[0].path),b"fixture").unwrap();
    store.complete(&id,ui.clone()).unwrap();
    let id=RequestId::new("home").unwrap();
    let state=frontcap_core::native::app_state::State::Background;
    let command=UiOperation::SetHostAppState {bundle:host_config.app.bundle.clone(),pid:owner.process.pid,state};
    let request=store.dispatch_ui(id.clone(),&command).unwrap();
    let change=frontcap_core::native::ios_ui::HostAppStateChange {bundle:host_config.app.bundle.clone(),pid:owner.process.pid,requested:state,before_state:4,app_state:3};
    let bytes=serde_json::to_vec(&serde_json::json!({"request":request,"generation":ready.identity.generation,"pid":ready.identity.pid,"port":1234,"last_id":2,"status":"ok","result":{"state":"host_app_state_changed","change":change}})).unwrap();
    let digest=sha256_bytes_hex(&bytes);std::fs::write(client.join("receipts").join(format!("{digest}.json")),bytes).unwrap();
    let ui=UiCompletion {generation:ready.identity.generation.clone(),pid:ready.identity.pid,last_id:2,request,receipt_sha256:digest,outcome:UiOutcome::HostAppStateChanged {change}};
    save_atomic(&system.join("results/2.json"),&UiReply::Completed {completion:Box::new(ui)}).unwrap();
    let operation=Operation::SetAppState {owner:Box::new(owner.clone()),state};
    let request_root=root.join("host-requests/home");std::fs::create_dir(&request_root).unwrap();
    save_atomic(&request_root.join("request.json"),&serde_json::json!({"owner":owner,"operation":operation})).unwrap();
    let evidence=host.join("app-state/home");std::fs::create_dir_all(&evidence).unwrap();
    save_atomic(&evidence.join("before.json"),&owner.process).unwrap();
    assert!(crate::ios_host_session::read_result(&root,&host_config,&id,&operation).unwrap().is_none());
    save_atomic(&evidence.join("after.json"),&owner.process).unwrap();
    let before=std::fs::read(system.join("requests.json")).unwrap();
    let answer=crate::ios_host_session::read_result(&root,&host_config,&id,&operation).unwrap().unwrap();
    let crate::webview_session::Reply::Completed {completion}=answer else {panic!()};
    assert!(matches!(*completion,crate::webview_session::Completion::Host {completion:Completion::StateChanged {transition}} if transition.state().unwrap()==state));
    assert_eq!(std::fs::read(system.join("requests.json")).unwrap(),before);
    assert!(!request_root.join("result.json").exists());assert!(!evidence.join("receipt.json").exists());assert!(store.ensure_resolved().is_err());
    let mut replacement=owner.process.clone();replacement.started="another process".into();
    save_atomic(&evidence.join("after.json"),&replacement).unwrap();
    assert!(crate::ios_host_session::read_result(&root,&host_config,&id,&operation).is_err());

}
