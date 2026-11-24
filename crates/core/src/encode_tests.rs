use super::*;

#[test]
fn base64_matches_rfc4648() {
    assert_eq!(base64(b""), "");
    assert_eq!(base64(b"{}"), "e30=");
    assert_eq!(base64(b"hello"), "aGVsbG8=");
    assert_eq!(base64(b"hel"), "aGVs");
}

#[test]
fn base64_decodes_what_it_encodes() {
    for bytes in [&b""[..], b"{}", b"hello", b"hel", &[0u8, 255, 7, 128][..]] {
        assert_eq!(unbase64(&base64(bytes)).unwrap(), bytes);
    }
    assert!(unbase64("a$==").is_none());
    assert!(unbase64("A").is_none(), "a dangling character");
    assert!(unbase64("e3=0").is_none(), "padding inside");
}
