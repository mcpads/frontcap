use super::*;
use crate::intervention::StateField;

fn recipe() -> Recipe {
    let json = r##"{
        "url": "http://127.0.0.1:1/checkout.html",
        "profile": {"initial_date_ms": 0, "time_zone": "UTC", "locale": "en-US", "random_seed": 0,
                    "viewport": {"width": 800, "height": 600, "device_scale_milli": 1000},
                    "software_rendering": true, "frame_interval_us": 16667, "starvation_limit": 1000,
                    "host_class": "unspecified"},
        "rules": [{"id": "pay", "match": {"url": "https://api.example.test/pay"}, "action": {"type": "mock", "status": 402}}],
        "passthrough_origins": null,
        "barrier": {"type": "until", "predicate": {"is": "attached", "target": {"by": "test_id", "id": "cart"}}, "max_ms": 2000},
        "changes": [{"target": {"by": "css", "selector": "#note"}, "field": {"type": "text"}, "value": "hi", "expected": null}],
        "verify": [{"name": "cart-shown", "predicate": {"is": "visible", "target": {"by": "test_id", "id": "cart"}}}]
    }"##;
    serde_json::from_str(json).unwrap()
}

#[test]
fn recipe_progress_is_optional_and_bounded() {
    let mut recipe = recipe();
    assert!(serde_json::to_value(&recipe).unwrap().get("progress_ms").is_none(), "old recipe content retains its identity");
    recipe.progress_ms = Some(10_000);
    assert!(recipe.validate().is_ok());
    recipe.progress_ms = Some(10_001);
    assert!(recipe.validate().is_err());
}

#[test]
fn identity_follows_content_only() {
    let a = BasisManifest { kind: BasisKind::Recipe, recipe: Some(recipe()), replay: None, checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify: Vec::new() };
    let b = BasisManifest { kind: BasisKind::Recipe, recipe: Some(recipe()), replay: None, checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify: Vec::new() };
    assert_eq!(a.sha256(), b.sha256());
    assert_eq!(a.saved_id(), b.saved_id());
    assert!(a.saved_id().as_str().starts_with("b-") && a.saved_id().as_str().len() == 18);

    let mut changed = b.clone();
    changed.recipe.as_mut().unwrap().url.push('?');
    assert_ne!(a.saved_id(), changed.saved_id());
    let mut file = b.clone();
    file.files.insert("files/x.jsonl".into(), "00".repeat(32));
    assert_ne!(a.sha256(), file.sha256());
}

#[test]
fn a_valid_recipe_passes() {
    recipe().validate().unwrap();
}

#[test]
fn references_are_not_portable_but_role_predicates_are() {
    let mut r = recipe();
    r.changes[0].target = Locator::Ref { id: "e3".into() };
    assert!(r.validate().unwrap_err().to_string().contains("reference"));

    let mut r = recipe();
    r.verify[0].predicate = Predicate::Visible { target: Locator::Role { role: "button".into(), name: None } };
    assert!(r.validate().is_ok(), "native role predicates are portable; engine capability is checked at execution");

    let mut r = recipe();
    r.barrier = Barrier::Until { predicate: Predicate::Attached { target: Locator::Ref { id: "e1".into() } }, max_ms: 10 };
    assert!(r.validate().is_err());

    let mut r = recipe();
    r.changes[0].field = StateField::Cookies;
    assert!(r.validate().unwrap_err().to_string().contains("read-only"));
}

#[test]
fn bounds_and_names_are_checked() {
    let mut r = recipe();
    r.barrier = Barrier::Settle { idle_window_ms: 250, max_ms: MAX_BARRIER_MS + 1 };
    assert!(r.validate().is_err());

    let mut r = recipe();
    r.barrier = Barrier::Until { predicate: r.verify[0].predicate.clone(), max_ms: 0 };
    assert!(r.validate().is_err());

    let mut r = recipe();
    r.verify.push(r.verify[0].clone());
    assert!(r.validate().unwrap_err().to_string().contains("twice"));

    let mut r = recipe();
    r.verify[0].name = WatchName::new("mcp-x").unwrap();
    assert!(r.validate().is_err());

    let mut r = recipe();
    let change = r.changes[0].clone();
    r.changes = vec![change; MAX_CHANGES + 1];
    assert!(r.validate().is_err());
    assert!(matches!(r.changes[0].field, StateField::Text));
}

#[test]
fn unknown_fields_are_rejected() {
    let mut v = serde_json::to_value(recipe()).unwrap();
    v["extra"] = serde_json::json!(1);
    assert!(serde_json::from_value::<Recipe>(v).is_err());
    let kind: Result<BasisKind, _> = serde_json::from_str("\"snapshot\"");
    assert!(kind.is_err());
}

#[test]
fn a_basis_holds_exactly_its_kinds_content() {
    let source = ReplaySource { run: RunId::new("r-1").unwrap(), manifest_sha256: "00".repeat(32), commands: 3, kept: false };
    let recipe_basis = BasisManifest { kind: BasisKind::Recipe, recipe: Some(recipe()), replay: None, checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify: Vec::new() };
    recipe_basis.validate().unwrap();
    let replay_basis = BasisManifest { kind: BasisKind::Replay, recipe: None, replay: Some(source.clone()), checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify: Vec::new() };
    replay_basis.validate().unwrap();
    // A replay or checkpoint basis carries its children's verify; a recipe keeps its own in the recipe.
    let check = Verification { name: crate::ids::WatchName::new("ready").unwrap(), predicate: crate::predicate::Predicate::Attached { target: crate::predicate::Locator::Css { selector: "#ready".into() } } };
    BasisManifest { verify: vec![check.clone()], ..replay_basis.clone() }.validate().unwrap();
    assert!(BasisManifest { verify: vec![check.clone(), check.clone()], ..replay_basis.clone() }.validate().is_err(), "names are unique");
    assert!(BasisManifest { verify: vec![check], ..recipe_basis.clone() }.validate().is_err());
    assert!(BasisManifest { kind: BasisKind::Replay, recipe: Some(recipe()), replay: Some(source), checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify: Vec::new() }.validate().is_err());
    assert!(BasisManifest { kind: BasisKind::Recipe, recipe: None, replay: None, checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify: Vec::new() }.validate().is_err());
    assert!(BasisManifest { kind: BasisKind::Checkpoint, recipe: None, replay: None, checkpoint: None, files: BTreeMap::new(), origin: None, excluded: Vec::new(), verify: Vec::new() }.validate().is_err());
    let image = CheckpointSource { tree_sha256: "00".repeat(32), bytes: 1 };
    let origin = BasisOrigin {
        launch_id: crate::ids::LaunchId::new("g-1").unwrap(), run: RunId::new("r-1").unwrap(),
        boundary: crate::observation::ObservationBoundary {
            launch_id: crate::ids::LaunchId::new("g-1").unwrap(), state_revision: 0, t_us: crate::time::VirtualTime(0),
            context_id: "page".into(), document_id: "1".into(),
        },
    };
    let checkpoint = |origin: Option<BasisOrigin>| BasisManifest { kind: BasisKind::Checkpoint, recipe: None, replay: None, checkpoint: Some(image.clone()), files: BTreeMap::new(), origin, excluded: Vec::new(), verify: Vec::new() };
    checkpoint(Some(origin)).validate().unwrap();
    assert!(checkpoint(None).validate().is_err(), "an image is of one generation at one boundary");
    // A recipe basis serializes as it did before replay and checkpoint bases existed, so its identity is unchanged.
    let json = serde_json::to_string(&recipe_basis).unwrap();
    assert!(!json.contains("replay") && !json.contains("checkpoint"));
}
