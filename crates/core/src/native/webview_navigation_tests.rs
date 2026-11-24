use super::*;
use serde_json::json;

fn sample() -> serde_json::Value {
    json!({"document":"abcd0000-0000-0000-0000-000000000001",
        "current":{"href":"http://localhost/new","hrefTruncated":false,"focused":false},
        "historyObserved":true,"failure":null,"received":10,"dropped":8,"events":[
            {"sequence":9,"elapsedMs":50.0,"href":"http://localhost/new","hrefTruncated":false,
             "focused":true,"kind":"push_state","trusted":null,"persisted":null},
            {"sequence":10,"elapsedMs":50.0,"href":"http://localhost/new","hrefTruncated":false,
             "focused":false,"kind":"pagehide","trusted":true,"persisted":true}]})
}

#[test]
fn navigation_retains_loss_trust_and_observation_failure() {
    let value = sample();
    let navigation: Navigation = serde_json::from_value(value.clone()).unwrap();
    assert!(navigation.valid());
    assert_eq!(serde_json::to_value(navigation).unwrap(), value);
    let mut unavailable = value;
    unavailable["current"] = json!(null);
    unavailable["historyObserved"] = json!(false);
    unavailable["failure"] = json!("focus unavailable");
    assert!(
        serde_json::from_value::<Navigation>(unavailable)
            .unwrap()
            .valid()
    );
}

#[test]
fn inconsistent_or_unbounded_navigation_is_not_admitted() {
    for (path, bad) in [
        ("/document", json!("other")),
        ("/dropped", json!(0)),
        ("/events/0/sequence", json!(10)),
        ("/events/0/elapsedMs", json!(-1)),
        ("/events/0/trusted", json!(true)),
        ("/events/1/persisted", json!(null)),
        ("/events/1/href", json!("🚆".repeat(1025))),
        ("/current", json!(null)),
        ("/current/href", json!("x".repeat(2049))),
        ("/failure", json!("x".repeat(257))),
    ] {
        let mut value = sample();
        *value.pointer_mut(path).unwrap() = bad;
        assert!(
            !serde_json::from_value::<Navigation>(value).unwrap().valid(),
            "{path}"
        );
    }
}
