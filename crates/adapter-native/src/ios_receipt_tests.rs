use super::*;
use frontcap_core::native::{
    ios::ClockIdentity,
    ios_targets::NotAppliedReason,
    ios_ui::{UiIdentity, UiOperation},
};

#[test]
fn press_receipt_projection_preserves_source_evidence_and_rejects_extra_fields() {
    let bytes = include_bytes!("../../core/tests/fixtures/uikit-press.json");
    let raw: Press = serde_json::from_slice(bytes).unwrap();
    let app = ClockIdentity {
        simulator: "abcd0000-0000-0000-0000-000000000001".into(),
        generation: raw.app.generation,
        pid: raw.app.pid,
    };
    let e = &raw.evidence;
    e.validate(&app, &e.reference, &e.observation, &e.clock_request).unwrap();
    assert_eq!(raw.app.port, e.clock_reply.port);
    let mut extra: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    extra["unexpected"] = true.into();
    assert!(serde_json::from_value::<Press>(extra).is_err());
}
#[test]
fn no_effect_receipts_are_verified_against_persisted_bytes() {
    let owner = Owner {
        runner: UiIdentity {
            simulator: "abcd0000-0000-0000-0000-000000000001".into(),
            generation: "abcd0000-0000-0000-0000-000000000002".into(),
            pid: 123,
        },
        app: ClockIdentity {
            simulator: "abcd0000-0000-0000-0000-000000000001".into(),
            generation: "abcd0000-0000-0000-0000-000000000003".into(),
            pid: 124,
        },
        controller_id: "abcd0000-0000-0000-0000-000000000004".into(),
        source_sha256: "a".repeat(64),
    };
    let observation = "abcd0000-0000-0000-0000-000000000009".to_string();
    let reference = format!("{observation}-0");
    let request = UiRequest {
        generation: owner.runner.generation.clone(),
        request_id: 2,
        operation: UiOperation::TapTarget {
            reference: reference.clone(),
            observation: observation.clone(),
        },
    };
    let raw = serde_json::to_vec(&serde_json::json!({
        "request":request,"generation":owner.runner.generation,"pid":owner.runner.pid,"last_id":2,"status":"ok","port":1234,
        "result":{"input":"not_applied","app":{"generation":owner.app.generation,"pid":owner.app.pid,"port":1235},
            "reference":reference,"observation":observation,"reason":"observation_changed"}
    })).unwrap();
    let digest = sha256_bytes_hex(&raw);
    let root = tempfile::tempdir().unwrap();
    let receipts = root.path().join("ui-client/receipts");
    std::fs::create_dir_all(&receipts).unwrap();
    let path = receipts.join(format!("{digest}.json"));
    std::fs::write(&path, &raw).unwrap();
    let mut completion = Completion::Ui(Box::new(UiCompletion {
        request: request.clone(),
        generation: owner.runner.generation.clone(),
        pid: owner.runner.pid,
        last_id: 2,
        receipt_sha256: digest,
        outcome: UiOutcome::TargetNotApplied {
            app: owner.app.clone(),
            reference: reference.clone(),
            observation: observation.clone(),
            reason: NotAppliedReason::ObservationChanged,
        },
    }));
    let bound = Request::Ui(request);
    verify(root.path(), &owner, &bound, &completion).unwrap();
    if let Completion::Ui(value) = &mut completion {
        value.outcome = UiOutcome::TargetTapped {
            app: owner.app.clone(),
            reference,
            observation,
        };
    }
    assert!(verify(root.path(), &owner, &bound, &completion).is_err());
    std::fs::write(path, b"altered receipt").unwrap();
    assert!(verify(root.path(), &owner, &bound, &completion).is_err());
}

#[test]
fn published_text_edit_receipt_is_verified_without_resending_input() {

    let bytes = include_bytes!("../tests/fixtures/uikit-text-receipt.json");
    let raw: UiReceipt<TextEdit> = serde_json::from_slice(bytes).unwrap();
    let TextEdit::UikitTextControl { app, reference, observation, now_ns, receipt } = raw.result else {
        panic!("expected source text delivery");
    };
    let simulator = "abcd0000-0000-0000-0000-000000000001".to_string();
    let owner = Owner {
        runner: UiIdentity { simulator: simulator.clone(), generation: raw.generation.clone(), pid: raw.pid },
        app: ClockIdentity { simulator, generation: app.generation, pid: app.pid },
        controller_id: "abcd0000-0000-0000-0000-000000000004".into(),
        source_sha256: "a".repeat(64),
    };
    let digest = sha256_bytes_hex(bytes);
    let root = tempfile::tempdir().unwrap();
    let receipts = root.path().join("ui-client/receipts");
    std::fs::create_dir_all(&receipts).unwrap();
    std::fs::write(receipts.join(format!("{digest}.json")), bytes).unwrap();
    let request = Request::Ui(raw.request.clone());
    let mut completion = Completion::Ui(Box::new(UiCompletion {
        request: raw.request, generation: raw.generation, pid: raw.pid, last_id: raw.last_id,
        receipt_sha256: digest,
        outcome: UiOutcome::TextEdit {
            app: owner.app.clone(), reference, observation, now_ns,
            delivery: frontcap_core::native::ios_edit::Delivery::UikitTextControl { receipt },
        },
    }));
    verify(root.path(), &owner, &request, &completion).unwrap();
    if let Completion::Ui(c) = &mut completion
        && let UiOutcome::TextEdit { now_ns, .. } = &mut c.outcome { *now_ns += 1; }
    assert!(verify(root.path(), &owner, &request, &completion).is_err());
    let mut extra: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    extra["result"]["unexpected"] = true.into();
    assert!(serde_json::from_value::<UiReceipt<TextEdit>>(extra).is_err());
}
