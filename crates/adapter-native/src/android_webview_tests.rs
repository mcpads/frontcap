use super::*;
#[test]
fn discovery_preserves_candidates_but_rejects_foreign_or_ambiguous_endpoints() {
    let page = |id: &str, socket: &str| serde_json::json!({"id":id,"type":"page","url":"about:blank","title":"Host","webSocketDebuggerUrl":socket});
    let a = page("one", "ws://127.0.0.1:1234/devtools/page/one");
    let b = page("two", "ws://127.0.0.1:1234/devtools/page/two");
    assert_eq!(
        pages(&serde_json::to_vec(&[&a, &b]).unwrap(), 1234)
            .unwrap()
            .len(),
        2
    );
    for socket in [
        "ws://example.com:1234/devtools/page/one",
        "ws://127.0.0.1:9999/devtools/page/one",
        "ws://127.0.0.1:1234/devtools/page/two",
        "ws://127.0.0.1:1234/devtools/page/one?x=1",
    ] {
        assert!(pages(&serde_json::to_vec(&[page("one", socket)]).unwrap(), 1234).is_err());
    }
    assert!(pages(&serde_json::to_vec(&[&a, &a]).unwrap(), 1234).is_err());
    assert!(pages(b"not JSON", 1234).is_err());
}
