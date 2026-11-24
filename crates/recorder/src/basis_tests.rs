use super::*;
use frontcap_core::basis::{Barrier, BasisKind, Recipe};
use std::collections::BTreeMap;

fn recipe() -> Recipe {
    Recipe {
        url: "http://127.0.0.1:1/".into(),
        profile: Default::default(),
        rules: Default::default(),
        passthrough_origins: None,
        barrier: Barrier::default(),
        changes: vec![],
        progress_ms: None,
        dialog_rules: Default::default(),
        verify: vec![],
    }
}

fn workspace() -> (tempfile::TempDir, Workspace) {
    let tmp = tempfile::tempdir().unwrap();
    let ws = Workspace { root: tmp.path().join(".frontcap") };
    (tmp, ws)
}

#[test]
fn saving_the_same_content_returns_the_same_basis() {
    let (_tmp, ws) = workspace();
    let manifest = BasisManifest { kind: BasisKind::Recipe, recipe: Some(recipe()), replay: None, checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify: Vec::new() };
    let first = save(&ws, manifest.clone(), vec![]).unwrap();
    let again = save(&ws, manifest.clone(), vec![]).unwrap();
    assert_eq!(first.id, again.id);
    assert_eq!(first.sha256, manifest.sha256());
    assert_eq!(load(&ws, &first.id).unwrap().manifest, manifest);
    let entries: Vec<_> = std::fs::read_dir(ws.saved()).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(entries.len(), 1, "no private directory is left behind: {entries:?}");
}

#[test]
fn files_are_kept_by_digest_and_checked_on_every_read() {
    let (_tmp, ws) = workspace();
    let bytes = b"{\"line\":1}\n".to_vec();
    let name = file_name(&bytes, "jsonl");
    let manifest = BasisManifest { kind: BasisKind::Recipe, recipe: Some(recipe()), replay: None, checkpoint: None, files: BTreeMap::from([(name.clone(), sha256_bytes_hex(&bytes))]), origin: None, excluded: Vec::new(), verify: Vec::new() };
    let saved = save(&ws, manifest, vec![(name.clone(), bytes)]).unwrap();
    assert_eq!(std::fs::read(saved.file(&name)).unwrap(), b"{\"line\":1}\n");
    std::fs::write(saved.file(&name), b"changed").unwrap();
    assert!(matches!(load(&ws, &saved.id), Err(BasisError::Corrupt { .. })));
}

#[test]
fn a_changed_manifest_or_digest_is_refused() {
    let (_tmp, ws) = workspace();
    let saved = save(&ws, BasisManifest { kind: BasisKind::Recipe, recipe: Some(recipe()), replay: None, checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify: Vec::new() }, vec![]).unwrap();
    let path = saved.dir.join("manifest.json");
    let original = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, original.replace("127.0.0.1:1", "127.0.0.1:2")).unwrap();
    let error = load(&ws, &saved.id).unwrap_err().to_string();
    assert!(error.contains("identity"), "{error}");
    std::fs::write(&path, &original).unwrap();
    std::fs::write(saved.dir.join("sha256"), "0".repeat(64)).unwrap();
    assert!(matches!(load(&ws, &saved.id), Err(BasisError::Corrupt { .. })));
}

#[test]
fn unknown_and_mismatched_bases_are_refused() {
    let (_tmp, ws) = workspace();
    assert!(matches!(load(&ws, &SavedId::new("b-0000000000000000").unwrap()), Err(BasisError::NotFound(_))));
    let manifest = BasisManifest { kind: BasisKind::Recipe, recipe: Some(recipe()), replay: None, checkpoint: None, files: BTreeMap::from([("files/x.jsonl".into(), "00".into())]), origin: None, excluded: Vec::new(), verify: Vec::new() };
    assert!(matches!(save(&ws, manifest, vec![]), Err(BasisError::Corrupt { .. })));
    assert!(!ws.saved().exists(), "a refused save writes nothing");
}

#[test]
fn a_kept_prefix_moves_into_the_basis_and_is_checked_on_every_read() {
    let (tmp, ws) = workspace();
    let run = frontcap_core::ids::RunId::new("r-kept").unwrap();
    let staging = tmp.path().join("staging");
    let bundle = frontcap_core::bundle::Bundle::create(&staging, &run).unwrap();
    std::fs::write(bundle.root().join("trace.jsonl"), "").unwrap();
    std::fs::write(bundle.root().join("manifest.json"), "{\"finalized\":true}").unwrap();
    let digest = sha256_bytes_hex(b"{\"finalized\":true}");
    let source = frontcap_core::basis::ReplaySource { run: run.clone(), manifest_sha256: digest, commands: 0, kept: true };
    let manifest = BasisManifest { kind: BasisKind::Replay, recipe: None, replay: Some(source), checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify: Vec::new() };
    let saved = save_kept(&ws, manifest, &staging.join("r-kept")).unwrap();
    assert!(!staging.join("r-kept").exists(), "the copy moved into the basis");
    assert_eq!(saved.replay_source(&ws).unwrap(), saved.dir.join("bundle/r-kept"));
    std::fs::write(saved.dir.join("bundle/r-kept/manifest.json"), "{}").unwrap();
    assert!(matches!(load(&ws, &saved.id), Err(BasisError::Corrupt { .. })));
}

