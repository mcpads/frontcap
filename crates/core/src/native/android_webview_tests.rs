use super::*;
use serde_json::json;
const DOCUMENT: &str = "abcd0000-0000-0000-0000-000000000001";
fn sample() -> serde_json::Value {
    json!({"href":"http://localhost/","ready_state":"complete","visibility":"visible",
        "focused":false,"performance_ms":500.0,"date_ms":1700000000500.0,
        "page_events":{"document":DOCUMENT,"received":10,"dropped":9,"events":[
            {"sequence":10,"elapsedMs":500.0,"kind":"message","origin":"http://localhost",
             "originTruncated":false,"dataType":"string","data":"metadata","truncated":false}]}})
}
#[test]
fn page_message_history_preserves_original_document_and_explicit_loss() {
    let value=sample();
    let snapshot:Snapshot=serde_json::from_value(value.clone()).unwrap();
    snapshot.validate(Some(DOCUMENT)).unwrap();
    assert_eq!(serde_json::to_value(snapshot).unwrap(),value);
    let mut unavailable=value.clone();
    unavailable["page_events"]["failure"]=json!("observation stopped");
    serde_json::from_value::<Snapshot>(unavailable).unwrap().validate(Some(DOCUMENT)).unwrap();
    let mut legacy=value;legacy.as_object_mut().unwrap().remove("page_events");
    let snapshot:Snapshot=serde_json::from_value(legacy.clone()).unwrap();
    snapshot.validate(None).unwrap();
    assert_eq!(serde_json::to_value(snapshot).unwrap(),legacy);
}
#[test]
fn foreign_or_inconsistent_message_history_rejects() {
    for (path,bad) in [
        ("/page_events/document",json!("abcd0000-0000-0000-0000-000000000002")),
        ("/page_events/dropped",json!(0)),
        ("/page_events/events/0/sequence",json!(9)),
        ("/page_events/events/0/data",json!("x".repeat(65))),
        ("/page_events/events/0/dataType",json!("object")),
        ("/page_events/events/0/elapsedMs",json!(-1))] {
        let mut value=sample();*value.pointer_mut(path).unwrap()=bad;
        assert!(serde_json::from_value::<Snapshot>(value).unwrap().validate(Some(DOCUMENT)).is_err(),"{path}");
    }
    let mut value=sample();
    value["navigation"]=json!({"document":"abcd0000-0000-0000-0000-000000000002",
        "current":{"href":"http://localhost/","hrefTruncated":false,"focused":false},
        "historyObserved":true,"failure":null,"received":0,"dropped":0,"events":[]});
    assert!(serde_json::from_value::<Snapshot>(value).unwrap().validate(None).is_err());
}

#[test]
fn library_publication_is_optional_and_rejects_foreign_document_evidence() {
    for published in [false, true] {
        let mut value = sample();
        value["canvas_kit"] = json!({"document": DOCUMENT, "published": published});
        let mut snapshot: Snapshot = serde_json::from_value(value.clone()).unwrap();
        snapshot.validate(Some(DOCUMENT)).unwrap();
        assert_eq!(serde_json::to_value(&snapshot).unwrap(), value);
        snapshot.canvas_kit.as_mut().unwrap().document = "abcd0000-0000-0000-0000-000000000002".into();
        assert!(snapshot.validate(Some(DOCUMENT)).is_err());
        // The retained page evidence must agree even without an explicit caller document.
        assert!(snapshot.validate(None).is_err());
        snapshot.page_events = None;
        snapshot.canvas_kit.as_mut().unwrap().document = "unknown".into();
        assert!(snapshot.validate(None).is_err());
    }
}

#[test]
fn entry_attachment_can_observe_held_resources_after_parsing_but_not_initial_navigation() {
    let mut snapshot: Snapshot = serde_json::from_value(sample()).unwrap();
    for state in ["loading", "interactive", "complete", "unknown"] {
        snapshot.ready_state = state.into();
        assert_eq!(snapshot.entry_document_parsed("http://localhost/"),
            matches!(state, "interactive" | "complete"));
        assert_eq!(snapshot.initial_navigation_finished(), state == "complete");
        assert!(!snapshot.entry_document_parsed("http://localhost/replaced"));
    }
}

#[test]
fn starting_randomness_retains_scope_and_rejects_foreign_or_impossible_counters() {
    let mut value = sample();
    value["starting_randomness"] = json!({"document":DOCUMENT,"seed":42,
        "math_calls":2,"crypto_byte_calls":1,"crypto_uuid_calls":1,
        "observation_errors":0,"uncontrolled_calls":1});
    let snapshot:Snapshot=serde_json::from_value(value.clone()).unwrap();
    snapshot.validate(Some(DOCUMENT)).unwrap();
    assert_eq!(serde_json::to_value(snapshot).unwrap(), value);
    for (key,bad) in [("document",json!("abcd0000-0000-0000-0000-000000000002")),
        ("observation_errors",json!(5)),("math_calls",json!(u64::MAX)),
        ("uncontrolled_calls",json!(9_007_199_254_740_992_u64))] {
        let mut invalid=value.clone();invalid["starting_randomness"][key]=bad;
        assert!(serde_json::from_value::<Snapshot>(invalid).unwrap().validate(None).is_err());
    }
}
