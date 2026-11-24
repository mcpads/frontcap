use super::*;
#[test]
fn bounded_delivery_requires_consistent_tail_and_document() {
    let mut snapshot: Snapshot = serde_json::from_value(serde_json::json!({
        "document":"abcd0000-0000-0000-0000-000000000001", "received":10,"dropped":9,
        "active":0,"maximumActive":2,"events":[{"sequence":10,"kind":"pointercancel",
            "id":1,"trusted":true,"x":12.0,"y":20.0,"pageMs":100.0,"active":0}]
    }))
    .unwrap();
    assert!(snapshot.validate().is_ok());
    snapshot.events[0].sequence = 9;
    assert!(snapshot.validate().is_err());
    snapshot.events[0].sequence = 10;
    snapshot.dropped = 8;
    assert!(snapshot.validate().is_err());
    snapshot.dropped = 9;
    snapshot.active = 1;
    assert!(snapshot.validate().is_err());
    snapshot.active = 0;
    snapshot.document = "foreign".into();
    assert!(snapshot.validate().is_err());
}
