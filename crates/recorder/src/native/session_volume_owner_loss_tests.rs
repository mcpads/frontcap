use super::*;
#[test]
fn preparation_reads_original_acquisition_cleanup_without_a_live_mount_or_synthetic_stop() {
    use crate::{native::preparation::{self, TargetRetirement}, runtime::{self, RuntimeRecord, RuntimeState}};
    use frontcap_core::{ids::{RequestId, RunId}, state::save_atomic};
    let temp=tempfile::tempdir().unwrap(); let root=temp.path().canonicalize().unwrap();
    let worker=root.join("native-session-volume/worker"); std::fs::create_dir_all(&worker).unwrap();
    let mut target: frontcap_native::ios_target::IosLaunch=serde_json::from_str(
        include_str!("../../tests/fixtures/native/ios-launch.json")).unwrap();
    target.resource_lifetime_ms=Some(30_000);
    let config=Config { python:"/python".into(), worker:"/volume-worker".into(), size_mib:256, reserve_mib:8 };
    let spec=LaunchSpec { request_id:RequestId::generate(),launch_id:LaunchId::generate(),run_id:RunId::new("acquisition-readback").unwrap(),
        target:target::Launch::Ios(target),session_executable:"/original/reader".into(),acquisition_budget_ms:30_000,
        min_free_bytes:None,write_budget:None,session_root:None,session_volume:Some(config.clone()),service:None };
    save_atomic(&crate::native::spec_path(&root),&spec).unwrap();
    save_atomic(&root.join("native-session-volume-plan.json"),&config).unwrap();
    let session=worker.join("volume/mount").join(spec.launch_id.as_str());
    let binding=AcquisitionBinding {
        registration:AcquisitionRegistration { operation:"watch_uikit_acquisition".into(),session:session.clone(),
            worker:ProcessIdentity {pid:11,started:"session".into()},retirement_reader:Some("/original/reader".into()) },
        diagnostic:session.join("worker.log.diagnostics/original"),diagnostic_process:ProcessIdentity {pid:12,started:"diagnostic".into()},
        diagnostic_config_sha256:"a".repeat(64),retirement_reader_sha256:Some("b".repeat(64)),launch_sha256:Some("c".repeat(64)) };
    save_atomic(&worker.join("watch-acquisition.json"),&binding).unwrap();
    let mut bytes=serde_json::to_vec(&binding.registration).unwrap();bytes.push(b'\n');
    save_atomic(&worker.join("watch-acquisition-ready.json"),&serde_json::json!({
        "request_sha256":frontcap_core::canonical::sha256_bytes_hex(&bytes)})).unwrap();
    let proof=serde_json::json!({"binding":binding,"pids_absent":[11,12],"session_result_sha256":null,"result":null,
        "diagnostic_result_sha256":"d".repeat(64),"independent_retirement_sha256":"e".repeat(64),
        "retirement":{"passed":true,"errors":[],"signal_error":null}});
    let path=worker.join("owner-loss-acquisition-retired.json");save_atomic(&path,&proof).unwrap();
    let mut record=RuntimeRecord {launch_id:spec.launch_id.clone(),state:RuntimeState::Starting,recorder:ProcessIdentity::current(),
        browser:None,endpoint:None,socket:root.join("recorder.sock")};
    runtime::save_runtime(&root,&record).unwrap();
    assert!(matches!(preparation::inspect(&root,&spec.launch_id).unwrap().target_retirement,TargetRetirement::Pending));
    let mut child=std::process::Command::new("/bin/sleep").arg("30").spawn().unwrap();
    record.recorder=ProcessIdentity::of(child.id()).unwrap().unwrap();child.kill().unwrap();child.wait().unwrap();
    runtime::save_runtime(&root,&record).unwrap();let original=std::fs::read(root.join("runtime.json")).unwrap();
    assert!(matches!(preparation::inspect(&root,&spec.launch_id).unwrap().target_retirement,
        TargetRetirement::Recorded { retirement } if retirement.passed));
    assert!(!session.exists());assert!(!root.join("native-session-volume-retirement.json").exists());
    assert_eq!(std::fs::read(root.join("runtime.json")).unwrap(),original);
    for field in ["path","pids","digest","failure","result"] {
        let mut corrupt=proof.clone();
        match field {
            "path"=>corrupt["binding"]["registration"]["session"]="/foreign".into(),
            "pids"=>corrupt["pids_absent"]=serde_json::json!([11,99]),
            "digest"=>corrupt["independent_retirement_sha256"]="invalid".into(),
            "failure"=>corrupt["retirement"]["passed"]=false.into(),
            _=>corrupt["session_result_sha256"]="f".repeat(64).into(),
        }
        save_atomic(&path,&corrupt).unwrap();
        assert!(matches!(preparation::inspect(&root,&spec.launch_id).unwrap().target_retirement,TargetRetirement::Unavailable {..}),"{field}");
    }
    save_atomic(&path,&proof).unwrap();
    save_atomic(&worker.join("watch-acquisition-ready.json"),&serde_json::json!({"request_sha256":"f".repeat(64)})).unwrap();
    assert!(matches!(preparation::inspect(&root,&spec.launch_id).unwrap().target_retirement,TargetRetirement::Unavailable {..}));
    assert_eq!(std::fs::read(root.join("runtime.json")).unwrap(),original);
}

