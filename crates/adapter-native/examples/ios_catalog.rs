//! Validate a generic UIKit catalog independently of its XCTest producer.
use frontcap_core::native::ios_targets::Catalog;
use std::{
    io::{self, Read},
    path::PathBuf,
};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let path = PathBuf::from(args.next().ok_or("usage: ios_catalog catalog.json")?);
    if args.next().is_some() {
        return Err("usage: ios_catalog catalog.json".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(io::Error::other("catalog exceeds byte budget").into());
    }
    let catalog: Catalog = serde_json::from_slice(&bytes)?;
    catalog.validate()?;
    println!(
        "validated {} targets from observation {}",
        catalog.targets.len(),
        catalog.observation
    );
    Ok(())
}
