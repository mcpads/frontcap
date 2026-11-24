use super::*;

#[test]
fn native_bindings_reopen_with_stable_ordinals_and_reject_conflicts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dialog-bindings.jsonl");
    let mut bindings = DialogBindings::open(&path).unwrap();
    assert_eq!(bindings.bind("engine-a").unwrap(), "dialog-1");
    assert_eq!(bindings.bind("engine-b").unwrap(), "dialog-2");
    drop(bindings);
    let before = std::fs::read(&path).unwrap();
    let mut bindings = DialogBindings::open(&path).unwrap();
    assert_eq!(bindings.bind("engine-a").unwrap(), "dialog-1");
    assert_eq!(bindings.native_id("dialog-2").unwrap(), "engine-b");
    assert!(bindings.native_id("dialog-3").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(bindings.bind("engine-c").unwrap(), "dialog-3");
    drop(bindings);
    use std::io::Write;
    std::fs::OpenOptions::new().append(true).open(&path).unwrap()
        .write_all(b"{\"id\":\"dialog-4\",\"native_id\":\"engine-a\"}\n").unwrap();
    assert!(DialogBindings::open(&path).is_err(), "one engine dialog cannot acquire two ordinals");
}
