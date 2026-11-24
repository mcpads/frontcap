use super::*;
use frontcap_core::native::{android_lifecycle::{Installation, Process}, android_webview::AppProcess};

fn id(value: &str) -> RequestId { RequestId::new(value).unwrap() }
fn save<T: Serialize>(path: &Path, value: &T) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap(); save_atomic(path,value).unwrap();
}
fn request(root: &Path, name: &str, operation: host::Operation, binding: Option<&Binding>) {
    let directory = root.join("requests").join(name);
    save(&directory.join("operation.json"),&Operation::Host { request:operation });
    if let Some(binding) = binding { save(&directory.join("host-binding.json"),binding); }
}
fn result(root: &Path, name: &str, completion: host::Completion) {
    save(&root.join("requests").join(name).join("result.json"),
        &Reply::Completed { completion:Box::new(Completion::Host { completion }) });
}
fn restart(root: &Path, name: &str, epoch: Option<RequestId>, before: &host::Ready, number: u32) -> host::Ready {
    let process = |owner: &AppProcess| Process { pid:owner.pid,start_ticks:owner.process_start_ticks,
        command:owner.package.clone(),boot_id:"abcd0000-0000-0000-0000-000000000001".into() };
    let mut after = before.clone(); after.owner.pid += 1; after.owner.process_start_ticks += 1;
    let installation = Installation {package:before.owner.package.clone(),apk:"/data/app/owned/base.apk".into(),
        apk_sha256:before.owner.apk_sha256.clone(),data:format!("/data/user/0/{}",before.owner.package).into(),data_device:1,data_inode:7};
    let url = (number == 2).then_some("example://item/2");
    let entry = url.map(|url| host::UrlAcceptance {owner:before.owner.clone(),url:url.into(),
        confirmation:frontcap_core::native::external_entry::Observation::Unobserved,
        destination:frontcap_core::native::external_entry::Observation::Unobserved});
    let replacement = host::Replacement {previous:before.clone(),ready:after.clone(),previous_process:process(&before.owner),
        process:process(&after.owner),installation:installation.clone(),entry};
    request(root,name,host::Operation::Restart {previous:before.owner.clone().into(), url:url.map(str::to_owned)},Some(&Binding {owner:before.owner.clone(),epoch}));
    let directory = root.join("target/host-restarts").join(name);
    save(&directory.join("intent.json"),&serde_json::json!({"previous":before,"process":replacement.previous_process,"installation":installation,"url":url}));
    save(&directory.join("terminated.json"),&replacement.previous_process);
    save(&directory.join("app-process.json"),&after.owner);
    let command = format!("command-{number}/command.json"); let path=root.join("target").join(&command);
    save(&path,&["fixture process replacement"]);
    let digest=frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(path).unwrap());
    save(&directory.join("receipt.json"),&serde_json::json!({"replacement":replacement,"commands":{command:digest}}));
    after
}
fn observe(root: &Path, name: &str, owner: &AppProcess, epoch: Option<RequestId>, binding: bool) {
    request(root,name,host::Operation::Observe {input_basis:false},binding.then_some(&Binding {owner:owner.clone(),epoch}));
    result(root,name,host::Completion::Observed {owner:Box::new(owner.clone()),
        screen:root.join("requests").join(name).join("capture/screen.png"),bytes:42,sha256:"c".repeat(64),basis:None});
}
fn lookup(root: &Path, name: &str, operation: host::Operation) -> io::Result<Option<Reply>> {
    super::super::super::read_result(root,&id(name),&Operation::Host {request:operation})
}

