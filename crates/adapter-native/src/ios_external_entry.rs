//! Admit URL delivery using the installed apps of an already-owned Simulator.
use crate::simulator::Simulator;
use frontcap_core::canonical::sha256_bytes_hex;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs::File, io::{self, Read}, path::PathBuf,
    sync::atomic::{AtomicBool, Ordering}, time::Instant};

const PLIST_LIMIT: usize = 4 * 1024 * 1024;
const INVENTORY_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Deserialize)]
struct InstalledApp {
    #[serde(rename = "CFBundleIdentifier")]
    bundle: String,
    #[serde(rename = "Path")]
    path: PathBuf,
}
#[derive(Deserialize)]
struct AppInfo {
    #[serde(rename = "CFBundleIdentifier")]
    bundle: String,
    #[serde(default, rename = "CFBundleURLTypes")]
    url_types: Vec<UrlType>,
}
#[derive(Deserialize)]
struct UrlType {
    #[serde(default, rename = "CFBundleURLSchemes")]
    schemes: Vec<String>,
}
#[derive(Serialize)]
pub(crate) struct Admission {
    bundle: String,
    scheme: String,
    inventory_sha256: String,
    apps: Vec<AppDeclaration>,
}
#[derive(Serialize)]
struct AppDeclaration {
    bundle: String,
    info_plist: PathBuf,
    sha256: String,
    schemes: Vec<String>,
}

pub(crate) fn admit(simulator: &mut Simulator, bundle: &str, url: &str,
    deadline: Instant, cancelled: &AtomicBool) -> io::Result<Admission> {
    simulator.ensure_booted(deadline, cancelled)?;
    let udid = simulator.udid().to_owned();
    let inventory = simulator.command_with_environment(&["listapps", &udid],
        &BTreeMap::new(), deadline, cancelled)?;
    inspect(&inventory, bundle, url, || {
        if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
            Err(io::Error::other("iOS URL admission cancelled or expired"))
        } else { Ok(()) }
    })
}

pub(crate) fn inspect(inventory: &[u8], bundle: &str, url: &str,
    check: impl Fn() -> io::Result<()>) -> io::Result<Admission> {
    let scheme = url.split_once(':').ok_or_else(|| io::Error::other("missing URL scheme"))?.0;
    // This path admits declared custom schemes, not universal links or OS/default-app routing.
    // A consumer declaration cannot establish ownership of these system-handled URL families.
    if matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "file" | "data"
        | "javascript" | "tel" | "telprompt" | "sms" | "mailto" | "facetime" | "facetime-audio") {
        return Err(io::Error::other("system and universal URL schemes require separate owner resolution"));
    }
    if inventory.len() > PLIST_LIMIT { return Err(io::Error::other("app inventory too large")); }
    // simctl listapps emits an ASCII plist; installed Info.plist may be binary or XML.
    let installed: BTreeMap<String, InstalledApp> = plist::from_reader_ascii(inventory).map_err(io::Error::other)?;
    if installed.is_empty() || installed.len() > 1024 || !installed.contains_key(bundle) {
        return Err(io::Error::other("owned app missing from bounded installed inventory"));
    }
    let mut apps = Vec::new();
    let mut handlers = Vec::new();
    let mut total = 0;
    for (key, app) in installed {
        check()?;
        if key != app.bundle || !app.path.is_absolute() || app.path.extension().is_none_or(|s| s != "app") {
            return Err(io::Error::other("installed app identity/path mismatch"));
        }
        let info_plist = app.path.join("Info.plist");
        let mut bytes = Vec::new();
        File::open(&info_plist)?.take((PLIST_LIMIT + 1) as u64).read_to_end(&mut bytes)?;
        total += bytes.len();
        if bytes.len() > PLIST_LIMIT || total > INVENTORY_LIMIT {
            return Err(io::Error::other("installed URL declarations exceed size limit"));
        }
        let info: AppInfo = plist::from_bytes(&bytes).map_err(io::Error::other)?;
        if info.bundle != key { return Err(io::Error::other("installed Info.plist identity mismatch")); }
        let schemes: Vec<_> = info.url_types.into_iter().flat_map(|t| t.schemes).collect();
        if schemes.iter().any(|s| s.eq_ignore_ascii_case(scheme)) { handlers.push(key.clone()); }
        apps.push(AppDeclaration { bundle: key, info_plist, sha256: sha256_bytes_hex(&bytes), schemes });
    }
    check()?;
    if handlers != [bundle] {
        return Err(io::Error::other("URL scheme must have exactly one installed handler: the owned app"));
    }
    Ok(Admission { bundle: bundle.into(), scheme: scheme.into(), inventory_sha256: sha256_bytes_hex(inventory), apps })
}

pub(crate) fn open(simulator: &mut Simulator, url: &str, deadline: Instant,
    cancelled: &AtomicBool) -> io::Result<()> {
    let udid = simulator.udid().to_owned();
    simulator.command_with_environment(&["openurl", &udid, url], &BTreeMap::new(), deadline, cancelled)?;
    // This is OS acceptance only. No app delivery, prompt resolution or new owner is inferred.
    Ok(())
}

#[cfg(test)]
#[path = "ios_external_entry_tests.rs"]
mod tests;
