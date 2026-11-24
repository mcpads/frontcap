//! Same-process URL delivery and immutable acceptance evidence for ordinary iOS hosts.
use super::*;
use frontcap_core::{canonical::sha256_bytes_hex, native::{external_entry::{Observation, validate_url}, ios_host::UrlAcceptance}};
use std::collections::BTreeMap;

#[derive(PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent { owner: Owner, url: String }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    acceptance: UrlAcceptance,
    admission_sha256: String,
    commands: BTreeMap<String, String>,
}
fn unlinked(root: &Path, path: &Path) -> io::Result<()> {
    let mut cursor = root.to_owned();
    for part in path.strip_prefix(root).map_err(io::Error::other)?.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Err(io::Error::other("invalid iOS URL evidence path"));
        }
        cursor.push(part);
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(io::Error::other("linked iOS URL evidence")),
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => break,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
/// Read original evidence after retirement or replacement, without consulting the live simulator.
pub(crate) fn read_url(root: &Path, host: &Path, id: &RequestId, owner: &Owner,
    url: &str) -> io::Result<Option<UrlAcceptance>> {
    owner.validate().map_err(io::Error::other)?;
    validate_url(url).map_err(io::Error::other)?;
    let directory = host.join("urls").join(id.as_str());
    for name in ["intent.json", "receipt.json", "admission.json"] { unlinked(root, &directory.join(name))?; }
    let Some(intent): Option<Intent> = load(&directory.join("intent.json"))? else { return Ok(None); };
    if intent != (Intent { owner: owner.clone(), url: url.into() }) {
        return Err(io::Error::other("iOS host URL request conflicts"));
    }
    let Some(proof): Option<Proof> = load(&directory.join("receipt.json"))? else { return Ok(None); };
    if proof.acceptance.owner != *owner || proof.acceptance.url != url || proof.commands.is_empty()
        || sha256_bytes_hex(&std::fs::read(directory.join("admission.json"))?) != proof.admission_sha256
    { return Err(io::Error::other("iOS host URL proof differs from intent")); }
    for (name, digest) in proof.commands {
        let parts: Vec<_> = name.split('/').collect();
        if parts.len() != 2 || !parts[0].strip_prefix("command-").is_some_and(|n|
            n.parse::<u64>().is_ok_and(|value| value > 0 && value.to_string() == n))
            || !["command.json", "exchange.json"].contains(&parts[1]) {
            return Err(io::Error::other("invalid iOS host URL command path"));
        }
        // The simulator owner survives app replacement; its evidence stays at the initial root.
        let path = root.join("target/simulator").join(name); unlinked(root, &path)?;
        if sha256_bytes_hex(&std::fs::read(&path)?) != digest {
            return Err(io::Error::other("iOS host URL command evidence changed"));
        }
        crate::process::validate_exchange_files(&path)?;
    }
    Ok(Some(proof.acceptance))
}
impl Target {
    pub fn dispatch_url(&mut self, id: &RequestId, owner: &Owner, url: &str,
        deadline: Instant, cancelled: &AtomicBool) -> Result<UrlAcceptance, AcquisitionError> {
        owner.validate().map_err(io::Error::other)?;
        validate_url(url).map_err(io::Error::other)?;
        if owner != &self.owner || self.retired.is_some() {
            return Err(io::Error::other("iOS host URL owner unavailable or changed").into());
        }
        let directory = self.directory.join("urls").join(id.as_str());
        if directory.exists() { return Err(io::Error::other("iOS host URL attempt already exists").into()); }
        let (simulator_root, first) = {
            let (path, number) = self.app.simulator_mut()?.command_cursor(); (path.to_owned(), number + 1)
        };
        self.app.ensure_running(deadline, cancelled)?;
        let bundle = self.app.bundle().to_owned();
        let admission = crate::ios_external_entry::admit(self.app.simulator_mut()?, &bundle, url, deadline, cancelled)?;
        std::fs::create_dir_all(directory.parent().unwrap())?;
        std::fs::create_dir(&directory)?;
        // After intent admission, any uncertainty fences the owner. There is no retry or fallback.
        let work = (|| -> io::Result<UrlAcceptance> {
            save_evidence(&directory.join("admission.json"), &admission)?;
            save_evidence(&directory.join("intent.json"), &Intent { owner: owner.clone(), url: url.into() })?;
            self.input.capture_basis = None;
            crate::ios_external_entry::open(self.app.simulator_mut()?, url, deadline, cancelled)?;
            self.app.ensure_running(deadline, cancelled)?;
            let acceptance = UrlAcceptance { owner: owner.clone(), url: url.into(),
                confirmation: Observation::Unobserved, destination: Observation::Unobserved };
            let last = self.app.simulator_mut()?.command_cursor().1;
            let mut commands = BTreeMap::new();
            for number in first..=last {
                for file in ["command.json", "exchange.json"] {
                    let name = format!("command-{number}/{file}");
                    commands.insert(name.clone(), sha256_bytes_hex(&std::fs::read(simulator_root.join(name))?));
                }
            }
            save_evidence(&directory.join("receipt.json"), &Proof { acceptance: acceptance.clone(), commands,
                admission_sha256: sha256_bytes_hex(&std::fs::read(directory.join("admission.json"))?) })?;
            Ok(acceptance)
        })();
        work.map_err(|error| AcquisitionError { cause: error.to_string(), retirement: Some(self.stop()) })
    }
}

#[cfg(test)]
#[path = "ios_host_entry_tests.rs"]
mod tests;
