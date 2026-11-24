use super::*;

#[test]
fn cassette_handshake_repairs_torn_tail_and_rejects_conflicting_outcomes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("feed.jsonl");
    let line = frontcap_core::cassette::StreamEvent {
        key: frontcap_core::cassette::Key::of("GET", "https://example.test/feed", None),
        session: 0, at_us: 0, event: "open".into(), data: None,
        sse_event: None, sse_id: None, code: None,
    };
    std::fs::write(&path, b"{\"stream\":").unwrap();
    persist_stream_handshake(&path, &line).unwrap();
    persist_stream_handshake(&path, &line).unwrap();
    let mut conflicting = line.clone();
    conflicting.event = "error".into();
    assert!(persist_stream_handshake(&path, &conflicting).is_err());
    let (_, stored) = frontcap_core::cassette::parse_all(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(stored, vec![line]);
}
