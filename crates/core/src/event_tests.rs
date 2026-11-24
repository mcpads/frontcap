use super::*;
use crate::ids::{RequestId, WatchName};
use crate::time::VirtualTime;

#[test]
fn behavior_event_envelope_is_flat() {
    let e = BehaviorEvent {
        seq: 3,
        t_us: VirtualTime(3_500_000),
        kind: EventKind::Watch(WatchSample {
            name: WatchName::new("toast-shown").unwrap(),
            value: SampleValue::True,
            point: PointKind::Mutation,
            rseq: 0,
            document: 0,
            context: None,
        }),
    };
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v, serde_json::json!({
        "seq": 3, "t_us": 3500000, "kind": "watch",
        "payload": { "name": "toast-shown", "value": "true", "point": "mutation" }
    }));
    let back: BehaviorEvent = serde_json::from_value(v).unwrap();
    assert_eq!(back, e);
}

#[test]
fn clock_record_carries_wall_time_only_in_payload() {
    let e = BehaviorEvent {
        seq: 0,
        t_us: VirtualTime(0),
        kind: EventKind::Clock(ClockRecord {
            op: ClockOp::Settle,
            phase: ClockPhase::End,
            goal_us: None,
            outcome: Some(ClockOutcome::Quiescent),
            wall_ms: Some(12),
        }),
    };
    let v = serde_json::to_value(&e).unwrap();
    assert_eq!(v["payload"]["wall_ms"], 12);
    assert_eq!(v["payload"]["outcome"], serde_json::json!({ "type": "quiescent" }));
}

#[test]
fn action_record_round_trips() {
    let e = BehaviorEvent {
        seq: 1,
        t_us: VirtualTime(10),
        kind: EventKind::Action(ActionRecord {
            received_events: None,
            pointer: None,
            keys: None,
            request_id: RequestId::new("click-1").unwrap(),
            action: "click".into(),
            target: Some("e12".into()),
            args: Some(serde_json::json!({"text": "hi"})),
            delivered_to: Some("e13".into()),
            context: Some("popup-1".into()),
        }),
    };
    let s = serde_json::to_string(&e).unwrap();
    assert_eq!(serde_json::from_str::<BehaviorEvent>(&s).unwrap(), e);
}

#[test]
fn acquisition_event_places_itself_after_a_behavior_seq() {
    let a = AcquisitionEvent {
        aseq: 0,
        after_seq: 7,
        t_us: VirtualTime(100),
        kind: AcquisitionKind::Observation(ObservationRecord { call: "snapshot".into(), summary: "12 nodes".into() }),
    };
    let v = serde_json::to_value(&a).unwrap();
    assert_eq!(v["after_seq"], 7);
    assert_eq!(v["kind"], "observation");
}

#[test]
fn dialog_and_exception_records_have_their_kinds() {
    let d = EventKind::Dialog(DialogRecord { kind: DialogKind::Confirm, message: "Delete?".into(), answer: "false".into(), native: None, observer: None, context: None });
    let v = serde_json::to_value(BehaviorEvent { seq: 1, t_us: VirtualTime(5), kind: d.clone() }).unwrap();
    assert_eq!(v["kind"], "dialog");
    assert_eq!(v["payload"]["kind"], "confirm");
    assert_eq!(serde_json::from_value::<BehaviorEvent>(v).unwrap().kind, d);
    let e = EventKind::Exception(ExceptionRecord { text: "Uncaught Error: boom".into(), native: None, receipt: None, context: None });
    assert_eq!(serde_json::to_value(BehaviorEvent { seq: 2, t_us: VirtualTime(5), kind: e }).unwrap()["kind"], "exception");
}
