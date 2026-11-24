use super::*;

#[test]
fn accepts_managed_identifiers() {
    for ok in ["a", "run-1", "g-0123456789abcdef", "default-block", "x1-y2-z3"] {
        assert!(validate(ok).is_ok(), "{ok}");
    }
}

#[test]
fn rejects_everything_else() {
    for bad in ["", "-a", "a-", "a--b", "A", "a_b", "a.b", "a/b", "../x", "a b", "é"] {
        assert!(validate(bad).is_err(), "{bad:?}");
    }
    assert!(validate(&"a".repeat(129)).is_err());
}

#[test]
fn serde_rejects_invalid_ids() {
    assert!(serde_json::from_str::<RunId>("\"Bad_Id\"").is_err());
    let id: RunId = serde_json::from_str("\"run-7\"").unwrap();
    assert_eq!(serde_json::to_string(&id).unwrap(), "\"run-7\"");
}

#[test]
fn generated_launch_ids_are_valid_and_distinct() {
    let a = LaunchId::generate();
    let b = LaunchId::generate();
    assert!(validate(a.as_str()).is_ok());
    assert!(a.as_str().starts_with("g-") && a.as_str().len() == 18);
    assert_ne!(a, b);
}
