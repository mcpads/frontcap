use super::*;
use std::fs;
const A: &str = "10000000-0000-0000-0000-000000000001";
const B: &str = "10000000-0000-0000-0000-000000000002";
fn fixture() -> (tempfile::TempDir, Recipe) {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir_all(root.path().join("source/Documents")).unwrap();
    fs::write(
        root.path().join("source/Documents/state.json"),
        b"persisted state",
    )
    .unwrap();
    let recipe = Recipe {
        app_id: "org.example.client".into(),
        product_sha256: "a".repeat(64),
        inventory: BundleInventory::capture(&root.path().join("source")).unwrap(),
    };
    (root, recipe)
}
#[test]
fn children_preserve_declared_bytes_without_sharing_mutations() {
    let (root, recipe) = fixture();
    let source = root.path().join("source");
    for (name, id) in [("a", A), ("b", B)] {
        let child = root.path().join(name);
        let receipt = recipe
            .construct(&source, &child, id, &recipe.app_id, &recipe.product_sha256)
            .unwrap();
        assert_eq!(
            Prepared::read(&child, &recipe.app_id, &recipe.product_sha256).unwrap(),
            receipt
        );
    }
    fs::write(
        root.path().join("a/data/Documents/state.json"),
        b"local edit",
    )
    .unwrap();
    assert_eq!(
        fs::read(root.path().join("b/data/Documents/state.json")).unwrap(),
        b"persisted state"
    );
    recipe.inventory.verify(&source).unwrap();
    assert!(
        Prepared::read(
            &root.path().join("a"),
            &recipe.app_id,
            &recipe.product_sha256
        )
        .is_err()
    );
    assert!(
        recipe
            .construct(
                &source,
                &root.path().join("b"),
                A,
                &recipe.app_id,
                &recipe.product_sha256
            )
            .is_err()
    );
}
#[test]
fn incompatible_or_changed_inputs_reject_before_creating_a_child() {
    let (root, recipe) = fixture();
    let child = root.path().join("child");
    let source = root.path().join("source");
    assert!(
        recipe
            .construct(&source, &child, A, "another.app", &recipe.product_sha256)
            .is_err()
    );
    assert!(
        recipe
            .construct(&source, &child, A, &recipe.app_id, &"b".repeat(64))
            .is_err()
    );
    fs::write(source.join("Documents/state.json"), b"changed source").unwrap();
    assert!(
        recipe
            .construct(&source, &child, A, &recipe.app_id, &recipe.product_sha256)
            .is_err()
    );
    assert!(!child.exists());
}
#[test]
fn incomplete_construction_and_links_never_admit_prepared_data() {
    let (root, recipe) = fixture();
    let child = root.path().join("child");
    recipe
        .construct(
            &root.path().join("source"),
            &child,
            A,
            &recipe.app_id,
            &recipe.product_sha256,
        )
        .unwrap();
    // Simulate interruption after writing data, before publishing completion.
    fs::remove_file(child.join("prepared.json")).unwrap();
    assert!(child.join("data/Documents/state.json").exists());
    assert!(child.join("construction.json").exists());
    assert!(Prepared::read(&child, &recipe.app_id, &recipe.product_sha256).is_err());
    assert!(
        recipe
            .construct(
                &root.path().join("source"),
                &child,
                A,
                &recipe.app_id,
                &recipe.product_sha256
            )
            .is_err()
    );
    std::os::unix::fs::symlink("state.json", root.path().join("source/Documents/alias")).unwrap();
    assert!(
        recipe
            .construct(
                &root.path().join("source"),
                &root.path().join("linked"),
                A,
                &recipe.app_id,
                &recipe.product_sha256
            )
            .is_err()
    );
    assert!(!root.path().join("linked").exists());
}

#[test]
fn container_application_checks_all_destinations_before_writing() {
    let (root, mut recipe) = fixture();
    let source = root.path().join("source");
    fs::write(source.join("Documents/a.json"), b"new first file").unwrap();
    recipe.inventory = BundleInventory::capture(&source).unwrap();
    let child = root.path().join("child");
    let prepared = recipe
        .construct(&source, &child, A, &recipe.app_id, &recipe.product_sha256)
        .unwrap();
    let container = root.path().join("container");
    fs::create_dir_all(container.join("Documents")).unwrap();
    fs::write(container.join("Documents/state.json"), b"already persisted").unwrap();
    assert!(apply_new_files(&prepared, &child.join("data"), &container).is_err());
    assert!(!container.join("Documents/a.json").exists());
    assert_eq!(
        fs::read(container.join("Documents/state.json")).unwrap(),
        b"already persisted"
    );
    fs::remove_dir_all(container.join("Documents")).unwrap();
    let outside = root.path().join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, container.join("Documents")).unwrap();
    assert!(apply_new_files(&prepared, &child.join("data"), &container).is_err());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

#[test]
fn container_application_admits_all_payload_before_creating_files() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let (root, recipe) = fixture();
    let source = root.path().join("source");
    let child = root.path().join("child");
    let prepared = recipe.construct(&source, &child, A, &recipe.app_id, &recipe.product_sha256).unwrap();
    let bytes: u64 = recipe.inventory.files.values().map(|file| file.bytes).sum();
    for (name, allowance) in [("small", bytes - 1), ("enough", bytes)] {
        let container = root.path().join(name);
        fs::create_dir(&container).unwrap();
        let budget = WriteBudget::create(&root.path().join(format!("{name}-budget")),
            LaunchId::generate(), Limits { ordinary: allowance, retirement: 1024 }).unwrap();
        let result = apply_new_files_with_budget(&prepared, &child.join("data"), &container, Some(&budget));
        if allowance < bytes {
            assert!(result.is_err());
            assert_eq!(fs::read_dir(&container).unwrap().count(), 0);
            assert_eq!(budget.charged().unwrap().ordinary, 0);
        } else {
            result.unwrap();
            recipe.inventory.verify(&container).unwrap();
            assert_eq!(budget.charged().unwrap().ordinary, bytes);
            assert!(apply_new_files_with_budget(&prepared, &child.join("data"), &container, Some(&budget)).is_err());
            assert_eq!(budget.charged().unwrap().ordinary, bytes);
        }
        assert_eq!(budget.charged().unwrap().retirement, 0);
    }
    recipe.inventory.verify(&source).unwrap();
    Prepared::read(&child, &recipe.app_id, &recipe.product_sha256).unwrap();
}
