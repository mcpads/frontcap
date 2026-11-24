//! Warm URL delivery to an exact ordinary host process, without a consumer callback.
use super::*;
use frontcap_core::{ids::RequestId, native::{android_host::UrlAcceptance,
    android_webview::AppProcess, external_entry::{Observation, validate_url}}, state::load};
use std::collections::BTreeMap;

#[derive(Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent { owner: AppProcess, url: String }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof { acceptance: UrlAcceptance, commands: BTreeMap<String, String> }

pub(super) fn unlinked(root: &Path, path: &Path) -> io::Result<()> {
    let relative = path.strip_prefix(root).map_err(io::Error::other)?;
    let mut current = root.to_owned();
    for part in relative.components() {
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(io::Error::other("linked Android URL evidence")),
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Original evidence only: never dispatch, probe the device, or adopt a replacement process.
pub fn read_host_url(root: &Path, epoch: &Path, id: &RequestId, owner: &AppProcess,
    url: &str) -> io::Result<Option<UrlAcceptance>> {
    validate_url(url).map_err(io::Error::other)?;
    let directory = epoch.join("host-urls").join(id.as_str());
    let intent_path = directory.join("intent.json");
    let proof_path = directory.join("receipt.json");
    unlinked(root, &intent_path)?; unlinked(root, &proof_path)?;
    let Some(intent): Option<Intent> = load(&intent_path)? else { return Ok(None); };
    if intent != (Intent { owner: owner.clone(), url: url.into() }) {
        return Err(io::Error::other("Android host URL request conflicts"));
    }
    let Some(proof): Option<Proof> = load(&proof_path)? else { return Ok(None); };
    if proof.acceptance.owner != *owner || proof.acceptance.url != url || proof.commands.is_empty() {
        return Err(io::Error::other("Android host URL proof differs from intent"));
    }
    for (name, digest) in proof.commands {
        let parts: Vec<_> = name.split('/').collect();
        if parts.len() != 2 || !parts[0].strip_prefix("command-").is_some_and(|n|
            n.parse::<u64>().is_ok_and(|value| value > 0 && value.to_string() == n))
            || !["command.json", "exchange.json"].contains(&parts[1]) {
            return Err(io::Error::other("invalid Android host URL evidence path"));
        }
        let path = root.join(name); unlinked(root, &path)?;
        if frontcap_core::canonical::sha256_bytes_hex(&std::fs::read(&path)?) != digest {
            return Err(io::Error::other("Android host URL command evidence changed"));
        }
        crate::process::validate_exchange_files(&path)?;
    }
    Ok(Some(proof.acceptance))
}

impl Device {
    pub fn dispatch_host_url(&mut self, id: &RequestId, owner: &AppProcess, url: &str,
        deadline: Instant, cancelled: &AtomicBool) -> Result<UrlAcceptance, AcquisitionError> {
        let epoch = self.host_evidence_directory().to_owned();
        if let Some(accepted) = read_host_url(&self.directory, &epoch, id, owner, url)? { return Ok(accepted); }
        let directory = epoch.join("host-urls").join(id.as_str());
        if directory.exists() || self.config.host_app.is_none() || self.retired.is_some()
            || !self.unretired_commands.is_empty() {
            return Err(io::Error::other("Android host URL unresolved or owner unavailable").into());
        }
        self.require_host_foreground()?;
        let first = self.command_number + 1;
        self.verify_app_process(owner, deadline, cancelled)?;
        std::fs::create_dir_all(directory.parent().unwrap())?;
        std::fs::create_dir(&directory)?;
        // Once intent exists, failure retires the owner. Never retry a possibly delivered URL.
        let result = (|| -> io::Result<UrlAcceptance> {
            save_atomic(&directory.join("intent.json"), &Intent { owner: owner.clone(), url: url.into() })?;
            self.invalidate_contact_capture();
            let args = crate::external_entry::android_arguments(&owner.package, url);
            let output = self.adb(&args.iter().map(String::as_str).collect::<Vec<_>>(), deadline, cancelled, false)?
                .ok_or_else(|| io::Error::other("missing Android URL response"))?;
            crate::external_entry::android_accepted(&output, &owner.package, false)?;
            self.verify_app_process(owner, deadline, cancelled)?;
            let acceptance = UrlAcceptance { owner: owner.clone(), url: url.into(),
                confirmation: Observation::Unobserved, destination: Observation::Unobserved };
            let mut commands = BTreeMap::new();
            for number in first..=self.command_number {
                for file in ["command.json", "exchange.json"] {
                    let name = format!("command-{number}/{file}");
                    commands.insert(name.clone(), frontcap_core::canonical::sha256_bytes_hex(
                        &std::fs::read(self.directory.join(name))?));
                }
            }
            save_atomic(&directory.join("receipt.json"), &Proof { acceptance: acceptance.clone(), commands })?;
            Ok(acceptance)
        })();
        result.map_err(|error| AcquisitionError { cause: error.to_string(), retirement: Some(self.stop()) })
    }
}

#[cfg(test)]
#[path = "device_host_entry_tests.rs"]
mod tests;
