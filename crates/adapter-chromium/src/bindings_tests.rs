use super::*;
use frontcap_core::event::ResponseSource;
use frontcap_core::rules::{FailReason, MockSpec};
use std::io::Write;

fn spec(body: &str, body_base64: Option<&str>) -> MockSpec {
    MockSpec { status: 200, headers: Default::default(), body: body.into(), body_base64: body_base64.map(str::to_string), latency_ms: 500 }
}

fn bound(ordinal: &str, decision: Decision) -> BindingEntry {
    BindingEntry::Bound(Box::new(Binding {
        blocks_main_thread: false,
        frame_id: None, recording: None, continuation: None, selection_taint: None,
        rules_revision: 7,
        ordinal: ordinal.into(),
        target_id: "target".into(),
        fetch_id: format!("interception-job-{ordinal}.0"),
        network_id: Some("1.2".into()),
        worker_lineage: Some(WorkerLineage { creator_document: "document-token".into(), constructors: vec![2, 1] }),
        document: 1,
        issued_us: 100_000,
        issued_during_progress: false,
        rule: RuleId::new("slow").unwrap(),
        entry: None,
        body: None,
        method: "GET".into(),
        url: "https://api.example.test/slow".into(),
        origin: None,
        decision,
        document_request: false,
        navigation: None,
    }))
}

#[test]
fn every_entry_round_trips_through_the_journal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("interceptions.jsonl");
    let entries = vec![
        BindingEntry::WorkerRequestOwner { network_id: "1.2".into(), issued_us: 100_000, lineage: WorkerLineage {
            creator_document: "document-token".into(), constructors: vec![2, 1],
        }, document: Some(DocumentPlace::Current { window: 1, frames: vec![0, 2] }) },
        BindingEntry::WorkerRequestOwner { network_id: "1.3".into(), issued_us: 100_000, lineage: WorkerLineage {
            creator_document: "retired-token".into(), constructors: vec![1],
        }, document: Some(DocumentPlace::Retired) },
        bound("req-1", Decision::Deliver { spec: spec("slow", None), source: ResponseSource::Mock, due_us: 600_000, requires_progress: true }),
        bound("req-2", Decision::Hold),
        bound("req-3", Decision::Passthrough),
        bound("req-4", Decision::Fail { reason: FailReason::Aborted }),
        BindingEntry::Dispatched { ordinal: "req-1".into(), fetch_id: "interception-job-req-1.0".into(), resolution: Resolution::Fulfill { status: 200, body_sha256: body_sha256(&spec("slow", None)) } },
        BindingEntry::Dispatched { ordinal: "req-4".into(), fetch_id: "interception-job-req-4.0".into(), resolution: Resolution::Fail { reason: FailReason::Aborted } },
        BindingEntry::Dispatched { ordinal: "req-3".into(), fetch_id: "interception-job-req-3.0".into(), resolution: Resolution::Continue },
    ];
    let mut journal = BindingJournal::open(&path).unwrap();
    for e in &entries {
        journal.append(e).unwrap();
    }
    assert_eq!(BindingJournal::read(&path).unwrap(), entries);
}

#[test]
fn a_line_cut_by_a_killed_writer_is_ignored_and_moved_aside_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("interceptions.jsonl");
    let whole = bound("req-1", Decision::Hold);
    BindingJournal::open(&path).unwrap().append(&whole).unwrap();
    let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(br#"{"kind":"bound","ordinal":"req-2","tar"#).unwrap();
    assert_eq!(BindingJournal::read(&path).unwrap(), vec![whole.clone()]);
    // Reopening moves the cut line aside, so later entries start a fresh line and stay readable.
    let mut reopened = BindingJournal::open(&path).unwrap();
    let aside = reopened.recovered_tail().expect("the cut line is kept aside").to_path_buf();
    assert_eq!(std::fs::read(&aside).unwrap(), br#"{"kind":"bound","ordinal":"req-2","tar"#);
    let next = bound("req-3", Decision::Hold);
    reopened.append(&next).unwrap();
    assert_eq!(BindingJournal::read(&path).unwrap(), vec![whole, next]);
}

#[test]
fn a_corrupt_line_before_the_end_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("interceptions.jsonl");
    std::fs::write(&path, "not json\n").unwrap();
    BindingJournal::open(&path).unwrap().append(&bound("req-1", Decision::Hold)).unwrap();
    assert!(BindingJournal::read(&path).is_err());
}

#[test]
fn a_missing_journal_has_no_entries() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(BindingJournal::read(&dir.path().join("absent.jsonl")).unwrap(), vec![]);
}

#[test]
fn the_body_digest_is_of_the_bytes_the_engine_receives() {
    assert_eq!(body_sha256(&spec("slow", None)), frontcap_core::canonical::sha256_bytes_hex(b"slow"));
    // `body_base64` wins over `body`, as `fulfill` sends it.
    assert_eq!(body_sha256(&spec("ignored", Some("AAEC"))), frontcap_core::canonical::sha256_bytes_hex(&[0, 1, 2]));
}

#[test]
fn recorded_response_publication_is_idempotent_without_merging_identical_responses() {
    let dir = tempfile::tempdir().unwrap();
    let first = RecordedResponse {
        ordinal: "req-1".into(), cassette: "api".into(), offset: 0,
        prefix_sha256: frontcap_core::canonical::sha256_bytes_hex(b""),
        line: frontcap_core::cassette::Line {
            key: frontcap_core::cassette::Key::of("GET", "https://example.test/api", None),
            response: frontcap_core::cassette::Response { status: 200, headers: Default::default(), body_base64: "b2s=".into() },
            latency: frontcap_core::cassette::Latency { virtual_ms: 0, wall_ms: 0 }, requires_progress: false,
        },
    };
    first.publish(dir.path()).unwrap();
    first.publish(dir.path()).unwrap();
    let path = dir.path().join("cassettes/api.jsonl");
    let prefix = std::fs::read(&path).unwrap();
    let second = RecordedResponse { ordinal: "req-2".into(), offset: prefix.len(),
        prefix_sha256: frontcap_core::canonical::sha256_bytes_hex(&prefix), ..first.clone() };
    second.publish(dir.path()).unwrap();
    first.publish(dir.path()).unwrap();
    second.publish(dir.path()).unwrap();
    let lines = frontcap_core::cassette::parse(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(lines, vec![first.line.clone(), first.line]);
    std::fs::write(&path, "foreign evidence\n").unwrap();
    assert!(second.publish(dir.path()).is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "foreign evidence\n");
}
