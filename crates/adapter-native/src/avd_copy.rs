//! A frozen AVD input copied into the live group's scratch; source paths never grant cleanup authority.
use crate::products::{BundleInventory, ProductFile};
use std::collections::BTreeMap;
use frontcap_core::state::save_atomic;
use std::{io, path::{Component, Path, PathBuf}};

pub(crate) fn prepare(source: &Path, name: &str, inventory: &BundleInventory,
    scratch: &Path, evidence: &Path, check: &mut dyn FnMut() -> io::Result<()>) -> io::Result<PathBuf> {
    check()?;
    if !source.is_absolute() || name.is_empty() || name.len() > 128 || name == "." || name == ".."
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b)) {
        return Err(io::Error::other("invalid frozen AVD source or name"));
    }
    inventory.validate()?;
    let registry = format!("{name}.ini");
    let device = format!("{name}.avd");
    let prefix = format!("{device}/");
    if !inventory.files.contains_key(&registry) || !inventory.files.contains_key(&format!("{prefix}config.ini"))
        || inventory.files.keys().any(|p| p != &registry && !p.starts_with(&prefix))
        || inventory.directories.iter().any(|p| p != &device && !p.starts_with(&prefix)) {
        return Err(io::Error::other("frozen AVD inventory must contain exactly the selected device"));
    }
    let destination = scratch.join("avd");
    // Exclusive copy refuses repeat requests and preserves partial copies for group retirement.
    inventory.copy_verified_checked(source, &destination, check)?;
    check()?;
    // A copied owner marker cannot represent authority over this newly created, unused tree.
    // In particular, a stale snapshot PID may now identify an unrelated live host process.
    // Never delete the source marker or signal that PID; record this exact preparation change.
    let mut discarded_owner_records = BTreeMap::new();
    let snapshot_owner = format!("{prefix}snapshot.lock.lock");
    if let Some(record) = inventory.files.get(&snapshot_owner) {
        std::fs::remove_file(destination.join(&snapshot_owner))?;
        std::fs::File::open(destination.join(&device))?.sync_all()?;
        discarded_owner_records.insert(snapshot_owner, record.clone());
    }
    let configuration = destination.join(&device).join("config.ini");
    if std::fs::metadata(&configuration)?.len() > 65536 {
        return Err(io::Error::other("AVD configuration exceeds readback bound"));
    }
    for line in std::fs::read_to_string(configuration)?.lines() {
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim(); let value = value.trim();
            if (key.starts_with("disk.") && key.ends_with(".path") || key == "sdcard.path")
                && !value.is_empty() && value != "<temp>"
                && (Path::new(value).is_absolute() || Path::new(value).components().any(|c| matches!(c, Component::ParentDir))) {
                return Err(io::Error::other("AVD writable disk path escapes its owned copy"));
            }
        }
    }
    let registry_path = destination.join(&registry);
    if std::fs::metadata(&registry_path)?.len() > 65536 {
        return Err(io::Error::other("AVD registry exceeds readback bound"));
    }
    let original = std::fs::read_to_string(&registry_path)?;
    let mut registry_text = original.lines().filter(|line| {
        let key = line.split_once('=').map(|(key, _)| key.trim());
        key != Some("path") && key != Some("path.rel")
    }).map(str::to_owned).collect::<Vec<_>>();
    let relocated = destination.join(&device);
    let relocated = relocated.to_str().filter(|path| !path.contains(['\n', '\r']))
        .ok_or_else(|| io::Error::other("AVD registry path is not representable"))?;
    registry_text.push(format!("path={relocated}"));
    let mut registry_file = std::fs::OpenOptions::new().write(true).truncate(true).open(&registry_path)?;
    use std::io::Write;
    check()?;
    registry_file.write_all((registry_text.join("\n") + "\n").as_bytes())?;
    registry_file.sync_all()?;
    // This describes only the prepared tree. Nested disk backing references and shared SDK inputs
    // remain external dependencies, not a claim of complete emulator writer confinement.
    let prepared_inventory = BundleInventory::capture_checked(&destination, check)?;
    check()?;
    save_atomic(&evidence.join("avd-copy.json"), &CopyReceipt { source, destination: &destination,
        source_inventory: inventory, prepared_inventory, discarded_owner_records })?;
    Ok(destination)
}

#[derive(serde::Serialize)]
struct CopyReceipt<'a> {
    source: &'a Path,
    destination: &'a Path,
    source_inventory: &'a BundleInventory,
    prepared_inventory: BundleInventory,
    discarded_owner_records: BTreeMap<String, ProductFile>,
}
