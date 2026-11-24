use super::*;
use serde_json::json;
#[test]
fn native_speech_retains_terminal_occurrence_after_tail_eviction() {
    let callbacks =
        json!({"start":true,"finish":true,"cancel":false,"pause":false,"resume":false});
    let value = json!({"process":"abcd0000-0000-0000-0000-000000000001","pid":42,
        "apiObserved":true,"delegateObserved":true,"received":40,"dropped":39,
        "truncated":false,"faults":0,"utteranceLimitReached":false,"synthesizers":[],"omittedSynthesizers":0,
        "events":[{"sequence":40,"synthesizer":1,"utterance":null,"kind":"stop-returned",
            "speaking":false,"paused":false,"accepted":true,"implementedCallbacks":null}],
        "utterances":[{"id":1,"requests":1,"starts":1,"finishes":1,"cancellations":0,"failures":0,
            "lastSequence":4,"lastKind":"finished","implementedCallbacks":callbacks}]});
    let history: History = serde_json::from_value(value.clone()).unwrap();
    assert!(history.valid());
    assert_eq!(serde_json::to_value(history).unwrap(), value);
    for (path, replacement) in [
        ("/pid", json!(0)),
        ("/process", json!("invalid")),
        ("/events/0/sequence", json!(39)),
        ("/events/0/accepted", json!(null)),
        ("/utterances/0/lastSequence", json!(40)),
        ("/utterances/0/id", json!(65)),
        ("/utterances/0/starts", json!(u64::MAX)),
        ("/apiObserved", json!(false)),
    ] {
        let mut broken = value.clone();
        *broken.pointer_mut(path).unwrap() = replacement;
        assert!(
            !serde_json::from_value::<History>(broken).unwrap().valid(),
            "{path}"
        );
    }
}
#[test]
#[ignore = "requires freshly generated speech_observer_test.m output"]
fn native_speech_decodes_native_observer_fixture() {
    let path = std::env::var("FRONTCAP_SPEECH_FIXTURE").expect("native fixture output path");
    let raw: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let history: History = serde_json::from_value(raw.clone()).unwrap();
    assert!(history.valid());
    assert!(history.dropped > 0 && history.delegate_observed);
    assert!(
        history
            .utterances
            .iter()
            .any(|u| u.cancellations > 0 && u.last_sequence <= history.dropped)
    );
    assert_eq!(serde_json::to_value(history).unwrap(), raw);
}
