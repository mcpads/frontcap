use super::*;
use frontcap_core::native::ios_ui::{UiCompletion, UiOperation, UiRequest};
#[test]
fn detached_read_requires_original_completed_answer_and_exact_png_bytes() {
    let request = RequestId::new("observe").unwrap();
    let outcome = UiOutcome::SystemButtonTapped { reference: "button".into(), observation: "capture".into() };
    let completion = UiCompletion { request: UiRequest { generation: "runner".into(), request_id: 2,
        operation: UiOperation::ObserveSystemAlerts {} }, generation: "runner".into(), pid: 10,
        last_id: 2, outcome: outcome.clone(), receipt_sha256: "a".repeat(64) };
    let answer = serde_json::to_value(Answer::Ok(Reply::Native { body: NativeReply::Done {
        request_id: request.clone(), outcome: Box::new(Outcome::IosCompleted { completion: IosCompletion::Ui(Box::new(completion)) }) } })).unwrap();
    validate_answer(answer.clone(), &request, &outcome).unwrap();
    assert!(validate_answer(answer.clone(), &RequestId::new("other").unwrap(), &outcome).is_err());
    assert!(validate_answer(answer, &request, &UiOutcome::SystemButtonTapped { reference: "changed".into(), observation: "capture".into() }).is_err());
    let root = tempfile::tempdir().unwrap(); let path = root.path().join("artifact");
    let bytes = b"\x89PNG\r\n\x1a\noriginal"; fs::write(&path, bytes).unwrap();
    let artifact = Artifact { path: "screen.png".into(), bytes: bytes.len() as u64,
        sha256: frontcap_core::canonical::sha256_bytes_hex(bytes) };
    assert_eq!(read_png(&path, &artifact).unwrap(), bytes);
    fs::write(&path, vec![0; bytes.len()]).unwrap(); assert!(read_png(&path, &artifact).is_err());
}
