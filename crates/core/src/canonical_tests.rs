use super::*;

#[test]
fn sorts_keys_at_every_depth() {
    let v = serde_json::json!({ "b": 1, "a": { "d": [ { "z": 1, "y": 2 } ], "c": null } });
    assert_eq!(to_canonical_string(&v), r#"{"a":{"c":null,"d":[{"y":2,"z":1}]},"b":1}"#);
}

#[test]
fn escapes_strings_like_json() {
    let v = serde_json::json!({ "k": "line\n\"quoted\"" });
    assert_eq!(to_canonical_string(&v), r#"{"k":"line\n\"quoted\""}"#);
}

#[test]
fn sha256_of_canonical_text() {
    // sha256("{}")
    assert_eq!(sha256_hex("{}"), "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a");
}
