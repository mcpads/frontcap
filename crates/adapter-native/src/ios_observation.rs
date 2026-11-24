//! Verify XCTest observation bytes independently of the Python bridge reply.
use frontcap_core::native::{
    ios_ui::{UiOutcome, valid_observation_artifacts},
    observation::Artifact,
};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

pub fn verify(outcome: &UiOutcome, directory: &Path) -> io::Result<()> {
    let (UiOutcome::SystemAlertsObserved { artifacts, .. } | UiOutcome::ObservedTargets { artifacts, .. } | UiOutcome::ObservationUnavailable { artifacts,.. }) =
        outcome
    else {
        return Err(io::Error::other("not an iOS observation"));
    };
    if matches!(outcome,UiOutcome::ObservationUnavailable { reason:frontcap_core::native::ios_ui::UnavailableReason::ScrollInMotion,.. }) && artifacts.is_empty() { return Ok(()); }
    if !valid_observation_artifacts(artifacts) {
        return Err(io::Error::other("invalid observation inventory"));
    }
    for Artifact {
        path,
        bytes,
        sha256,
    } in artifacts
    {
        let relative = Path::new(path);
        let mut current = directory.to_path_buf();
        for part in relative.components() {
            current.push(part);
            if std::fs::symlink_metadata(&current)?
                .file_type()
                .is_symlink()
            {
                return Err(io::Error::other("linked observation artifact"));
            }
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&current)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("not an observation file"));
        }
        let mut raw = Vec::new();
        file.take(16 * 1024 * 1024 + 1).read_to_end(&mut raw)?;
        let digest: String = Sha256::digest(&raw)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if raw.len() as u64 != *bytes || digest != *sha256 {
            return Err(io::Error::other("observation bytes changed"));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "ios_observation_tests.rs"]
mod tests;
