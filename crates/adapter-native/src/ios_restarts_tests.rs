use super::*;
use frontcap_core::{ids::LaunchId,native::{ios::ClockIdentity,ios_ui::UiIdentity},write_budget::Limits};
#[test]
fn restart_admits_url_evidence_and_intent_together_before_creating_attempt() {
    let temp=tempfile::tempdir().unwrap();let root=temp.path();
    let simulator="abcd0000-0000-0000-0000-000000000001";
    let owner=Owner {runner:UiIdentity {simulator:simulator.into(),generation:"abcd0000-0000-0000-0000-000000000002".into(),pid:42},
        app:ClockIdentity {simulator:simulator.into(),generation:"abcd0000-0000-0000-0000-000000000003".into(),pid:43},
        controller_id:"abcd0000-0000-0000-0000-000000000004".into(),source_sha256:"a".repeat(64)};
    let app=root.join("Client.app");std::fs::create_dir(&app).unwrap();
    plist::to_file_xml(app.join("Info.plist"),&serde_json::json!({"CFBundleIdentifier":"org.example.client",
        "CFBundleURLTypes":[{"CFBundleURLSchemes":["client-test"]}]})).unwrap();
    let inventory=format!(r#"{{ "org.example.client" = {{ CFBundleIdentifier = "org.example.client"; Path = "{}"; }}; }}"#,app.display());
    let admission=crate::ios_external_entry::inspect(inventory.as_bytes(),"org.example.client","client-test://item/1",||Ok(())).unwrap();
    let installation=Installation {bundle:"org.example.client".into(),app,data:root.join("data"),data_device:1,data_inode:2,app_sha256:"b".repeat(64)};
    let id=RequestId::new("restart").unwrap();
    let request=UiRequest {generation:owner.runner.generation.clone(),request_id:2,
        operation:UiOperation::Restart {app:owner.app.clone(),url:Some("client-test://item/1".into())}};
    let budget=WriteBudget::create(&root.join("budget"),LaunchId::new("g-uikit").unwrap(),
        Limits {ordinary:65536,retirement:4096}).unwrap();
    let mut history=History::create_with_budget(root,owner.clone(),installation.clone(),Some(&budget)).unwrap();
    let path=root.join("restarts/history.json");let original=std::fs::read(&path).unwrap();
    let admission_size=frontcap_core::canonical::canonical_of(&admission).len() as u64;
    // Enough for the URL evidence alone, but not its corresponding history snapshot.
    budget.charge(Class::Ordinary,65536-budget.charged().unwrap().ordinary-admission_size).unwrap();
    let charged=budget.charged().unwrap();
    assert!(history.dispatch(id.clone(),request.clone(),installation.clone(),Some(&admission)).is_err());
    assert!(!root.join("restarts/restart").exists());assert!(!history.contains(&id));history.ensure_resolved().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(),original);assert_eq!(budget.charged().unwrap(),charged);
    let control=tempfile::tempdir().unwrap();
    let control_budget=WriteBudget::create(&control.path().join("budget"),LaunchId::new("g-control").unwrap(),
        Limits {ordinary:65536,retirement:4096}).unwrap();
    let mut control_history=History::create_with_budget(control.path(),owner,installation.clone(),Some(&control_budget)).unwrap();
    let initial=control_budget.charged().unwrap().ordinary;
    control_history.dispatch(id,request,installation,Some(&admission)).unwrap();
    assert_eq!(control_budget.charged().unwrap().ordinary,initial+admission_size+
        std::fs::metadata(control.path().join("restarts/history.json")).unwrap().len());
    assert_eq!(control_budget.charged().unwrap().retirement,0);
}
