use super::*;
use frontcap_core::{ids::LaunchId,write_budget::Limits};

#[test]
fn visibility_dispatch_admits_history_and_input_invalidation_together() {
    let temp=tempfile::tempdir().unwrap();let root=temp.path();
    let history=History {owner:Owner {package:"org.example.client".into(),activity:"org.example.client.Main".into(),
        serial:"emulator-5580".into(),generation:"abcd0000-0000-0000-0000-000000000001".into(),
        pid:42,source_sha256:"a".repeat(64),apk_sha256:"b".repeat(64)},entries:vec![]};
    std::fs::create_dir(root.join("app-state")).unwrap();
    let path=root.join("app-state/history.json");save_atomic(&path,&history).unwrap();
    let latest=root.join("latest-view.json");save_atomic(&latest,&Some(RequestId::new("before").unwrap())).unwrap();
    let original_history=std::fs::read(&path).unwrap();let original_view=std::fs::read(&latest).unwrap();
    let mut next=history.clone();next.entries.push(Entry {id:RequestId::new("home").unwrap(),state:State::Background,completion:None});
    let size=frontcap_core::canonical::canonical_of(&next).len() as u64;
    let budget=WriteBudget::create(&root.join("budget"),LaunchId::new("g-small").unwrap(),
        Limits {ordinary:size,retirement:4096}).unwrap();
    assert!(publish_dispatch(root,&next,Some(&budget)).is_err());
    assert_eq!(std::fs::read(&path).unwrap(),original_history);
    assert_eq!(std::fs::read(&latest).unwrap(),original_view);
    assert_eq!(budget.charged().unwrap(),Limits {ordinary:0,retirement:0});
    let budget=WriteBudget::create(&root.join("enough"),LaunchId::new("g-enough").unwrap(),
        Limits {ordinary:size+4,retirement:4096}).unwrap();
    publish_dispatch(root,&next,Some(&budget)).unwrap();
    assert_eq!(std::fs::read(&latest).unwrap(),b"null");
    assert_eq!(budget.charged().unwrap(),Limits {ordinary:size+4,retirement:0});
}
