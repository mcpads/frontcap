use super::*;
use crate::request::ClockOperation;

const GENERATION: &str = "4d75203e-0a3e-4c68-8e3b-77f0a831ab83";
fn retained(raw: serde_json::Value) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let bytes = serde_json::to_vec(&raw).unwrap();
    let hash = sha256_bytes_hex(&bytes);
    std::fs::write(dir.path().join(format!("{hash}.json")), bytes).unwrap();
    (dir, hash)
}
#[test]
fn clock_success_and_nonconsuming_rejection_require_exact_receipt_fields() {
    let request = ClockRequest {
        generation: GENERATION.into(),
        request_id: 1,
        operation: ClockOperation::Advance,
        delta_ns: 1000,
    };
    for rejected in [false, true] {
        let raw = serde_json::json!({"request": request, "generation": GENERATION,
            "pid": 123, "last_id": if rejected { 0 } else { 1 },
            "status": if rejected { "rejected" } else { "ok" }, "before_ns": 0,
            "now_ns": if rejected { 0 } else { 1000 }, "applied": !rejected,
            "error": if rejected { Some("clock_overflow") } else { None }, "replayed": false});
        let (dir, hash) = retained(raw);
        let completion = if rejected {
            Completion::ClockRejected {
                request: request.clone(),
                generation: GENERATION.into(),
                pid: 123,
                last_id: 0,
                before_ns: 0,
                now_ns: 0,
                applied: false,
                error: ClockRejection::ClockOverflow,
                receipt_sha256: hash,
            }
        } else {
            Completion::Clock {
                request: request.clone(),
                generation: GENERATION.into(),
                pid: 123,
                last_id: 1,
                before_ns: 0,
                now_ns: 1000,
                applied: true,
                receipt_sha256: hash,
            }
        };
        verify(dir.path(), &completion).unwrap();
        let mut wrong = completion.clone();
        match &mut wrong {
            Completion::Clock { now_ns, .. } | Completion::ClockRejected { now_ns, .. } => {
                *now_ns += 1
            }
            _ => unreachable!(),
        }
        assert!(verify(dir.path(), &wrong).is_err());
    }
}
