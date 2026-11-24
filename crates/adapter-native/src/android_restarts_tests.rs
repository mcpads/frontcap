use super::*;
use frontcap_core::{ids::LaunchId,write_budget::Limits};
#[test]
fn restart_admission_refusal_preserves_history_and_identity() {
    let temp=tempfile::tempdir().unwrap();let root=temp.path();
    let budget=WriteBudget::create(&root.join("budget"),LaunchId::new("g-android").unwrap(),
        Limits {ordinary:65536,retirement:4096}).unwrap();
    let owner=Owner {package:"org.example.client".into(),activity:"org.example.client.Main".into(),
        serial:"emulator-5580".into(),generation:"abcd0000-0000-0000-0000-000000000001".into(),
        pid:42,source_sha256:"a".repeat(64),apk_sha256:"b".repeat(64)};
    let mut history=History::create_with_budget(root,owner.clone(),Some(&budget)).unwrap();
    let path=root.join("restarts/history.json");
    assert_eq!(budget.charged().unwrap().ordinary,std::fs::metadata(&path).unwrap().len());
    budget.charge(Class::Ordinary,65536-budget.charged().unwrap().ordinary).unwrap();
    let original=std::fs::read(&path).unwrap();let id=RequestId::new("restart").unwrap();
    assert!(history.dispatch(&id,&owner.generation,None).is_err());
    assert!(!history.contains(&id));assert!(!history.poisoned);
    assert_eq!(std::fs::read(&path).unwrap(),original);
    assert_eq!(budget.charged().unwrap().retirement,0);
    let control=tempfile::tempdir().unwrap();
    History::create_with_budget(control.path(),owner.clone(),None).unwrap().dispatch(&id,&owner.generation,None).unwrap();
}
