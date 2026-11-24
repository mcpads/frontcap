use super::*;
#[test]
fn document_history_preserves_lineage_and_reports_bounded_loss() {
    let entry=|sequence| Entry { sequence, page:"550e8400-e29b-41d4-a716-446655440000".into(),
        document:format!("550e8400-e29b-41d4-a716-{sequence:012}"),
        url:format!("https://example.invalid/{sequence}"),url_truncated:false };
    let old=History{received:2,dropped:0,failure:None,entries:vec![entry(1),entry(2)]};
    assert!(old.valid());
    let current=History{received:34,dropped:2,failure:None,entries:(3..=34).map(entry).collect()};
    assert!(current.follows(&old));
    assert_eq!(serde_json::from_str::<History>(&serde_json::to_string(&current).unwrap()).unwrap(),current);
    let mut rewritten=old.clone();rewritten.entries[1].document=entry(3).document;
    assert!(!rewritten.follows(&old));
    let mut invalid=current.clone();invalid.dropped=1;assert!(!invalid.valid());
    invalid=current.clone();invalid.entries[0].sequence=2;assert!(!invalid.valid());
    invalid=current.clone();invalid.entries[0].url="界".repeat(URL_LIMIT);assert!(!invalid.valid());
    invalid=current.clone();invalid.entries[0].page="foreign".into();assert!(!invalid.valid());
    let mut failed=old.clone();failed.failure=Some("unavailable".into());assert!(failed.follows(&old));
    assert!(!old.follows(&failed));assert!(!old.follows(&current));
}
