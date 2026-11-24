use frontcap_core::state::save_atomic;
use super::*;
use frontcap_core::{canonical::sha256_bytes_hex,native::ios_host::EntryAcceptance};
#[test]
fn pending_entry_reads_original_wait_without_promoting_later_readiness_or_repairing_files() {
    let (f,app,_)=crate::simulator_app::prepared::tests::fixture();
    let products=f.root.path().join("system-products");let names=crate::products::ProductNames {app:"SystemUI".into(),tests:"SystemUITests".into()};
    crate::products::tests::named_fixture(&products,&names);
    let manifest=crate::products::ProductsManifest::capture_role(&products,"Consumer.xctestrun",names,crate::products::ProductRole::ExternalRunner).unwrap();
    let config=Config {lifetime_ms: 900_000, simulator:f.config.clone(),app,contacts:true,system_ui:Some(crate::ios_host::system::Config {result_bundle_volume:None,runner:crate::ios_system_runner::Launch {xcodebuild:"/missing/xcodebuild".into(),products,manifest},python:"/missing/python".into(),script:"/missing/script".into()})};
    let root=f.root.path().join("session");let begin=RequestId::new("begin").unwrap();let poll=RequestId::new("poll").unwrap();
    let owner=Owner {simulator:"ABCD0000-0000-0000-0000-000000000001".into(),process:frontcap_core::process::ProcessIdentity {pid:42,started:"original".into()},attachment_sha256:config.app.attachment_sha256()};
    let binding=Binding {owner:owner.clone(),operation:Operation::BeginEntry {previous:owner.clone(),url:"consumer://history".into()},epoch:None};
    let epoch=root.join("host-epochs/begin");let app=epoch.join("app");std::fs::create_dir_all(&app).unwrap();
    std::fs::create_dir_all(root.join("host-requests/begin")).unwrap();save_atomic(&root.join("host-requests/begin/request.json"),&binding).unwrap();
    let accepted=EntryAcceptance {previous:owner.clone(),url:"consumer://history".into(),confirmation:frontcap_core::native::external_entry::Observation::Unobserved,destination:frontcap_core::native::external_entry::Observation::Unobserved};
    save_atomic(&app.join("entry-intent.json"),&accepted).unwrap();save_atomic(&app.join("terminated.json"),&owner.process).unwrap();
    std::fs::write(app.join("entry-admission.json"),b"admission").unwrap();
    let command=root.join("target/simulator/command-1");std::fs::create_dir_all(&command).unwrap();std::fs::write(command.join("command.json"),b"original URL").unwrap();
    save_atomic(&app.join("entry-accepted.json"),&serde_json::json!({"begun":accepted,"admission_sha256":sha256_bytes_hex(b"admission"),"commands":{"command-1/command.json":sha256_bytes_hex(b"original URL")}})).unwrap();
    let before=std::fs::read(app.join("entry-accepted.json")).unwrap();
    let recovered=begun(&root,&config,&begin,&binding).unwrap().unwrap();assert_eq!(recovered.acceptance,accepted);
    assert!(!epoch.join("entry.json").exists());
    let poll_binding=Binding {owner,operation:Operation::PollEntry {begin:begin.clone()},epoch:None};
    // Acquired replacement process/producer files alone cannot publish a ready poll. The
    // original begin remains recoverable and the unpublished poll remains uncertain.
    save_atomic(&app.join("process.json"),&frontcap_core::process::ProcessIdentity {pid:44,started:"replacement".into()}).unwrap();
    std::fs::create_dir(epoch.join("host")).unwrap();
    save_atomic(&epoch.join("host/ready.json"),&poll_binding.owner).unwrap();
    assert!(polled(&root,&config,&poll,&poll_binding,&begin).unwrap().is_none());
    assert_eq!(begun(&root,&config,&begin,&binding).unwrap().unwrap(),recovered);
    assert!(!epoch.join("entry-published-by.json").exists());
    assert!(!epoch.join("replacement-ready.json").exists());
    let waiting=Completion::EntryPolled {begin:begin.clone(),ready:None};
    std::fs::create_dir(epoch.join("polls")).unwrap();save_atomic(&epoch.join("polls/poll.json"),&waiting).unwrap();
    for published in [None,Some(RequestId::new("later").unwrap())] {
        if let Some(id)=published {save_atomic(&epoch.join("entry-published-by.json"),&id).unwrap();}
        assert_eq!(polled(&root,&config,&poll,&poll_binding,&begin).unwrap(),Some(waiting.clone()));
    }
    assert_eq!(std::fs::read(app.join("entry-accepted.json")).unwrap(),before);
    assert!(!root.join("host-requests/poll/result.json").exists());
    save_atomic(&epoch.join("entry-published-by.json"),&poll).unwrap();assert!(polled(&root,&config,&poll,&poll_binding,&begin).is_err());
    std::fs::write(command.join("command.json"),b"changed URL").unwrap();assert!(begun(&root,&config,&begin,&binding).is_err());
}
