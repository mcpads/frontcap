use super::*;
use crate::ids::{LaunchId, RunId};

fn manifest(run: &RunId) -> Manifest {
    Manifest {
        run: run.clone(),
        launch: LaunchId::new("g-0000000000000001").unwrap(),
        engine: "HeadlessChrome/154.0.8037.92".into(),
        engine_revision: "1689415".into(),
        host_class: "macos-aarch64".into(),
        determinism: Determinism { level: Level::Time, taints: vec![] },
        profile: DeterminismProfile::default(),
        replayable: true,
        missing_inputs: vec![],
        finalization: Finalization::Open,
            recovery_snapshot: None,
        members: Default::default(),
    }
}

#[test]
fn creates_the_layout_once() {
    let dir = tempfile::tempdir().unwrap();
    let run = RunId::new("run-1").unwrap();
    let b = Bundle::create(dir.path(), &run).unwrap();
    for d in [b.rules_dir(), b.screenshots_dir(), b.derived_dir(), b.root().join("inputs/bodies")] {
        assert!(d.is_dir(), "{}", d.display());
    }
    assert!(matches!(Bundle::create(dir.path(), &run), Err(BundleError::Exists(_))));
}

#[test]
fn finalize_hashes_members_but_not_derived() {
    let dir = tempfile::tempdir().unwrap();
    let run = RunId::new("run-1").unwrap();
    let b = Bundle::create(dir.path(), &run).unwrap();
    std::fs::write(b.trace(), "{}\n").unwrap();
    std::fs::write(b.derived_dir().join("video.mp4"), "x").unwrap();
    let m = b.finalize(manifest(&run)).unwrap();
    assert_eq!(m.finalization, Finalization::Finalized);
    assert!(m.members.contains_key("trace.jsonl"));
    assert!(!m.members.keys().any(|k| k.starts_with("derived/") || k == "manifest.json"));
    let inspection = b.inspect().unwrap();
    assert!(inspection.finalized && inspection.mismatched.is_empty() && inspection.unexpected.is_empty());
    assert!(matches!(b.finalize(manifest(&run)), Err(BundleError::AlreadyFinalized)));
}

#[test]
fn inspect_detects_modified_member() {
    let dir = tempfile::tempdir().unwrap();
    let run = RunId::new("run-1").unwrap();
    let b = Bundle::create(dir.path(), &run).unwrap();
    std::fs::write(b.trace(), "{}\n").unwrap();
    b.finalize(manifest(&run)).unwrap();
    std::fs::write(b.trace(), "{\"tampered\":true}\n").unwrap();
    std::fs::write(b.root().join("extra.txt"), "x").unwrap();
    let i = Bundle::open(dir.path(), &run).unwrap().inspect().unwrap();
    assert_eq!(i.mismatched, vec!["trace.jsonl".to_string()]);
    assert_eq!(i.unexpected, vec!["extra.txt".to_string()]);
}

#[test]
fn derived_artifacts_may_be_added_after_finalization() {
    let dir = tempfile::tempdir().unwrap();
    let run = RunId::new("run-1").unwrap();
    let b = Bundle::create(dir.path(), &run).unwrap();
    b.finalize(manifest(&run)).unwrap();
    std::fs::write(b.derived_dir().join("keyframes.png"), "x").unwrap();
    let i = b.inspect().unwrap();
    assert!(i.mismatched.is_empty() && i.unexpected.is_empty());
}

#[test]
fn manifest_shape_matches_the_documented_layout() {
    let run = RunId::new("run-1").unwrap();
    let v = serde_json::to_value(manifest(&run)).unwrap();
    assert_eq!(v["determinism"]["level"], "time");
    assert!(v["determinism"]["taints"].is_array());
    assert_eq!(v["engine_revision"], "1689415");
    assert_eq!(v["profile"]["frame_interval_us"], 16_667);
    assert_eq!(v["finalization"], serde_json::json!({ "state": "open" }));
}

#[test]
fn top_level_dotfiles_are_members() {
    let dir = tempfile::tempdir().unwrap();
    let run = RunId::new("run-1").unwrap();
    let b = Bundle::create(dir.path(), &run).unwrap();
    std::fs::write(b.root().join(".hidden"), "a").unwrap();
    b.finalize(manifest(&run)).unwrap();
    std::fs::write(b.root().join(".hidden"), "b").unwrap();
    std::fs::write(b.root().join(".newdot"), "c").unwrap();
    let i = b.inspect().unwrap();
    assert_eq!(i.mismatched, vec![".hidden".to_string()]);
    assert_eq!(i.unexpected, vec![".newdot".to_string()]);
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed() {
    let dir = tempfile::tempdir().unwrap();
    let run = RunId::new("run-1").unwrap();
    let b = Bundle::create(dir.path(), &run).unwrap();
    std::os::unix::fs::symlink(b.root(), b.root().join("loop")).unwrap();
    assert!(matches!(b.finalize(manifest(&run)), Err(BundleError::Symlink(_))));
}
