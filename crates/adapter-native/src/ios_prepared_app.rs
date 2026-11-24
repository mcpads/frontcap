//! Admission for a platform-prepared Simulator app; no consumer SDK or second injection.
use super::Launch;
use crate::{products::BundleInventory, simulator::Simulator};
use frontcap_core::state::load;
use super::save_evidence;
use serde::{Deserialize, Serialize};
use std::{collections::{BTreeMap, BTreeSet}, fs::File, io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf}, sync::atomic::AtomicBool, time::Instant};

const FOLDER: &str = "FrontcapAttachment";
const LOADER: &str = "FrontcapAttachment/loader.dylib";
const LIBRARY: &str = "FrontcapAttachment/native-input.dylib";
const SCRIPT: &str = "FrontcapAttachment/timeline.js";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub original: PathBuf,
    pub original_inventory: BundleInventory,
    pub inventory: BundleInventory,
    pub executable: String,
    pub shared_libraries: BTreeSet<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Installed {
    simulator: String,
    bundle: String,
    pub(super) container: PathBuf,
    inventory: BundleInventory,
}
#[derive(Deserialize)]
struct Info {
    #[serde(rename = "CFBundleIdentifier")]
    bundle: String,
    #[serde(rename = "CFBundleExecutable")]
    executable: String,
    #[serde(rename = "CFBundleSupportedPlatforms")]
    platforms: Vec<String>,
}
fn bad(message: &str) -> io::Error { io::Error::other(message) }
impl Prepared {
    /// Pure configuration checks also apply to offline result readers.
    pub(crate) fn validate(&self, config: &Launch) -> io::Result<()> {
        self.original_inventory.validate()?; self.inventory.validate()?;
        if !self.original.is_absolute() || self.original.extension().is_none_or(|x| x != "app")
            || !config.app.is_absolute() || config.app.extension().is_none_or(|x| x != "app")
            || self.original == config.app || self.executable.is_empty()
            || Path::new(&self.executable).components().count() != 1
            || !matches!(Path::new(&self.executable).components().next(), Some(std::path::Component::Normal(_)))
            || !config.environment.is_empty() || !config.page_registry || !config.mute_audio
            || self.original_inventory.files.keys().chain(self.original_inventory.directories.iter())
                .any(|name| name == FOLDER || name.starts_with(&format!("{FOLDER}/"))) {
            return Err(bad("invalid prepared app configuration"));
        }
        let original = &self.original_inventory.files;
        let files = &self.inventory.files;
        if !original.contains_key("Info.plist") || !original.get(&self.executable).is_some_and(|f| f.executable)
            || !files.get(&self.executable).is_some_and(|f| f.executable)
            || !files.contains_key(LOADER)
            || files.get(LIBRARY).map(|f| &f.sha256) != Some(&config.library.sha256)
            || files.get(SCRIPT).map(|f| &f.sha256) != config.script.as_ref().map(|f| &f.sha256)
            || config.script.is_none() {
            return Err(bad("prepared attachment differs from launch"));
        }
        for (name, before) in original {
            let after = files.get(name).ok_or_else(|| bad("prepared app removed a consumer file"))?;
            if before != after && (name != &self.executable && !name.starts_with("_CodeSignature/")
                || before.executable != after.executable) {
                return Err(bad("prepared app changed consumer content"));
            }
        }
        for name in files.keys().filter(|name| !original.contains_key(*name)) {
            if ![LOADER, LIBRARY, SCRIPT, "_CodeSignature/CodeResources"].contains(&name.as_str()) {
                return Err(bad("unexpected prepared app file"));
            }
        }
        let mut directories = self.original_inventory.directories.clone();
        directories.insert(FOLDER.into());
        if files.contains_key("_CodeSignature/CodeResources") { directories.insert("_CodeSignature".into()); }
        if directories != self.inventory.directories
            || !self.shared_libraries.contains(LOADER) || !self.shared_libraries.contains(LIBRARY)
            || self.shared_libraries.contains(&self.executable)
            || self.shared_libraries.iter().any(|name| !files.contains_key(name)) {
            return Err(bad("prepared app directories or library inventory differ"));
        }
        Ok(())
    }
    fn verify_inputs(&self, config: &Launch) -> io::Result<()> {
        self.validate(config)?;
        let original = self.original.canonicalize()?;
        let staged = config.app.canonicalize()?;
        if staged.starts_with(&original) || original.starts_with(&staged) {
            return Err(bad("prepared and original apps overlap"));
        }
        self.original_inventory.verify(&self.original)?;
        self.inventory.verify(&config.app)?;
        let info: Info = plist::from_file(config.app.join("Info.plist")).map_err(io::Error::other)?;
        if info.bundle != config.bundle || info.executable != self.executable
            || info.platforms != ["iPhoneSimulator"] {
            return Err(bad("prepared app identity differs from launch"));
        }
        let actual = self.inventory.files.keys().filter_map(|name| match shared_library(&config.app.join(name)) {
            Ok(true) => Some(Ok(name.clone())), Ok(false) => None, Err(e) => Some(Err(e)),
        }).collect::<io::Result<BTreeSet<_>>>()?;
        if actual != self.shared_libraries { return Err(bad("prepared Mach-O library inventory differs")); }
        Ok(())
    }
    fn validate_installed(&self, actual: &BundleInventory) -> io::Result<()> {
        validate_installed_inventory(&self.inventory, actual, &self.shared_libraries)
    }
}
impl Launch {
    pub(crate) fn validate_attachment(&self) -> io::Result<()> {
        if let Some(prepared) = &self.prepared { prepared.verify_inputs(self) }
        else if self.app.join(FOLDER).try_exists()? {
            Err(bad("prepared app requires explicit admission; refusing double injection"))
        } else { Ok(()) }
    }
}

