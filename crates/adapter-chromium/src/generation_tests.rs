use super::*;

fn sample(rseq: u64, document: u64) -> (VirtualTime, EventKind) {
    let name = WatchName::new("w").unwrap();
    (VirtualTime(rseq * 1000), EventKind::Watch(WatchSample { name, value: SampleValue::True, point: PointKind::Frame, rseq, document, context: None }))
}

#[test]
fn the_observer_cursor_moves_only_over_samples_written() {
    assert_eq!(written_cursor(3, &[sample(4, 2), sample(7, 2)], 2), 7);
    assert_eq!(written_cursor(3, &[], 2), 3, "samples still buffered are not acknowledged");
}

#[test]
fn another_documents_samples_do_not_move_the_cursor() {
    assert_eq!(written_cursor(3, &[sample(90, 1)], 2), 3);
}

#[test]
fn dialog_reports_resume_from_the_durable_document_cursor() {
    let dialog = EventKind::Dialog(DialogRecord {
        kind: DialogKind::Confirm, message: "continue?".into(), answer: "true".into(),
        native: Some(NativeDialogEvidence { id: "dialog-1".into(), source: NativeDialogSource::Answered }),
        observer: Some(DialogObserver { document: 2, rseq: 8 }),
        context: None,
    });
    assert_eq!(written_cursor(3, &[(VirtualTime(100), dialog.clone())], 2), 8);
    assert_eq!(written_cursor(3, &[(VirtualTime(100), dialog.clone())], 3), 3);
}
