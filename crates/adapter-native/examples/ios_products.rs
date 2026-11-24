//! Freeze a compiled XCTest products tree, or copy it against an existing manifest.
use frontcap_native::products::{ProductNames, ProductRole, ProductsManifest};
use std::path::PathBuf;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 6 && args.len() != 7 {
        return Err("usage: ios_products source xctestrun manifest-output copied-products app-product test-product [external-runner]".into());
    }
    let source = PathBuf::from(&args[0]);
    let xctestrun = args[1].to_str().ok_or("invalid test configuration name")?;
    let names = ProductNames {
        app: args[4].to_str().ok_or("invalid app product name")?.into(),
        tests: args[5].to_str().ok_or("invalid test product name")?.into(),
    };
    let role = if args.len() == 7 {
        if args[6] != "external-runner" {
            return Err("unknown product role".into());
        }
        ProductRole::ExternalRunner
    } else {
        ProductRole::ClockApp
    };
    let manifest = ProductsManifest::capture_role(&source, xctestrun, names, role)?;
    let manifest_path = PathBuf::from(&args[2]);
    if manifest_path.exists() {
        return Err("manifest output already exists".into());
    }
    frontcap_core::state::save_atomic(&manifest_path, &manifest)?;
    manifest.copy_verified(&source, &PathBuf::from(&args[3]))?;
    println!(
        "Verified {} files in {} directories",
        manifest.files.len(),
        manifest.directories.len()
    );
    Ok(())
}
