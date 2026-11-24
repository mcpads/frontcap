use super::*;
use crate::lines;
use crate::event::*;
use crate::ids::WatchName;
use crate::time::VirtualTime;
use std::io::Write;

fn sample(name: &str) -> EventKind {
    EventKind::Watch(WatchSample { name: WatchName::new(name).unwrap(), value: SampleValue::False, point: PointKind::Initial, rseq: 0, document: 0, context: None })
}

#[test]
fn appends_with_increasing_seq_and_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    let mut log = BehaviorLog::open(&path).unwrap();
    assert_eq!(log.append(VirtualTime(0), sample("a")).unwrap(), 0);
    assert_eq!(log.append(VirtualTime(5), sample("b")).unwrap(), 1);
    log.sync().unwrap();
    drop(log);
    let mut log = BehaviorLog::open(&path).unwrap();
    assert_eq!(log.next_seq(), 2);
    assert_eq!(log.last_time(), VirtualTime(5));
    assert_eq!(log.append(VirtualTime(5), sample("c")).unwrap(), 2);
    let events = BehaviorLog::read(&path).unwrap();
    assert_eq!(events.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![0, 1, 2]);
}

#[test]
fn lines_are_canonical_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    let mut log = BehaviorLog::open(&path).unwrap();
    log.append(VirtualTime(1), sample("a")).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text, "{\"kind\":\"watch\",\"payload\":{\"name\":\"a\",\"point\":\"initial\",\"value\":\"false\"},\"seq\":0,\"t_us\":1}\n");
}

#[test]
fn rejects_time_going_backwards() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = BehaviorLog::open(&dir.path().join("t.jsonl")).unwrap();
    log.append(VirtualTime(10), sample("a")).unwrap();
    assert!(matches!(log.append(VirtualTime(9), sample("b")), Err(LogError::TimeWentBackwards { .. })));
}

#[test]
fn truncated_tail_is_moved_aside_and_seq_continues() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    let mut log = BehaviorLog::open(&path).unwrap();
    log.append(VirtualTime(0), sample("a")).unwrap();
    log.append(VirtualTime(1), sample("b")).unwrap();
    drop(log);
    // A crash mid-write leaves a partial line.
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"{\"kind\":\"watch\",\"payl").unwrap();
    drop(f);
    let mut log = BehaviorLog::open(&path).unwrap();
    let aside = log.recovered_tail().expect("partial tail recorded").to_path_buf();
    assert_eq!(std::fs::read_to_string(&aside).unwrap(), "{\"kind\":\"watch\",\"payl");
    assert_eq!(log.append(VirtualTime(2), sample("c")).unwrap(), 2);
    assert_eq!(BehaviorLog::read(&path).unwrap().len(), 3);
}

#[test]
fn acquisition_log_keeps_its_own_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("evidence.jsonl");
    let mut log = AcquisitionLog::open(&path).unwrap();
    let kind = AcquisitionKind::Observation(ObservationRecord { call: "query".into(), summary: "1".into() });
    assert_eq!(log.append(4, VirtualTime(9), kind.clone()).unwrap(), 0);
    assert_eq!(log.append(4, VirtualTime(9), kind).unwrap(), 1);
    let read = AcquisitionLog::read(&path).unwrap();
    assert_eq!(read[1].after_seq, 4);
}

#[test]
fn trace_cut_inside_a_multibyte_character_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    let mut log = BehaviorLog::open(&path).unwrap();
    log.append(VirtualTime(0), sample("a")).unwrap();
    drop(log);
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(&"{\"text\":\"한".as_bytes()[..10]).unwrap();
    drop(f);
    let mut log = BehaviorLog::open(&path).unwrap();
    assert!(log.recovered_tail().is_some());
    assert_eq!(log.append(VirtualTime(1), sample("b")).unwrap(), 1);
}

#[test]
fn read_ignores_a_partial_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    let mut log = BehaviorLog::open(&path).unwrap();
    log.append(VirtualTime(0), sample("a")).unwrap();
    drop(log);
    let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    f.write_all(b"{\"kind\":").unwrap();
    drop(f);
    assert_eq!(BehaviorLog::read(&path).unwrap().len(), 1);
}

#[test]
fn corrupt_complete_line_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    std::fs::write(&path, b"\xff\xfe\n").unwrap();
    assert!(BehaviorLog::open(&path).is_err());
}

