use super::*;
use std::io::Write;

fn id(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}

#[test]
fn new_request_then_replay_returns_recorded_result() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let args = args_sha256(&serde_json::json!({ "action": "click", "target": "e1" }));
    let mut j = Journal::open(&path).unwrap();
    assert_eq!(j.begin(&id("c-1"), &args).unwrap(), Begin::New);
    j.dispatched(&id("c-1")).unwrap();
    j.complete(&id("c-1"), serde_json::json!({ "ok": true })).unwrap();
    drop(j);
    let mut j = Journal::open(&path).unwrap();
    assert_eq!(j.begin(&id("c-1"), &args).unwrap(), Begin::Completed(serde_json::json!({ "ok": true })));
    assert_eq!(j.last_completed(), Some(&id("c-1")));
}

#[test]
fn intent_only_is_not_applied() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut j = Journal::open(&path).unwrap();
    j.begin(&id("c-1"), "h").unwrap();
    drop(j);
    let mut j = Journal::open(&path).unwrap();
    assert_eq!(j.begin(&id("c-1"), "h").unwrap(), Begin::NotApplied);
}

#[test]
fn dispatched_without_completion_is_unresolved_until_resolved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut j = Journal::open(&path).unwrap();
    j.begin(&id("c-1"), "h").unwrap();
    j.dispatched(&id("c-1")).unwrap();
    drop(j);
    let mut j = Journal::open(&path).unwrap();
    assert_eq!(j.unresolved(), vec![id("c-1")]);
    assert_eq!(j.begin(&id("c-1"), "h").unwrap(), Begin::Unresolved);
    j.complete(&id("c-1"), serde_json::json!({ "recovered": true })).unwrap();
    assert!(j.unresolved().is_empty());
}

#[test]
fn reuse_with_different_arguments_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let mut j = Journal::open(&dir.path().join("j.jsonl")).unwrap();
    j.begin(&id("c-1"), "h1").unwrap();
    assert_eq!(j.begin(&id("c-1"), "h2").unwrap(), Begin::Conflict);
}

#[test]
fn steps_out_of_order_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let mut j = Journal::open(&dir.path().join("j.jsonl")).unwrap();
    assert!(matches!(j.dispatched(&id("nope")), Err(JournalError::UnknownRequest(_))));
    j.begin(&id("c-1"), "h").unwrap();
    j.dispatched(&id("c-1")).unwrap();
    j.complete(&id("c-1"), serde_json::json!(null)).unwrap();
    assert!(matches!(j.dispatched(&id("c-1")), Err(JournalError::WrongStep { .. })));
}

#[test]
fn partial_journal_line_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut j = Journal::open(&path).unwrap();
    j.begin(&id("c-1"), "h").unwrap();
    drop(j);
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"{\"request_id\":\"c-1\",\"st").unwrap();
    drop(f);
    let mut j = Journal::open(&path).unwrap();
    assert_eq!(j.begin(&id("c-1"), "h").unwrap(), Begin::NotApplied);
}

#[test]
fn cut_inside_a_multibyte_character_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut j = Journal::open(&path).unwrap();
    j.begin(&id("c-1"), "h").unwrap();
    j.dispatched(&id("c-1")).unwrap();
    drop(j);
    // A crash in the middle of writing `"한글"` leaves half a UTF-8 sequence.
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all("{\"request_id\":\"c-1\",\"result\":\"한".as_bytes()).unwrap();
    f.write_all(&"글".as_bytes()[..1]).unwrap();
    drop(f);
    let mut j = Journal::open(&path).unwrap();
    assert_eq!(j.begin(&id("c-1"), "h").unwrap(), Begin::Unresolved);
}

#[test]
fn unparseable_complete_line_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    std::fs::write(&path, "{\"step\":\"intent\",\"request_id\":\"c-1\",\"args_sha256\":\"h\"}\nnot json\n").unwrap();
    assert!(Journal::open(&path).is_err());
}

#[test]
fn step_for_unknown_request_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    std::fs::write(&path, "{\"request_id\":\"ghost\",\"step\":\"dispatched\"}\n").unwrap();
    assert!(Journal::open(&path).is_err());
}

