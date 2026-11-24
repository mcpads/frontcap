use super::*;


#[derive(Deserialize)]
struct RecordedPress {
    app: Endpoint,
    #[serde(flatten)]
    evidence: Evidence,
}
#[derive(Deserialize)]
struct Endpoint {
    generation: String,
    pid: u32,
    port: u16,
}

#[test]
fn source_press_requires_one_owned_clock_delivery_between_matching_touches() {
    let recorded: RecordedPress =
        serde_json::from_str(include_str!("../../tests/fixtures/uikit-press.json")).unwrap();
    let evidence = recorded.evidence;
    assert_eq!(recorded.app.port, evidence.clock_reply.port);
    let app = ClockIdentity {
        simulator: "abcd0000-0000-0000-0000-000000000001".into(),
        generation: recorded.app.generation,
        pid: recorded.app.pid,
    };
    let validate = |e: &Evidence| {
        e.validate(
            &app,
            &evidence.reference,
            &evidence.observation,
            &evidence.clock_request,
        )
    };
    validate(&evidence).unwrap();
    let corruptions: &[fn(&mut Evidence)] = &[
        |e| e.clock_reply.replayed = true,
        |e| e.clock_reply.pid += 1,
        |e| e.clock_reply.now_ns += 1,
        |e| e.began.serial += 1,
        |e| e.ended.touches[0].phase = 4,
        |e| e.ended.touches[0].input_label = "Body".into(),
        |e| e.reference = format!("{}-0", e.observation),
        |e| e.clock_request.request_id += 1,
        |e| e.ended.generation = "abcd0000-0000-0000-0000-000000000002".into(),
    ];
    for corrupt in corruptions {
        let mut invalid = evidence.clone();
        corrupt(&mut invalid);
        assert!(
            validate(&invalid).is_err(),
            "accepted corrupted press: {invalid:?}"
        );
    }
}
