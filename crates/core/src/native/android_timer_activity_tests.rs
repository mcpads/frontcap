use super::*;
use serde_json::json;

fn sample() -> serde_json::Value {
    json!({"document":"abcd0000-0000-0000-0000-000000000001", "sampledMs":25.0,
        "methodsObserved":true,"failure":null,"registered":4,"invoked":3,"canceled":1,
        "executing":0,"untrackedRegistrations":1,"uncertainCancellations":1,
        "tracked":2,"omitted":0,"timers":[
            {"id":1,"registration":2,"kind":"interval","registeredMs":10.0,"requestedDelayMs":20.0,"calls":3},
            {"id":2,"registration":4,"kind":"timeout","registeredMs":20.0,"requestedDelayMs":null,"calls":0}]})
}

#[test]
fn timer_activity_preserves_partial_coverage_and_validates_bounded_records() {
    let value = sample();
    let activity: Activity = serde_json::from_value(value.clone()).unwrap();
    assert!(activity.valid());
    assert_eq!(serde_json::to_value(activity).unwrap(), value);
    let mut unavailable = value.clone();
    unavailable["sampledMs"] = json!(null);
    unavailable["methodsObserved"] = json!(false);
    unavailable["failure"] = json!("Timer observation failed");
    assert!(
        serde_json::from_value::<Activity>(unavailable)
            .unwrap()
            .valid()
    );
    for (path, bad) in [
        ("/document", json!("foreign")),
        ("/sampledMs", json!(null)),
        ("/sampledMs", json!(-1)),
        ("/registered", json!(9_007_199_254_740_992_u64)),
        ("/untrackedRegistrations", json!(5)),
        ("/canceled", json!(5)),
        ("/executing", json!(4)),
        ("/tracked", json!(4097)),
        ("/omitted", json!(1)),
        ("/failure", json!("x".repeat(257))),
        ("/timers/0/id", json!(0)),
        ("/timers/0/id", json!(2)),
        ("/timers/0/id", json!(2_147_483_648_u32)),
        ("/timers/1/registration", json!(1)),
        ("/timers/1/registration", json!(5)),
        ("/timers/0/registeredMs", json!(-1)),
        ("/timers/0/calls", json!(4)),
        ("/timers/1/calls", json!(1)),
    ] {
        let mut invalid = value.clone();
        *invalid.pointer_mut(path).unwrap() = bad;
        assert!(
            !serde_json::from_value::<Activity>(invalid).unwrap().valid(),
            "{path}"
        );
    }
}

#[test]
fn timer_activity_is_optional_but_when_present_belongs_to_the_observed_document() {
    use crate::native::android_webview::Snapshot;
    let mut snapshot: Snapshot = serde_json::from_value(json!({"href":"http://localhost/",
        "ready_state":"complete","visibility":"visible","focused":true,
        "performance_ms":25.0,"date_ms":1000.0}))
    .unwrap();
    snapshot.validate(None).unwrap();
    assert!(
        serde_json::to_value(&snapshot)
            .unwrap()
            .get("timer_activity")
            .is_none()
    );
    snapshot.timer_activity = Some(serde_json::from_value(sample()).unwrap());
    snapshot
        .validate(Some("abcd0000-0000-0000-0000-000000000001"))
        .unwrap();
    assert!(
        snapshot
            .validate(Some("abcd0000-0000-0000-0000-000000000002"))
            .is_err()
    );
    snapshot.navigation = Some(
        serde_json::from_value(json!({
        "document":"abcd0000-0000-0000-0000-000000000002",
        "current":{"href":"http://localhost/","hrefTruncated":false,"focused":true},
        "historyObserved":true,"failure":null,"received":0,"dropped":0,"events":[]}))
        .unwrap(),
    );
    assert!(snapshot.validate(None).is_err());
}
