use super::*;

#[test]
fn origin_of_urls() {
    assert_eq!(origin_of("http://127.0.0.1:8080/a/b?c").as_deref(), Some("http://127.0.0.1:8080"));
    assert_eq!(origin_of("https://example.test").as_deref(), Some("https://example.test"));
    assert_eq!(origin_of("about:blank"), None);
}
