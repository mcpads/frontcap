use super::*;
#[test]
fn registered_terminal_result_refuses_replacement_links_and_oversize() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join("session"); fs::create_dir(&session).unwrap();
    assert_eq!(result_path(&session).unwrap(),session.join("worker/result.json"));
    let control = create_in(&session,root.path()).unwrap();
    assert!(create_in(&session,root.path()).is_err());
    let result=control.join("result.json");fs::write(&result,b"{}").unwrap();
    assert_eq!(result_path(&session).unwrap(),result);
    fs::write(&result,vec![b'x';RESULT_LIMIT as usize+1]).unwrap();
    assert!(result_path(&session).is_err());fs::remove_file(&result).unwrap();
    let foreign=root.path().join("foreign");fs::write(&foreign,b"original").unwrap();
    std::os::unix::fs::symlink(&foreign,&result).unwrap();assert!(result_path(&session).is_err());
    fs::rename(&control,root.path().join("original-control")).unwrap();fs::create_dir(&control).unwrap();
    fs::write(control.join("result.json"),b"{}").unwrap();assert!(result_path(&session).is_err());
    assert_eq!(fs::read(foreign).unwrap(),b"original");
}

#[test]
fn flutter_inspection_reads_only_registered_control_completion() {
    let root=tempfile::tempdir().unwrap();let session=root.path().join("session");
    fs::create_dir_all(session.join("worker")).unwrap();
    fs::write(session.join("worker/session.lock"),b"").unwrap();
    let control=create_in(&session,root.path()).unwrap();
    fs::write(session.join("worker/result.json"),br#"{"state":"stopped","retirement":{"passed":true,"errors":[],"signal_error":null},"error":null}"#).unwrap();
    assert!(matches!(crate::flutter_session::inspect(&session).unwrap(),crate::flutter_session::Recovery::Unresolved));
    fs::write(control.join("result.json"),br#"{"state":"failed","retirement":{"passed":false,"errors":["original storage error"],"signal_error":null},"error":"original"}"#).unwrap();
    let crate::flutter_session::Recovery::Finished(result)=crate::flutter_session::inspect(&session).unwrap() else {panic!("missing terminal record")};
    assert!(!result.retirement.passed);assert_eq!(result.error.as_deref(),Some("original"));
}

#[test]
fn diagnostic_inspection_survives_target_loss_without_claiming_worker_exit() {
    let root=tempfile::tempdir().unwrap();let session=root.path().join("target-session");
    fs::create_dir(&session).unwrap();let control=create_in(&session,root.path()).unwrap();
    assert!(inspect_root(root.path()).unwrap().unwrap().result.is_none());
    fs::write(control.join("result.json"),br#"{"state":"failed","retirement":{"passed":false,"errors":["status storage unavailable"],"signal_error":null},"error":null}"#).unwrap();
    fs::remove_dir_all(&session).unwrap();
    let before=fs::read(control.join("result.json")).unwrap();
    let inspection=inspect_root(root.path()).unwrap().unwrap();
    assert!(inspection.evidence_only);assert!(!inspection.worker_lifetime_verified);
    assert!(!inspection.result.unwrap().retirement.passed);
    assert_eq!(fs::read(control.join("result.json")).unwrap(),before);
    let long = serde_json::json!({"state":"failed","retirement":{"passed":false,
        "errors":vec!["한".repeat(1000);6],"signal_error":null},"error":null});
    fs::write(control.join("result.json"),serde_json::to_vec(&long).unwrap()).unwrap();
    let full=fs::read(control.join("result.json")).unwrap();
    let preview=inspect_root(root.path()).unwrap().unwrap();
    assert_eq!(preview.error_count,6);assert!(preview.details_truncated);
    let errors=preview.result.unwrap().retirement.errors;
    assert_eq!(errors.len(),4);assert_eq!(errors[0].chars().count(),512);
    assert_eq!(fs::read(control.join("result.json")).unwrap(),full);
    fs::rename(&control,root.path().join("original")).unwrap();fs::create_dir(&control).unwrap();
    fs::write(control.join("result.json"),before).unwrap();
    assert!(inspect_root(root.path()).is_err());
}

#[test]
fn control_reads_reject_linked_oversized_and_partial_records() {
    let root=tempfile::tempdir().unwrap();let path=root.path().join("record.json");
    assert!(read_bounded::<serde_json::Value>(&path,32).unwrap().is_none());
    fs::write(&path,br#"{"original":true}"#).unwrap();
    assert_eq!(read_bounded::<serde_json::Value>(&path,32).unwrap().unwrap()["original"],true);
    let link=root.path().join("linked.json");std::os::unix::fs::symlink(&path,&link).unwrap();
    assert!(read_bounded::<serde_json::Value>(&link,32).is_err());
    fs::write(&path,vec![b' ';33]).unwrap();assert!(read_bounded::<serde_json::Value>(&path,32).is_err());
    fs::write(&path,br#"{"partial":"#).unwrap();assert!(read_bounded::<serde_json::Value>(&path,32).is_err());
    assert_eq!(fs::read(&path).unwrap(),br#"{"partial":"#);
    assert!(read_bounded::<serde_json::Value>(root.path(),32).is_err());
}