#[test]
fn reading_request_evidence_never_creates_or_resolves_a_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = Journal::open(&path).unwrap();
    let request = id("inspected");
    assert_eq!(journal.state(&request), None);
    assert_eq!(journal.begin(&request, "args").unwrap(), Begin::New);
    assert_eq!(journal.state(&request), Some(&RequestState::Intent));
    journal.dispatched(&request).unwrap();
    assert_eq!(journal.state(&request), Some(&RequestState::Dispatched));
    let before = std::fs::read(&path).unwrap();
    assert_eq!(journal.state(&id("unknown")), None);
    assert_eq!(journal.state(&request), Some(&RequestState::Dispatched));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let answer = serde_json::json!({"error":"page_unresponsive","effects":["input_dispatched"]});
    journal.complete(&request, answer.clone()).unwrap();
    drop(journal);
    let journal = Journal::open(&path).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert_eq!(journal.state(&request), Some(&RequestState::Completed(answer)));
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[test]
fn read_only_evidence_preserves_partial_tail_and_reports_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let mut journal = Journal::open(&path).unwrap();
    journal.begin(&id("done"), "args").unwrap();
    let answer = serde_json::json!({"answer":"err", "body":{"message":"saved failure"}});
    journal.complete(&id("done"), answer.clone()).unwrap();
    journal.begin(&id("pending"), "args").unwrap();
    journal.dispatched(&id("pending")).unwrap();
    drop(journal);
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"{\"step\":").unwrap();
    let before = std::fs::read(&path).unwrap();
    assert_eq!(Journal::read_state(&path, &id("done")).unwrap(), Some(RequestState::Completed(answer)));
    assert_eq!(Journal::read_state(&path, &id("pending")).unwrap(), Some(RequestState::Dispatched));
    assert_eq!(Journal::read_state(&path, &id("unknown")).unwrap(), None);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"\n").unwrap();
    assert!(matches!(Journal::read_state(&path, &id("done")), Err(JournalError::Corrupt { .. })));
    let missing = dir.path().join("missing.jsonl");
    assert!(Journal::read_state(&missing, &id("done")).is_err());
    assert!(!missing.exists());
}

#[test]
fn dependent_answers_keep_admission_order_and_gate_parent_completion_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.jsonl");
    let parent = id("advance");
    let first = id("z-answer-first");
    let second = id("a-answer-second");
    let mut j = Journal::open(&path).unwrap();
    j.begin(&parent, "advance150ms").unwrap();
    assert!(j.begin_dependent(&parent, &first, "accept").is_err());
    j.dispatched(&parent).unwrap();
    assert_eq!(j.begin_dependent(&parent, &first, "accept").unwrap(), Begin::New);
    j.dispatched(&first).unwrap();
    assert_eq!(j.begin_dependent(&parent, &second, "reject").unwrap(), Begin::New);
    j.dispatched(&second).unwrap();
    // Completion order differs from admission order. Recovery must retain both.
    j.complete(&second, serde_json::json!({"accepted":false})).unwrap();
    drop(j);
    let mut j = Journal::open(&path).unwrap();
    assert_eq!(j.dependents(&parent), [first.clone(), second.clone()]);
    assert_eq!(j.parent(&first), Some(&parent));
    assert_eq!(j.pending_dependents(&parent), std::slice::from_ref(&first));
    let before = std::fs::read(&path).unwrap();
    assert!(j.complete(&parent, serde_json::json!({"reached":true})).is_err());
    assert!(j.resolve_unapplied(&parent).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    j.complete(&first, serde_json::json!({"accepted":true})).unwrap();
    j.complete(&parent, serde_json::json!({"reached":true})).unwrap();
    drop(j);
    let mut j = Journal::open(&path).unwrap();
    assert!(matches!(j.begin_dependent(&parent, &first, "accept").unwrap(), Begin::Completed(_)));
    assert!(j.begin_dependent(&parent, &id("late"), "accept").is_err());
    assert_eq!(j.last_completed(), Some(&parent));
}

