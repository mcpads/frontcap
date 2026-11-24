use crate::products::{inventory, read_file};
use super::*;

#[test]
fn standalone_product_freezes_dart_assets_and_rejects_linked_inputs() {
    let root = tempfile::tempdir().unwrap();
    let product = root.path().join("Client.app");
    std::fs::create_dir_all(product.join("flutter_assets")).unwrap();
    std::fs::write(product.join("Info.plist"), b"plist").unwrap();
    std::fs::write(
        product.join("flutter_assets/kernel_blob.bin"),
        b"compiled Dart",
    )
    .unwrap();
    let (directories, names) = inventory(&product).unwrap();
    let files = names
        .into_iter()
        .map(|name| {
            let file = read_file(&product.join(&name), None).unwrap();
            (name, file)
        })
        .collect();
    let launch = Launch {
        product: product.clone(),
        inventory: Inventory { files, directories },
    };
    launch.verify().unwrap();
    std::fs::write(
        product.join("flutter_assets/kernel_blob.bin"),
        b"changed Dart",
    )
    .unwrap();
    assert!(launch.verify().is_err());
    std::fs::write(
        product.join("flutter_assets/kernel_blob.bin"),
        b"compiled Dart",
    )
    .unwrap();
    std::fs::write(product.join("extra"), b"unexpected").unwrap();
    assert!(launch.verify().is_err());
    std::fs::remove_file(product.join("extra")).unwrap();
    std::os::unix::fs::symlink(&product, root.path().join("Linked.app")).unwrap();
    assert!(
        Launch {
            product: root.path().join("Linked.app"),
            inventory: launch.inventory
        }
        .verify()
        .is_err()
    );
}
