use super::*;
use serde_json::json;
#[test]
fn timer_queue_keeps_bounded_schedules_and_explicit_unavailable_evidence() {
    let value = json!({"timers":[{"id":1000000000000_u64,"kind":"interval","dueMs":20.0},
        {"id":1000000000001_u64,"kind":"timeout","dueMs":0.0}],"omitted":4,"failure":null});
    let queue: TimerQueue = serde_json::from_value(value.clone()).unwrap();
    assert!(queue.valid(6));
    assert_eq!(serde_json::to_value(queue).unwrap(), value);
    let unavailable: TimerQueue = serde_json::from_value(json!({"timers":[],"omitted":6,
        "failure":"Timer schedule unavailable"})).unwrap();
    assert!(unavailable.valid(6));
    for (path, bad) in [
        ("/omitted", json!(0)), ("/timers/0/id", json!(0)),
        ("/timers/0/id", json!(1000000000001_u64)),
        ("/timers/0/id", json!(9_007_199_254_740_992_u64)),
        ("/failure", json!("unavailable")),
    ] {
        let mut invalid = value.clone();
        *invalid.pointer_mut(path).unwrap() = bad;
        assert!(!serde_json::from_value::<TimerQueue>(invalid).unwrap().valid(6), "{path}");
    }
    let too_many = TimerQueue {timers:(1..=33).map(|id| ScheduledTimer {
        id, kind:TimerKind::Timeout, due_ms:0.0 }).collect(), omitted:0, failure:None};
    assert!(!too_many.valid(33));
}
fn observed() -> serde_json::Value {
    json!({"visibility":"visible",
        "events":{"received":12,"dropped":11,"events":[{"sequence":12,"elapsedMs":100,
            "kind":"message","origin":"local","originTruncated":false,"dataType":"string",
            "data":"LIFECYCLE_RESUME","truncated":false}]},
        "pointers":{"received":3,"dropped":2,"active":1,"maximumActive":2,"events":[
            {"sequence":3,"kind":"pointermove","id":-7,"trusted":true,"x":12.5,"y":-1,
             "pageMs":100,"active":1}]}})
}
#[test]
fn bounded_history_preserves_loss_and_active_contacts() {
    let value = observed();
    let observation: Observation = serde_json::from_value(value.clone()).unwrap();
    assert!(observation.valid());
    let encoded = serde_json::to_value(&observation).unwrap();
    assert_eq!(encoded["pointers"]["dropped"], 2);
    assert_eq!(encoded["pointers"]["active"], 1);
    assert_eq!(
        serde_json::from_value::<Observation>(encoded).unwrap(),
        observation
    );
    let absent: Observation = serde_json::from_value(json!({})).unwrap();
    assert!(absent.valid());
    assert_eq!(serde_json::to_value(absent).unwrap(), json!({}));
}
#[test]
fn inconsistent_or_unbounded_history_is_not_admitted() {
    for (field, value) in [
        ("dropped", json!(0)),
        ("active", json!(3)),
        ("received", json!(u64::MAX)),
    ] {
        let mut raw = observed();
        raw["pointers"][field] = value;
        assert!(!serde_json::from_value::<Observation>(raw).unwrap().valid());
    }
    let mut raw = observed();
    raw["events"]["events"][0]["data"] = json!("x".repeat(65));
    assert!(!serde_json::from_value::<Observation>(raw).unwrap().valid());
    let mut raw = observed();
    raw["events"]["received"] = json!(20);
    raw["events"]["events"] = json!(
        (12..=20)
            .map(|n| json!({"sequence":n,"elapsedMs":100,"kind":"visibility","state":"hidden"}))
            .collect::<Vec<_>>()
    );
    assert!(!serde_json::from_value::<Observation>(raw).unwrap().valid());
}