#[test]
fn dependent_identity_cannot_change_parent_arguments_or_request_role() {
    let dir = tempfile::tempdir().unwrap();
    let mut j = Journal::open(&dir.path().join("j.jsonl")).unwrap();
    for parent in [id("p1"), id("p2")] {
        j.begin(&parent, "args").unwrap();
        j.dispatched(&parent).unwrap();
    }
    let child = id("answer");
    j.begin_dependent(&id("p1"), &child, "yes").unwrap();
    assert_eq!(j.begin_dependent(&id("p1"), &child, "yes").unwrap(), Begin::NotApplied);
    assert_eq!(j.begin_dependent(&id("p2"), &child, "yes").unwrap(), Begin::Conflict);
    assert_eq!(j.begin_dependent(&id("p1"), &child, "no").unwrap(), Begin::Conflict);
    assert_eq!(j.begin(&child, "yes").unwrap(), Begin::Conflict);
    assert_eq!(j.begin_dependent(&id("p2"), &id("p1"), "args").unwrap(), Begin::Conflict);
    j.dispatched(&child).unwrap();
    assert!(j.begin_dependent(&child, &id("nested"), "args").is_err());
    assert_eq!(j.dependents(&id("p1")), [child]);
    assert!(j.dependents(&id("p2")).is_empty());
}

#[test]
fn partial_dependent_intent_cannot_separate_a_child_from_its_parent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j.jsonl");
    let parent = id("parent");
    let child = id("child");
    let mut j = Journal::open(&path).unwrap();
    j.begin(&parent, "args").unwrap();
    j.dispatched(&parent).unwrap();
    let prefix = std::fs::read(&path).unwrap();
    j.begin_dependent(&parent, &child, "answer").unwrap();
    drop(j);
    let full = std::fs::read(&path).unwrap();
    for cut in prefix.len()..full.len() {
        std::fs::write(&path, &full[..cut]).unwrap();
        let before = std::fs::read(&path).unwrap();
        assert_eq!(Journal::read_state(&path, &child).unwrap(), None);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let j = Journal::open(&path).unwrap();
        assert!(j.dependents(&parent).is_empty());
        assert_eq!(j.state(&child), None);
    }
    std::fs::write(&path, &full).unwrap();
    let mut j = Journal::open(&path).unwrap();
    assert_eq!(j.dependents(&parent), std::slice::from_ref(&child));
    assert_eq!(j.state(&child), Some(&RequestState::Intent));
    assert!(j.complete(&parent, serde_json::json!(null)).is_err());
    // Recovery can record a definite not-applied answer for an intent that was never dispatched.
    j.complete(&child, serde_json::json!({"not_applied":true})).unwrap();
    j.complete(&parent, serde_json::json!({"recovered":true})).unwrap();
}

#[test]
fn reader_rejects_forged_parent_completion_and_impossible_transitions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j.jsonl");
    let mut j = Journal::open(&path).unwrap();
    j.begin(&id("parent"), "args").unwrap();
    j.dispatched(&id("parent")).unwrap();
    j.begin_dependent(&id("parent"), &id("answer"), "yes").unwrap();
    drop(j);
    let prefix = std::fs::read_to_string(&path).unwrap();
    for suffix in [
        r#"{"step":"completed","request_id":"parent","result":null}"#,
        r#"{"step":"resolved_not_applied","request_id":"parent"}"#,
        r#"{"step":"intent","request_id":"answer","args_sha256":"replacement"}"#,
        r#"{"step":"dependent_intent","parent":"answer","request_id":"nested","args_sha256":"yes"}"#,
        r#"{"step":"dispatched","request_id":"parent"}"#,
    ] {
        std::fs::write(&path, format!("{prefix}{suffix}\n")).unwrap();
        assert!(matches!(Journal::read_state(&path, &id("parent")), Err(JournalError::Corrupt { .. })), "{suffix}");
        assert!(matches!(Journal::open(&path), Err(JournalError::Corrupt { .. })), "{suffix}");
    }
}
