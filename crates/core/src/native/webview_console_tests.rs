use super::*;
#[test]
fn console_history_preserves_loss_and_refuses_inconsistent_evidence() {
    let mut history = History { document: "abcd0000-0000-0000-0000-000000000001".into(),
        received: 4, dropped: 3, failure: None, entries: vec![Entry { sequence: 4,
            elapsed_ms: 12.0, level: Level::Log, text: "entity accepted".into(), truncated: false }] };
    assert!(history.valid());
    assert_eq!(serde_json::from_str::<History>(&serde_json::to_string(&history).unwrap()).unwrap(),history);
    history.dropped=0;assert!(!history.valid());history.dropped=3;
    history.entries[0].text="😀".repeat(513);assert!(!history.valid());
}
