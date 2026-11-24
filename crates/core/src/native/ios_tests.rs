use super::*;
fn identity() -> ClockIdentity {
    ClockIdentity {
        simulator: "ABCD0000-0000-0000-0000-000000000001".into(),
        generation: "abcd0000-0000-0000-0000-000000000002".into(),
        pid: 123,
    }
}
#[test]
fn clock_completion_preserves_unapplied_failure_and_applied_drain_failure() {
    let identity = identity();
    let request = ClockRequest {
        generation: identity.generation.clone(),
        request_id: 3,
        operation: ClockOperation::Advance,
        delta_ns: 1000,
    };
    for (advance, drain, applied, status) in [
        (0, Some(0), true, ClockStatus::Ok),
        (60, None, false, ClockStatus::Failed),
        (0, Some(60), true, ClockStatus::Failed),
    ] {
        let completion = ClockCompletion {
            request: request.clone(),
            generation: identity.generation.clone(),
            pid: identity.pid,
            last_id: 3,
            status,
            before_ns: 10,
            now_ns: if applied { 1010 } else { 10 },
            applied,
            advance_result: Some(advance),
            drain_result: drain,
            receipt_sha256: "a".repeat(64),
        };
        completion.validate(&identity, &request).unwrap();
        let mut invalid = completion.clone();
        invalid.last_id = 2;
        assert!(invalid.validate(&identity, &request).is_err());
        invalid = completion.clone();
        invalid.applied = !applied;
        assert!(invalid.validate(&identity, &request).is_err());
        invalid = completion.clone();
        invalid.now_ns += 1;
        assert!(invalid.validate(&identity, &request).is_err());
        let mut replaced = identity.clone();
        replaced.pid += 1;
        assert!(completion.validate(&replaced, &request).is_err());
    }
}
#[test]
fn read_cannot_hide_a_mutation_or_use_another_generation() {
    let identity = identity();
    let request = ClockRequest {
        generation: identity.generation.clone(),
        request_id: 1,
        operation: ClockOperation::Read,
        delta_ns: 0,
    };
    let completion = ClockCompletion {
        request: request.clone(),
        generation: identity.generation.clone(),
        pid: identity.pid,
        last_id: 1,
        status: ClockStatus::Ok,
        before_ns: 10,
        now_ns: 10,
        applied: false,
        advance_result: None,
        drain_result: None,
        receipt_sha256: "a".repeat(64),
    };
    completion.validate(&identity, &request).unwrap();
    let mut wrong = request.clone();
    wrong.delta_ns = 1;
    assert!(identity.validate_request(&wrong).is_err());
    wrong = request.clone();
    wrong.generation = identity.simulator.to_ascii_lowercase();
    assert!(identity.validate_request(&wrong).is_err());
    let mut wrong = completion;
    wrong.advance_result = Some(0);
    assert!(wrong.validate(&identity, &request).is_err());
}
