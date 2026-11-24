use super::*;
#[test]
fn retained_page_authority_binds_owner_document_and_original_action() {
    use frontcap_core::native::{android_webview::{AppProcess, Prepared}, android_timers};
    let root = tempfile::tempdir().unwrap();
    let owner = AppProcess { emulator: ProcessIdentity::of(std::process::id()).unwrap().unwrap(),
        package:"dev.example.consumer".into(), activity:"dev.example.consumer.MainActivity".into(),
        serial:"emulator-5580".into(), pid:123, process_start_ticks:42,
        apk_sha256:"a".repeat(64),source_sha256:"b".repeat(64) };
    let id = RequestId::new("attachment").unwrap();
    let original = host::Attachment { id:id.clone(), owner:owner.clone(),
        page:host::Page { id:"page-1".into(), url:"http://127.0.0.1/game".into(), title:"Game".into() },
        prepared:Prepared { random_seed:None, script_identifier:"1".into(),url:"http://127.0.0.1/game".into(),attachment_sha256:"c".repeat(64) },
        document:"e67c9b05-5144-4cab-a13b-99a9b8e04aee".into() };
    let path=root.path().join("pages/attachment/attachment.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    save_atomic(&path,&original).unwrap();
    let before=std::fs::read(&path).unwrap();
    let debugger=frontcap_core::native::android_webview::Debugger {
        emulator:owner.emulator.clone(), package:owner.package.clone(), activity:owner.activity.clone(),
        serial:owner.serial.clone(), pid:owner.pid, process_start_ticks:owner.process_start_ticks,
        socket:"webview_devtools_remote_123".into(), apk_sha256:owner.apk_sha256.clone(),source_sha256:owner.source_sha256.clone(),
    };
    let ledger=crate::android_frames::Ledger::bind(&crate::android_frames::Receipt {
        document:original.document.clone(),legacy_used:false,last:None,
    }).unwrap();
    let timers=android_timers::State::Paused { completion:android_timers::Completion {
        document:original.document.clone(),action:android_timers::Action::Pause,ticks_base:1.0,budget_expired:false,
    } };
    let progress=frontcap_core::native::android_progress::Progress {
        owner:debugger,page_id:original.page.id.clone(),
        basis:frontcap_core::native::webview_progress::Basis::RetainedReceipts,
        connection_closed:false,frames_need_reconciliation:false,timers_need_pause:false,
        connection_navigation:None,frames:Some(ledger.inspect()),timers:Some(timers.clone()),contacts:None,lifecycle:None,
        service:frontcap_core::native::webview_progress::Service::NotInspected,limitations:vec![],
    };
    validate_release_progress(&original,&progress).unwrap();
    for mode in 0..6 {
        let mut uncertain=progress.clone();
        match mode {
            0 => uncertain.connection_closed=true,
            1 => uncertain.frames_need_reconciliation=true,
            2 => uncertain.timers_need_pause=true,
            3 => uncertain.frames.as_mut().unwrap().pending=Some(crate::android_frames::Request {document:original.document.clone(),id:1,timestamp:10.0}),
            4 => uncertain.timers=Some(android_timers::State::Pending {document:original.document.clone(),request:RequestId::new("advance").unwrap(),action:android_timers::Action::Advance {milliseconds:100}}),
            _ => uncertain.frames.as_mut().unwrap().document="abcd0000-0000-0000-0000-000000000001".into(),
        }
        assert!(validate_release_progress(&original,&uncertain).is_err());
    }
    let page=path.parent().unwrap().join("page");std::fs::create_dir(&page).unwrap();
    save_atomic(&page.join("frame-producer.json"),&ledger).unwrap();
    save_atomic(&page.join("timer-state.json"),&timers).unwrap();
    let release_id=RequestId::new("release").unwrap();
    let release_op=host::Operation::ReleasePage {attachment:id.clone()};
    assert!(recover(root.path(),&release_id,&release_op).unwrap().is_none());
    let proof_path=root.path().join("requests/release/page-release.json");
    std::fs::create_dir_all(proof_path.parent().unwrap()).unwrap();
    save_atomic(&proof_path,&Release {attachment:original.clone(),
        frames_sha256:producer_digest(&page,"frame-producer.json").unwrap(),
        timers_sha256:producer_digest(&page,"timer-state.json").unwrap(),
    }).unwrap();
    assert!(released_attachment(root.path(),&release_id,&owner).is_err());
    save_atomic(&proof_path.parent().unwrap().join("operation.json"),
        &Operation::Host {request:release_op.clone()}).unwrap();
    assert_eq!(released_attachment(root.path(),&release_id,&owner).unwrap(),original);
    let mut foreign_owner=owner.clone();foreign_owner.process_start_ticks+=1;
    assert!(released_attachment(root.path(),&release_id,&foreign_owner).is_err());
    let foreign=Completion::Frames {receipt:Some(crate::android_frames::Receipt {
        document:"abcd0000-0000-0000-0000-000000000001".into(),legacy_used:false,last:None })};
    assert!(matches_page_result(&original,&host::PageOperation::InspectDocument,&foreign).unwrap());
    assert!(!matches_page_result(&original,&host::PageOperation::InspectFrames,&foreign).unwrap());
    let released=host::Completion::Released {attachment:Box::new(original.clone())};
    assert_eq!(recover(root.path(),&release_id,&release_op).unwrap(),Some(released.clone()));
    assert!(valid_result(root.path(),&release_id,&release_op,&released,&owner).unwrap());
    assert!(!root.path().join("requests/release/page-retirement.json").exists());
    assert!(!valid_transport_handoff(path.parent().unwrap(), &id).unwrap());
    let handoff=path.parent().unwrap().join("previous-transport-reused.json");
    save_atomic(&handoff,&id).unwrap();
    assert!(valid_transport_handoff(path.parent().unwrap(), &id).unwrap());
    assert!(!valid_transport_handoff(path.parent().unwrap(), &release_id).unwrap());
    let historical=path.parent().unwrap().join("previous-transport-retired.json");
    save_atomic(&historical,&id).unwrap();
    assert!(!valid_transport_handoff(path.parent().unwrap(), &id).unwrap());
    std::fs::remove_file(handoff).unwrap();
    assert!(valid_transport_handoff(path.parent().unwrap(), &id).unwrap());
    let original_timers=std::fs::read(page.join("timer-state.json")).unwrap();
    std::fs::write(page.join("timer-state.json"),b"changed").unwrap();
    assert!(recover(root.path(),&release_id,&release_op).is_err());
    std::fs::write(page.join("timer-state.json"),original_timers).unwrap();
    assert!(valid_result(root.path(),&release_id,&release_op,&released,&owner).unwrap());
    let proof=std::fs::read(&proof_path).unwrap();std::fs::remove_file(&proof_path).unwrap();
    assert!(!valid_result(root.path(),&release_id,&release_op,&released,&owner).unwrap());
    std::fs::write(proof_path,proof).unwrap();

    let retire=host::Operation::RetirePage { attachment:id.clone() };
    let retired=host::Completion::Retired { attachment:Box::new(original.clone()) };
    let request=RequestId::new("retire").unwrap();
    assert!(valid_result(root.path(),&request,&retire,&retired,&owner).unwrap());
    // Retirement does not erase original receipts or grant authority in a replacement owner.
    let mut replacement=owner.clone();replacement.process_start_ticks+=1;
    assert!(valid_result(root.path(),&request,&retire,&retired,&replacement).is_err());
    let command=host::PageOperation::Timers { action:android_timers::Action::Advance { milliseconds:100 } };
    let mut completion=Completion::Timers { completion:android_timers::Completion {
        document:original.document.clone(),action:android_timers::Action::Advance { milliseconds:100 },ticks_base:1.0,budget_expired:true } };
    assert!(matches_page_result(&original,&command,&completion).unwrap());
    if let Completion::Timers { completion }=&mut completion { completion.document="5597db58-7092-4749-b5bc-634c543d2647".into(); }
    assert!(!matches_page_result(&original,&command,&completion).unwrap());
    if let Completion::Timers { completion }=&mut completion { completion.document=original.document.clone();completion.action=android_timers::Action::Pause; }
    assert!(!matches_page_result(&original,&command,&completion).unwrap());
    assert_eq!(std::fs::read(path).unwrap(),before);
    assert!(recover(root.path(),&RequestId::new("missing").unwrap(),&retire).unwrap().is_none());
}
