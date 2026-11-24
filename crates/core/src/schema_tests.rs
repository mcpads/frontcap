#[test]
fn locator_schema_lists_the_tags() {
    let schema = schemars::schema_for!(crate::predicate::Locator);
    let text = serde_json::to_string(&schema).unwrap();
    for tag in ["\"css\"", "\"test_id\"", "\"text\"", "\"role\""] {
        assert!(text.contains(tag), "{tag} in {text}");
    }
}

#[test]
fn ids_are_patterned_strings() {
    let schema = schemars::schema_for!(crate::ids::RequestId);
    let v = serde_json::to_value(&schema).unwrap();
    assert_eq!(v["type"], "string");
    assert_eq!(v["pattern"], "^[a-z0-9]+(-[a-z0-9]+)*$");
}

#[test]
fn rule_sets_have_a_schema() {
    let schema = schemars::schema_for!(crate::rules::RuleSet);
    let text = serde_json::to_string(&schema).unwrap();
    assert!(text.contains("passthrough") && text.contains("latency_ms"), "{text}");
}