#[test]
fn failed_write_is_rolled_back_and_poisons_the_handle() {
    let mut appender = lines::Appender::new(lines::tests_support::FailingSink::new(5), 0);
    assert!(appender.append_line("0123456789").is_err());
    assert_eq!(appender.len(), 0);
    assert!(appender.append_line("ok").is_err(), "a poisoned handle refuses further writes");
}

#[test]
fn a_tail_returns_what_a_whole_read_returns_as_the_log_grows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    let mut log = BehaviorLog::open(&path).unwrap();
    let mut tail = BehaviorTail::new(&path);
    assert!(tail.events().unwrap().is_empty());
    for (i, name) in ["a", "b", "c"].into_iter().enumerate() {
        log.append(VirtualTime(i as u64), sample(name)).unwrap();
        assert_eq!(tail.events().unwrap(), BehaviorLog::read(&path).unwrap());
    }
    assert_eq!(tail.events().unwrap().len(), 3);
}

#[test]
fn a_tail_leaves_a_partial_line_for_the_read_that_finds_it_complete() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    let mut log = BehaviorLog::open(&path).unwrap();
    log.append(VirtualTime(0), sample("a")).unwrap();
    let whole = canonical_of(&BehaviorEvent { seq: 1, t_us: VirtualTime(1), kind: sample("b") });
    let (head, rest) = whole.split_at(whole.len() / 2);
    let mut raw = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    raw.write_all(head.as_bytes()).unwrap();
    let mut tail = BehaviorTail::new(&path);
    assert_eq!(tail.events().unwrap().len(), 1);
    raw.write_all(rest.as_bytes()).unwrap();
    assert_eq!(tail.events().unwrap().len(), 1);
    raw.write_all(b"\n").unwrap();
    assert_eq!(tail.events().unwrap(), BehaviorLog::read(&path).unwrap());
    assert_eq!(tail.events().unwrap().len(), 2);
}

#[test]
fn a_tail_starts_over_when_the_file_is_not_the_one_it_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    let mut log = BehaviorLog::open(&path).unwrap();
    log.append(VirtualTime(0), sample("a")).unwrap();
    log.append(VirtualTime(1), sample("b")).unwrap();
    let mut tail = BehaviorTail::new(&path);
    assert_eq!(tail.events().unwrap().len(), 2);
    drop(log);
    // A shorter file at the same path.
    std::fs::remove_file(&path).unwrap();
    let mut log = BehaviorLog::open(&path).unwrap();
    log.append(VirtualTime(7), sample("x")).unwrap();
    assert_eq!(tail.events().unwrap(), BehaviorLog::read(&path).unwrap());
    drop(log);
    // A longer file at the same path, written beside it and renamed over it.
    let other = dir.path().join("other.jsonl");
    let mut log = BehaviorLog::open(&other).unwrap();
    for name in ["p", "q", "r"] { log.append(VirtualTime(9), sample(name)).unwrap(); }
    drop(log);
    std::fs::rename(&other, &path).unwrap();
    assert_eq!(tail.events().unwrap(), BehaviorLog::read(&path).unwrap());
    assert_eq!(tail.events().unwrap().len(), 3);
}

#[test]
fn a_tail_reports_a_line_it_cannot_parse_by_its_number_in_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    let mut log = BehaviorLog::open(&path).unwrap();
    log.append(VirtualTime(0), sample("a")).unwrap();
    let mut tail = BehaviorTail::new(&path);
    assert_eq!(tail.events().unwrap().len(), 1);
    std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"not json\n").unwrap();
    assert!(matches!(tail.events(), Err(LogError::Parse { line: 2, .. })));
    assert!(matches!(BehaviorLog::read(&path), Err(LogError::Parse { line: 2, .. })));
}

#[test]
fn readers_of_one_log_share_its_tail_while_one_holds_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("trace.jsonl");
    let mut log = BehaviorLog::open(&path).unwrap();
    log.append(VirtualTime(0), sample("a")).unwrap();
    let first = BehaviorTail::shared(&path);
    let second = BehaviorTail::shared(&path);
    assert!(std::sync::Arc::ptr_eq(&first, &second));
    assert!(!std::sync::Arc::ptr_eq(&first, &BehaviorTail::shared(&dir.path().join("other.jsonl"))));
    assert_eq!(first.lock().unwrap().events().unwrap().len(), 1);
    let released = std::sync::Arc::downgrade(&first);
    drop((first, second));
    assert!(released.upgrade().is_none());
    assert_eq!(BehaviorTail::shared(&path).lock().unwrap().events().unwrap().len(), 1);
}