/// CoreSimulator can remove execute bits from caller-verified loadable Mach-O images.
/// Every byte, path, directory and other execution mode must still match the frozen product.
pub(crate) fn validate_installed_inventory(expected: &BundleInventory, actual: &BundleInventory,
    loadable_images: &BTreeSet<String>) -> io::Result<()> {
    let mut normalized = actual.clone();
    for name in loadable_images {
        if let (Some(expected), Some(actual)) = (expected.files.get(name), normalized.files.get_mut(name))
            && expected.executable && !actual.executable {
            actual.executable = true;
        }
    }
    if &normalized != expected { return Err(bad("installed app differs from prepared product")); }
    Ok(())
}

/// Read just Mach-O headers, including all universal slices, without executing a tool or image.
pub(crate) fn shared_library(path: &Path) -> io::Result<bool> { image_type(path, 6) }
/// Used only for the declared XCTest bundle executable, never an arbitrary app executable.
pub(crate) fn test_bundle(path: &Path) -> io::Result<bool> { image_type(path, 8) }
fn image_type(path: &Path, expected: u32) -> io::Result<bool> {
    let mut file = File::open(path)?;
    fn header(file: &mut File, offset: u64) -> io::Result<Option<([u8; 4], [u8; 12])>> {
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = [0; 16];
        match file.read_exact(&mut bytes) {
            Ok(()) => Ok(Some((bytes[..4].try_into().unwrap(), bytes[4..].try_into().unwrap()))),
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None), Err(e) => Err(e),
        }
    }
    let Some((magic, rest)) = header(&mut file, 0)? else { return Ok(false); };
    let thin = |magic: [u8; 4], rest: [u8; 12]| -> bool {
        match magic {
            [0xce,0xfa,0xed,0xfe] | [0xcf,0xfa,0xed,0xfe] => u32::from_le_bytes(rest[8..12].try_into().unwrap()) == expected,
            [0xfe,0xed,0xfa,0xce] | [0xfe,0xed,0xfa,0xcf] => u32::from_be_bytes(rest[8..12].try_into().unwrap()) == expected,
            _ => false,
        }
    };
    if thin(magic, rest) { return Ok(true); }
    let wide = match magic { [0xca,0xfe,0xba,0xbe] => false, [0xca,0xfe,0xba,0xbf] => true, _ => return Ok(false) };
    let count = u32::from_be_bytes(rest[..4].try_into().unwrap());
    if count == 0 || count > 64 { return Err(bad("invalid universal Mach-O slice count")); }
    for index in 0..count {
        file.seek(SeekFrom::Start(8 + u64::from(index) * if wide { 32 } else { 20 } + 8))?;
        let mut bytes = [0; 8]; file.read_exact(&mut bytes[..if wide { 8 } else { 4 }])?;
        let offset = if wide { u64::from_be_bytes(bytes) } else { u64::from(u32::from_be_bytes(bytes[..4].try_into().unwrap())) };
        if header(&mut file, offset)?.is_none_or(|(magic,rest)| !thin(magic,rest)) { return Ok(false); }
    }
    Ok(true)
}

pub(crate) fn verify_installed(simulator: &mut Simulator, config: &Launch, directory: &Path,
    previous: Option<&Path>, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
    let Some(prepared) = &config.prepared else { return Ok(()); };
    prepared.validate(config)?;
    let id = simulator.udid().to_owned();
    let output = simulator.command_with_environment(&["get_app_container", &id, &config.bundle, "app"],
        &BTreeMap::new(), deadline, cancelled)?;
    let container = PathBuf::from(std::str::from_utf8(&output).map_err(io::Error::other)?.trim());
    if !container.is_absolute() { return Err(bad("installed prepared app path is not absolute")); }
    let record = Installed { simulator: id.clone(), bundle: config.bundle.clone(),
        inventory: BundleInventory::capture(&container)?, container };
    prepared.validate_installed(&record.inventory)?;
    if let Some(previous) = previous
        && read_installed(previous, config, &id)?.as_ref() != Some(&record) {
        return Err(bad("prepared app installation changed across replacement"));
    }
    save_evidence(&directory.join("installed-app.json"), &record)
}
pub(crate) fn read_installed(directory: &Path, config: &Launch, simulator: &str) -> io::Result<Option<Installed>> {
    let Some(prepared) = &config.prepared else { return Ok(None); };
    prepared.validate(config)?;
    let path = directory.join("installed-app.json");
    if !std::fs::symlink_metadata(&path)?.is_file() { return Err(bad("linked or missing installed-app evidence")); }
    let record: Installed = load(&path)?.ok_or_else(|| bad("missing prepared app installation"))?;
    if record.simulator != simulator || record.bundle != config.bundle || !record.container.is_absolute() {
        return Err(bad("prepared app installation identity differs"));
    }
    prepared.validate_installed(&record.inventory)?;
    Ok(Some(record))
}

#[cfg(test)]
#[path = "ios_prepared_app_tests.rs"]
pub(crate) mod tests;
