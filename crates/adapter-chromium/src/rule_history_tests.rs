use super::*;
use frontcap_core::{event::{RequestRecord, RulesInstalledRecord}, time::VirtualTime};

fn rules(url: &str) -> RuleSet {
    RuleSet::from_json(&serde_json::json!([{"id":"shared-id","match":{"url":url},"action":{"type":"hold"}}]).to_string()).unwrap()
}
fn save(dir: &Path, revision: u64, rules: &RuleSet) -> BehaviorEvent {
    std::fs::write(dir.join(format!("{revision:04}.json")), frontcap_core::canonical::canonical_of(rules)).unwrap();
    BehaviorEvent { seq: revision, t_us: VirtualTime::ZERO, kind: EventKind::RulesInstalled(RulesInstalledRecord {
        request_id: None, revision, sha256: rules.sha256(), count: rules.rules.len() as u32,
    }) }
}

#[test]
fn recovery_keeps_selected_inputs_and_ignores_unpublished_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let old = rules("*/old"); let new = rules("*/new");
    let events = vec![save(temp.path(), 0, &old), save(temp.path(), 1, &new)];
    save(temp.path(), 2, &rules("*/unpublished"));
    let recovered = load(temp.path(), &events).unwrap();
    assert_eq!(recovered.revision, 1);
    assert_eq!(recovered.active.sha256(), new.sha256());
    assert_eq!(recovered.previous[&0].sha256(), old.sha256());
    assert!(!recovered.previous.contains_key(&2));
}

#[test]
fn changed_published_input_is_rejected_even_when_its_rule_id_matches() {
    let temp = tempfile::tempdir().unwrap();
    let original = save(temp.path(), 0, &rules("*/old"));
    save(temp.path(), 0, &rules("*/replacement"));
    assert!(load(temp.path(), &[original]).is_err());
}

#[test]
fn publication_requires_contiguous_installations_but_identical_content_can_be_reinstalled() {
    let temp = tempfile::tempdir().unwrap();
    let set = rules("*");
    let zero = save(temp.path(), 0, &set);
    let one = save(temp.path(), 1, &set);
    let two = save(temp.path(), 2, &set);
    assert!(load(temp.path(), &[zero.clone(), two]).is_err());
    assert!(load(temp.path(), &[zero.clone(), zero.clone()]).is_err());
    assert_eq!(load(temp.path(), &[zero, one]).unwrap().revision, 1);
}

#[test]
fn request_cannot_select_an_unpublished_input_file() {
    let temp = tempfile::tempdir().unwrap();
    let zero = save(temp.path(), 0, &rules("*"));
    let one = save(temp.path(), 1, &rules("*/pending"));
    let request = BehaviorEvent { seq: 1, t_us: VirtualTime::ZERO, kind: EventKind::Request(RequestRecord {
        rules_revision: 1, id: "req-1".into(), method: "GET".into(), url: "http://fixture/pending".into(), rule: None, entry: None, context: None, body: None,
    }) };
    assert!(load(temp.path(), &[zero.clone(), request.clone()]).is_err());
    assert!(load(temp.path(), &[zero, request, one]).is_err(), "a later installation cannot explain an earlier request");
}
