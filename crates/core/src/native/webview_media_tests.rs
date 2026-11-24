use super::*;
use serde_json::json;
const DOCUMENT: &str = "abcd0000-0000-0000-0000-000000000001";
fn sample() -> serde_json::Value {
    let state = json!({"sourceId":1,"requestedSourceId":1,"currentTime":0.5,"duration":0.5,
        "paused":true,"ended":true,"seeking":false,"loop":false,"readyState":4,
        "networkState":1,"muted":true,"volume":1.0,"playbackRate":1.0,"error":null});
    json!({"document":DOCUMENT,"sourceCount":1,"sourceLimitReached":false,
        "available":true,"controlled":false,"mutedOutput":true,"hostMs":800.0,
        "received":3,"dropped":2,"omitted":0,"faults":0,
        "players":[{"id":1,"sourceId":1,"requestedSourceId":1,"currentTime":0.5,"duration":0.5,
        "paused":true,"ended":true,"seeking":false,"loop":false,"readyState":4,
        "networkState":1,"muted":true,"volume":1.0,"playbackRate":1.0,"error":null}],
        "events":[{"sequence":3,"id":1,"kind":"ended","hostMs":700.0,"pageMs":100.0,"state":state}]})
}
#[test]
fn media_flow_retains_source_identity_loss_and_refuses_invalid_evidence() {
    let value = sample();
    let history: History = serde_json::from_value(value.clone()).unwrap();
    assert!(history.valid());
    assert_eq!(serde_json::to_value(history).unwrap(), value);
    for (pointer, replacement) in [
        ("/sourceCount", json!(0)),
        ("/controlled", json!(true)),
        ("/events/0/sequence", json!(2)),
        ("/events/0/id", json!(2)),
        ("/events/0/hostMs", json!(900)),
        ("/dropped", json!(0)),
        ("/players/0/sourceId", json!(2)),
        ("/players/0/volume", json!(2)),
        ("/document", json!("foreign")),
        ("/sourceLimitReached", json!(null)),
    ] {
        let mut broken = value.clone();
        *broken.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            !serde_json::from_value::<History>(broken).unwrap().valid(),
            "{pointer}"
        );
    }
    let mut legacy = value;
    for key in ["document", "sourceCount", "sourceLimitReached"] {
        legacy.as_object_mut().unwrap().remove(key);
    }
    for state in ["/players/0", "/events/0/state"] {
        let object = legacy.pointer_mut(state).unwrap().as_object_mut().unwrap();
        object.remove("sourceId");
        object.remove("requestedSourceId");
    }
    let decoded: History = serde_json::from_value(legacy.clone()).unwrap();
    assert!(decoded.valid());
    assert_eq!(serde_json::to_value(decoded).unwrap(), legacy);
}
#[test]
fn media_flow_is_bound_to_the_android_document() {
    let value = json!({"href":"http://localhost/","ready_state":"complete","visibility":"visible",
        "focused":true,"performance_ms":100.0,"date_ms":1000.0,"media":sample()});
    let mut snapshot: super::super::android_webview::Snapshot =
        serde_json::from_value(value).unwrap();
    snapshot.validate(Some(DOCUMENT)).unwrap();
    assert!(
        snapshot
            .validate(Some("abcd0000-0000-0000-0000-000000000002"))
            .is_err()
    );
    snapshot.media.as_mut().unwrap().document = None;
    assert!(snapshot.validate(None).is_err());
}

#[test]
fn media_flow_summary_survives_tail_loss_and_rejects_contradictions() {
    let mut value = sample();
    value["flow"] = json!({"unattributed":0,"sources":[{"sourceId":1,"events":{
        "play-call":{"count":1,"lastSequence":1},
        "playing":{"count":1,"lastSequence":2},
        "ended":{"count":1,"lastSequence":3}
    }}]});
    let history: History = serde_json::from_value(value.clone()).unwrap();
    assert!(history.valid());
    assert_eq!(serde_json::to_value(history).unwrap(), value);
    for (pointer, replacement) in [
        ("/flow/unattributed", json!(4)),
        ("/flow/sources/0/sourceId", json!(2)),
        ("/flow/sources/0/events/ended/count", json!(0)),
        ("/flow/sources/0/events/ended/lastSequence", json!(4)),
        ("/flow/sources/0/events/ended/lastSequence", json!(2)),
        ("/flow/sources/0/events/playing/count", json!(2)),
    ] {
        let mut broken = value.clone();
        *broken.pointer_mut(pointer).unwrap() = replacement;
        assert!(
            !serde_json::from_value::<History>(broken).unwrap().valid(),
            "{pointer}"
        );
    }
    let mut missing = value.clone();
    missing["flow"]["sources"][0]["events"]
        .as_object_mut()
        .unwrap()
        .remove("ended");
    assert!(!serde_json::from_value::<History>(missing).unwrap().valid());
    let mut duplicate = value.clone();
    let source = duplicate["flow"]["sources"][0].clone();
    duplicate["flow"]["sources"]
        .as_array_mut()
        .unwrap()
        .push(source);
    assert!(
        !serde_json::from_value::<History>(duplicate)
            .unwrap()
            .valid()
    );
}
