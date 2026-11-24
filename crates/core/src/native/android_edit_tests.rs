use super::*;
fn request() -> Request {
    Request {
        generation: "abcd0000-0000-0000-0000-000000000001".into(),
        controller_id: "abcd0000-0000-0000-0000-000000000002".into(),
        observation: "abcd0000-0000-0000-0000-000000000003".into(),
        target: "abcd0000-0000-0000-0000-000000000004".into(),
        request_id: 1,
        now_ns: 1000000,
        start: 4,
        end: 7,
        text: "수정".into(),
    }
}
#[test]
fn committed_unicode_replacement_preserves_sides_and_rejects_split_or_excess() {
    let mut r = request();
    assert_eq!(r.apply_to("한글😀abc 뒤").unwrap(), "한글😀수정 뒤");
    r.start = 3;
    assert!(r.apply_to("한글😀abc").is_err());
    r.start = 4;
    r.end = 3;
    assert!(r.apply_to("한글😀abc").is_err());
    r.end = 99;
    assert!(r.apply_to("한글😀abc").is_err());
    r.start = 0;
    r.end = 0;
    r.text = "x".repeat(16384);
    assert!(r.apply_to("a").is_err());
    assert_eq!(r.apply_to("").unwrap().len(), 16384);
}
#[test]
fn completed_receipts_bind_owner_request_and_actual_text() {
    let request = request();
    let receipt = Receipt {
        request: request.clone(),
        generation: request.generation.clone(),
        pid: 123,
        now_ns: request.now_ns,
        last_id: 1,
        state: State::Completed,
        before: Some("한글😀abc".into()),
        after: Some("한글😀수정".into()),
        reason: None,
    };
    receipt
        .validate(&request, &request.generation, 123)
        .unwrap();
    assert!(
        receipt
            .validate(&request, &request.generation, 124)
            .is_err()
    );
    let mut wrong = request.clone();
    wrong.target = wrong.controller_id.clone();
    assert!(receipt.validate(&wrong, &request.generation, 123).is_err());
    let mut changed = receipt.clone();
    changed.after = Some("한글😀abc".into());
    assert!(
        changed
            .validate(&request, &request.generation, 123)
            .is_err()
    );
    changed = receipt.clone();
    changed.state = State::Rejected;
    changed.reason = Some(Reason::StaleObservation);
    assert!(
        changed
            .validate(&request, &request.generation, 123)
            .is_err()
    );
    changed.before = None;
    changed.after = None;
    changed.now_ns += 1;
    changed
        .validate(&request, &request.generation, 123)
        .unwrap();
    changed.state = State::Pending;
    assert!(
        changed
            .validate(&request, &request.generation, 123)
            .is_err()
    );
}
