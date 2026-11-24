use super::*;
#[test]
fn admission_requires_retained_completion_and_same_document() {
    let doc = "abcd0000-0000-0000-0000-000000000001".to_owned();
    let mut receipt = Receipt {
        document: doc.clone(),
        legacy_used: false,
        last: None,
    };
    let mut ledger = Ledger::bind(&receipt).unwrap();
    let first = Request {
        document: doc,
        id: 1,
        timestamp: 10.0,
    };
    ledger.admit(&first).unwrap();
    assert_eq!(ledger.inspect().pending, Some(first.clone()));
    assert!(ledger.inspect().completed.is_none());
    let next = Request {
        id: 2,
        timestamp: 20.0,
        ..first.clone()
    };
    assert!(ledger.admit(&next).is_err());
    receipt.last = Some(Outcome {
        request: first.clone(),
        state: State::Pending,
        result: None,
        error: None,
    });
    ledger.observe(&receipt).unwrap();
    assert!(ledger.admit(&next).is_err());
    let last = receipt.last.as_mut().unwrap();
    last.state = State::Completed;
    last.result = Some(serde_json::json!({"timestamp":10.0}));
    ledger.observe(&receipt).unwrap();
    assert!(ledger.admit(&first).is_err());
    assert_eq!(ledger.inspect().completed, Some(first.clone()));
    assert!(ledger.inspect().pending.is_none());
    ledger.admit(&next).unwrap();
    ledger.validate().unwrap();
    let mut corrupted=serde_json::to_value(&ledger).unwrap();
    corrupted["pending"]["id"]=4.into();
    assert!(serde_json::from_value::<Ledger>(corrupted).unwrap().validate().is_err());
    assert!(ledger.observe(&receipt).is_err());
    receipt.document = "abcd0000-0000-0000-0000-000000000002".into();
    assert!(ledger.observe(&receipt).is_err());
    assert!(Ledger::bind(&receipt).is_err());
}