/// A staged process image becomes a checkpoint basis named by its content; a changed image is refused.
#[test]
fn a_checkpoint_basis_pins_its_image() {
    use std::os::unix::fs::DirBuilderExt;
    let (_tmp, ws) = workspace();
    std::fs::create_dir_all(&ws.root).unwrap();
    let stage = |name: &str, content: &str| {
        let dir = crate::image::image_dir(&ws, name).unwrap();
        std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
        std::fs::create_dir(dir.join("images")).unwrap();
        std::fs::write(dir.join("images/pages-1.img"), content).unwrap();
        std::os::unix::fs::symlink("pages-1.img", dir.join("images/link")).unwrap();
    };
    let origin = || BasisOrigin {
        launch_id: frontcap_core::ids::LaunchId::new("g-1").unwrap(),
        run: frontcap_core::ids::RunId::new("r-1").unwrap(),
        boundary: frontcap_core::observation::ObservationBoundary {
            launch_id: frontcap_core::ids::LaunchId::new("g-1").unwrap(), state_revision: 0,
            t_us: frontcap_core::time::VirtualTime(0), context_id: "page".into(), document_id: "1".into(),
        },
    };
    stage("s-1", "memory");
    let saved = save_checkpoint(&ws, "s-1", origin(), vec!["servers".into()], vec![]).unwrap();
    assert_eq!(saved.manifest.checkpoint.as_ref().unwrap().bytes, 6);
    assert_eq!(saved.checkpoint_image(), Some(saved.id.as_str()));
    assert!(!ws.images().join("s-1").exists(), "the staged image is renamed to the basis");
    let image = ws.images().join(saved.id.as_str());
    assert!(image.join("images/pages-1.img").exists());
    assert_eq!(load(&ws, &saved.id).unwrap().id, saved.id);
    // Another image is another basis.
    stage("s-2", "other");
    assert_ne!(save_checkpoint(&ws, "s-2", origin(), vec![], vec![]).unwrap().id, saved.id);
    // A name that leaves the images is refused before anything is read.
    assert!(save_checkpoint(&ws, "../s-1", origin(), vec![], vec![]).is_err());
    std::fs::write(image.join("images/pages-1.img"), "change").unwrap();
    assert!(matches!(load(&ws, &saved.id), Err(BasisError::Corrupt { .. })), "a changed image is never restored");
}

/// A deleted basis goes with its image; deletion is refused while a branch holds it.
#[test]
fn a_basis_is_deleted_with_its_image_unless_a_branch_holds_it() {
    use std::os::unix::fs::DirBuilderExt;
    let (_tmp, ws) = workspace();
    let manifest = BasisManifest { kind: BasisKind::Recipe, recipe: Some(recipe()), replay: None, checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify: Vec::new() };
    let saved = save(&ws, manifest.clone(), vec![]).unwrap();
    let held = hold(&ws, &saved.id).unwrap();
    assert!(matches!(delete(&ws, &saved.id), Err(BasisError::InUse(_))), "refused while a branch constructs");
    assert!(load(&ws, &saved.id).is_ok());
    drop(held);
    delete(&ws, &saved.id).unwrap();
    assert!(matches!(load(&ws, &saved.id), Err(BasisError::NotFound(_))));
    assert!(matches!(hold(&ws, &saved.id), Err(BasisError::NotFound(_))));
    assert!(matches!(delete(&ws, &saved.id), Err(BasisError::NotFound(_))));
    assert_eq!(std::fs::read_dir(ws.saved()).unwrap().count(), 0, "nothing is left behind");
    // The same content saves again under the same id.
    assert_eq!(save(&ws, manifest, vec![]).unwrap().id, saved.id);

    let dir = crate::image::image_dir(&ws, "s-1").unwrap();
    std::fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
    std::fs::write(dir.join("pages-1.img"), "memory").unwrap();
    let origin = BasisOrigin {
        launch_id: frontcap_core::ids::LaunchId::new("g-1").unwrap(),
        run: frontcap_core::ids::RunId::new("r-1").unwrap(),
        boundary: frontcap_core::observation::ObservationBoundary {
            launch_id: frontcap_core::ids::LaunchId::new("g-1").unwrap(), state_revision: 0,
            t_us: frontcap_core::time::VirtualTime(0), context_id: "page".into(), document_id: "1".into(),
        },
    };
    let checkpoint = save_checkpoint(&ws, "s-1", origin, vec![], vec![]).unwrap();
    // What a deletion interrupted by a crash left is removed by the next.
    std::fs::create_dir(ws.images().join(".deleted.b-0.g-0")).unwrap();
    delete(&ws, &checkpoint.id).unwrap();
    assert_eq!(std::fs::read_dir(ws.images()).unwrap().count(), 0, "the image goes with its basis");
    assert!(matches!(load(&ws, &checkpoint.id), Err(BasisError::NotFound(_))));
}
