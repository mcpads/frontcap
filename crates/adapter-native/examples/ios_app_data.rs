//! Construct declared first-launch data bound to an ordinary iOS application product.
use frontcap_native::{app_data::Recipe, products::BundleInventory};
use frontcap_core::canonical::{canonical_of, sha256_hex};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let [app, bundle, source, destination, child] = args.as_slice() else {
        return Err("usage: ios_app_data app-directory bundle-id data-directory new-preparation child-uuid".into());
    };
    let product = BundleInventory::capture(&PathBuf::from(app))?;
    let recipe = Recipe {
        app_id: bundle.clone(),
        product_sha256: sha256_hex(&canonical_of(&product)),
        inventory: BundleInventory::capture(&PathBuf::from(source))?,
    };
    let prepared = recipe.construct(&PathBuf::from(source), &PathBuf::from(destination),
        child, bundle, &recipe.product_sha256)?;
    println!("{}", serde_json::json!({
        "child_id": prepared.child_id,
        "recipe_sha256": prepared.recipe_sha256,
        "product_sha256": recipe.product_sha256,
        "declared_files": recipe.inventory.files.len(),
    }));
    Ok(())
}