#[test]
fn owner_loss_readback_rejects_foreign_session_changed_binding_and_failed_cleanup() {
    let worker = PathBuf::from("/runtime/native-session-volume/worker");let launch = LaunchId::new("g-test").unwrap();
    let mut binding = Binding { registration: Registration { operation: "watch_uikit".into(), session: worker.join("volume/mount/g-test"),
        worker: ProcessIdentity { pid: 11, started: "worker".into() }, ready_sha256: "a".repeat(64), retirement_reader: None },
        diagnostic: worker.join("volume/mount/g-test/worker.log.diagnostics/original"),
        diagnostic_process: ProcessIdentity { pid: 12, started: "log".into() }, diagnostic_config_sha256: "b".repeat(64), target_pids: [13,14], retirement_reader_sha256: None, launch_sha256: None };
    let mut bytes = serde_json::to_vec(&binding.registration).unwrap();bytes.push(b'\n');
    let mut ack = Acknowledged { request_sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes) };
    let mut proof = Proof { binding: serde_json::from_slice(&serde_json::to_vec(&binding).unwrap()).unwrap(),
        pids_absent: vec![11,12,13,14], session_result_sha256: Some("c".repeat(64)), independent_retirement_sha256: None, diagnostic_result_sha256: "d".repeat(64),
        retirement: Retirement { passed: true, errors: vec![], signal_error: None } };
    validate(&worker,&launch,&proof,&binding,&ack).unwrap();
    assert!(validate(&worker,&LaunchId::new("g-other").unwrap(),&proof,&binding,&ack).is_err());
    proof.pids_absent.pop();assert!(validate(&worker,&launch,&proof,&binding,&ack).is_err());proof.pids_absent.push(14);
    proof.retirement.passed=false;assert!(validate(&worker,&launch,&proof,&binding,&ack).is_err());proof.retirement.passed=true;
    proof.binding.registration.worker.pid=15;assert!(validate(&worker,&launch,&proof,&binding,&ack).is_err());
    proof.binding.registration.worker.pid=11;
    proof.session_result_sha256=None;
    assert!(validate(&worker,&launch,&proof,&binding,&ack).is_err());
    proof.independent_retirement_sha256=Some("e".repeat(64));
    assert!(validate(&worker,&launch,&proof,&binding,&ack).is_err());
    binding.registration.retirement_reader=Some("/original/ios_session_worker".into());
    binding.retirement_reader_sha256=Some("f".repeat(64)); binding.launch_sha256=Some("1".repeat(64));
    proof.binding=serde_json::from_slice(&serde_json::to_vec(&binding).unwrap()).unwrap();
    let mut bytes=serde_json::to_vec(&binding.registration).unwrap();bytes.push(b'\n');
    ack.request_sha256=frontcap_core::canonical::sha256_bytes_hex(&bytes);
    validate(&worker,&launch,&proof,&binding,&ack).unwrap();
    proof.independent_retirement_sha256=Some("invalid".into());
    assert!(validate(&worker,&launch,&proof,&binding,&ack).is_err());
}