#[test]
fn original_host_results_follow_two_replacements_without_publication_replay_or_owner_adoption() {
    let mut fixture=crate::device::tests::Fixture::new();
    fixture.config.host_app=Some(crate::android_webview::Profile {package:"org.example.app".into(),
        activity:"org.example.app.ClientActivity".into(),local_service:None});
    let config=Config {device:fixture.config.clone(),entry_url:String::new(),lifetime_ms:30_000,input:None};
    let root=fixture.root().join("session");std::fs::create_dir(&root).unwrap();
    let owner=AppProcess {emulator:ProcessIdentity::of(std::process::id()).unwrap().unwrap(),
        package:"org.example.app".into(),activity:"org.example.app.ClientActivity".into(),serial:format!("emulator-{}",config.device.port),
        pid:123,process_start_ticks:42,apk_sha256:config.device.apk_sha256.clone(),source_sha256:config.device.source_sha256.clone()};
    let initial=host::Ready {owner:owner.clone(),contacts:None};
    save(&root.join("host-ready.json"),&initial);save(&root.join("launch.json"),&Launch {config:config.clone(),deadline_ns:1});
    observe(&root,"before",&owner,None,false); // Original single-process records stay readable.
    let before=serde_json::to_value(lookup(&root,"before",host::Operation::Observe {input_basis:false}).unwrap()).unwrap();
    let first=restart(&root,"first",None,&initial,1);
    let second=restart(&root,"second",Some(id("first")),&first,2);
    observe(&root,"after",&second.owner,Some(id("second")),true);
    // Recovery cannot silently reinterpret a cold URL replacement as a normal launch.
    let original_request=root.join("requests/second/operation.json");let original_bytes=std::fs::read(&original_request).unwrap();
    for url in [None,Some("example://different".into())] {
        save(&original_request,&Operation::Host {request:host::Operation::Restart {previous:first.owner.clone().into(),url}});
        assert!(lookup(&root,"after",host::Operation::Observe {input_basis:false}).is_err());
    }
    std::fs::write(&original_request,&original_bytes).unwrap();
    for (name,old) in [("first",&initial),("second",&first)] {
        let operation=host::Operation::Restart {previous:old.owner.clone().into(), url:(name=="second").then(||"example://item/2".into())};
        let completion=lookup(&root,name,operation.clone()).unwrap().unwrap();
        assert!(matches!(&completion,Reply::Completed {completion} if matches!(completion.as_ref(),Completion::Host {completion:host::Completion::Restarted {replacement}} if &replacement.previous==old)));
        assert_eq!(serde_json::to_value(lookup(&root,name,operation).unwrap().unwrap()).unwrap(),serde_json::to_value(completion).unwrap());
        assert!(!root.join("requests").join(name).join("result.json").exists(),"lookup must not publish a missing result");
    }
    assert_eq!(serde_json::to_value(lookup(&root,"before",host::Operation::Observe {input_basis:false}).unwrap()).unwrap(),before);
    let original_binding=root.join("requests/after/host-binding.json");let original=std::fs::read(&original_binding).unwrap();
    std::fs::remove_file(&original_binding).unwrap();
    assert!(lookup(&root,"after",host::Operation::Observe {input_basis:false}).is_err(),"missing binding cannot adopt replacement");
    std::fs::write(&original_binding,&original).unwrap();
    assert!(lookup(&root,"after",host::Operation::Observe {input_basis:false}).unwrap().is_some());
    let restart_binding=root.join("requests/second/host-binding.json");
    save(&restart_binding,&Binding {owner:first.owner.clone(),epoch:Some(id("second"))});
    assert!(lookup(&root,"after",host::Operation::Observe {input_basis:false}).is_err());
    save(&restart_binding,&Binding {owner:first.owner.clone(),epoch:Some(id("first"))});
    let receipt=root.join("target/host-restarts/second/receipt.json");let proof=std::fs::read(&receipt).unwrap();
    std::fs::remove_file(&receipt).unwrap();
    assert!(lookup(&root,"second",host::Operation::Restart {previous:first.owner.clone().into(), url:Some("example://item/2".into())}).unwrap().is_none());
    assert!(lookup(&root,"after",host::Operation::Observe {input_basis:false}).unwrap().is_none());
    let external=fixture.root().join("receipt.json");std::fs::write(&external,&proof).unwrap();
    std::os::unix::fs::symlink(&external,&receipt).unwrap();
    assert!(lookup(&root,"after",host::Operation::Observe {input_basis:false}).is_err());
    std::fs::remove_file(&receipt).unwrap();std::fs::write(&receipt,&proof).unwrap();
    // Recycled page IDs cannot turn an old discovery into new-process attachment authority.
    request(&root,"discovery",host::Operation::DiscoverPages,None);
    let page=host::Page {id:"recycled".into(),url:"http://127.0.0.1/game".into(),title:"Game".into()};
    result(&root,"discovery",host::Completion::Pages {owner:Box::new(owner.clone()),pages:vec![page.clone()]});
    let attachment=host::Attachment {id:id("attach"),owner:second.owner.clone(),page:page.clone(),
        prepared:frontcap_core::native::android_webview::Prepared {random_seed:None,script_identifier:"1".into(),url:page.url.clone(),attachment_sha256:"d".repeat(64)},
        document:"abcd0000-0000-0000-0000-000000000001".into()};
    save(&root.join("pages/attach/attachment.json"),&attachment);
    assert!(super::super::pages::valid_result(&root,&id("attach"),&host::Operation::AttachPage {random_seed:None,discovery:id("discovery"),page:page.id,url:page.url},
        &host::Completion::Attached {attachment:Box::new(attachment)},&second.owner).is_err());
    request(&root,"old-input",host::Operation::Contacts {observation:id("old-basis"),input:frontcap_core::native::contacts::Operation::Cancel {expected_revision:0}},None);
    assert!(super::super::contact_request(&root,&id("old-input"),&initial,&Binding {owner:second.owner,epoch:Some(id("second"))}).is_err());
    std::fs::write(root.join("target/command-1/command.json"),b"changed").unwrap();
    assert!(lookup(&root,"after",host::Operation::Observe {input_basis:false}).is_err());
    assert_eq!(serde_json::to_value(lookup(&root,"before",host::Operation::Observe {input_basis:false}).unwrap()).unwrap(),before);
}
