use super::*;

#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct S {
    next_ordinal: u64,
    rules: Vec<String>,
}

#[test]
fn saves_and_loads() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.json");
    assert_eq!(load::<S>(&path).unwrap(), None);
    let s = S { next_ordinal: 3, rules: vec!["a".into()] };
    save_atomic(&path, &s).unwrap();
    assert_eq!(load::<S>(&path).unwrap(), Some(s));
}

#[test]
fn replaces_without_leaving_temporaries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.json");
    save_atomic(&path, &S { next_ordinal: 1, rules: vec![] }).unwrap();
    save_atomic(&path, &S { next_ordinal: 2, rules: vec![] }).unwrap();
    let names: Vec<String> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(names, vec!["control.json".to_string()]);
    assert_eq!(load::<S>(&path).unwrap().unwrap().next_ordinal, 2);
}

#[test]
fn invalid_content_is_an_error_not_absence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.json");
    std::fs::write(&path, "{not json").unwrap();
    assert_eq!(load::<S>(&path).unwrap_err().kind(), std::io::ErrorKind::InvalidData);
}

/// Concurrent saves of one path each use a temporary file of their own: none fails, and one complete value
/// is left.
#[test]
fn concurrent_saves_leave_one_complete_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
    let outcomes: Vec<std::io::Result<()>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..16)
            .map(|n| {
                let (b, p) = (barrier.clone(), path.clone());
                scope.spawn(move || {
                    b.wait();
                    save_atomic(&p, &serde_json::json!({"n": n, "text": "a".repeat(4096 * n + 1)}))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    assert!(outcomes.iter().all(|r| r.is_ok()), "{outcomes:?}");
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let n = v["n"].as_u64().unwrap() as usize;
    assert_eq!(v["text"].as_str().unwrap().len(), 4096 * n + 1);
    let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, vec![std::ffi::OsString::from("state.json")], "no temporary file is left");
}

/// A temporary name a crashed process with this pid left is skipped, not failed on.
#[test]
fn a_stale_temporary_name_does_not_fail_a_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.json");
    // From the next name on (other tests' saves move it): the save skips every one of them.
    let next = super::NEXT.load(std::sync::atomic::Ordering::Relaxed);
    for n in next..next + 256 {
        std::fs::write(dir.path().join(format!(".state.json.tmp-{}-{n}", std::process::id())), "stale").unwrap();
    }
    save_atomic(&path, &serde_json::json!({"ok": true})).unwrap();
    assert_eq!(load::<serde_json::Value>(&path).unwrap().unwrap(), serde_json::json!({"ok": true}));
    assert!(super::NEXT.load(std::sync::atomic::Ordering::Relaxed) > next + 256, "the stale names were skipped");
}
