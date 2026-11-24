//! Binary helper output is retained once, outside the JSON exchange.
use super::*;
use std::path::{Component, Path, PathBuf};
use frontcap_core::{canonical::sha256_bytes_hex, state::{save_atomic, save_atomic_bytes}};

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ArtifactExchange {
    pub exchange: Exchange,
    pub stdout_file: Option<Artifact>,
}
impl ArtifactExchange {
    pub fn read_stdout(&self, expected: &Path) -> io::Result<Vec<u8>> {
        let artifact = self.stdout_file.as_ref().ok_or_else(|| io::Error::other("helper stdout artifact absent"))?;
        if artifact.path != expected || !self.exchange.stdout.is_empty() || artifact.bytes > 16 * 1024 * 1024 {
            return Err(io::Error::other("helper stdout artifact binding differs"));
        }
        let metadata = artifact.path.symlink_metadata()?;
        if !metadata.file_type().is_file() || metadata.len() != artifact.bytes {
            return Err(io::Error::other("helper stdout artifact is linked or its size changed"));
        }
        let bytes = std::fs::read(&artifact.path)?;
        if bytes.len() as u64 != artifact.bytes || sha256_bytes_hex(&bytes) != artifact.sha256 {
            return Err(io::Error::other("helper stdout artifact digest changed"));
        }
        Ok(bytes)
    }
}

pub(crate) fn capture_artifact_recorded(command: &mut Command, deadline: Instant,
    cancelled: &AtomicBool, limits: CaptureLimits, evidence: &Path, name: &str) -> io::Result<ArtifactExchange> {
    let mut parts = Path::new(name).components();
    if !matches!(parts.next(), Some(Component::Normal(_))) || parts.next().is_some()
        || matches!(name, "exchange.json" | "command.json" | "process.json")
        || limits.stdout == 0 || limits.stdout > 16 * 1024 * 1024
        || limits.stderr == 0 || limits.stderr > 16 * 1024 * 1024 {
        return Err(io::Error::other("invalid helper stdout artifact name or limit"));
    }
    let budget = frontcap_core::write_budget::WriteBudget::from_environment()?;
    std::fs::create_dir(evidence)?;
    let evidence = evidence.canonicalize()?;
    let invocation = (command.get_program().to_string_lossy(),
        command.get_args().map(|a| a.to_string_lossy()).collect::<Vec<_>>());
    save_atomic(&evidence.join("command.json"), &invocation)?;
    let mut exchange = capture_started_limits(command, &[], deadline, cancelled, limits, |pid| {
        let identity = frontcap_core::process::ProcessIdentity::of(pid)?
            .ok_or_else(|| io::Error::other("command vanished before ownership publication"))?;
        save_atomic(&evidence.join("process.json"), &identity)
    });
    let bytes = std::mem::take(&mut exchange.stdout);
    let path = evidence.join(name);
    let publication = match &budget {
        Some(budget) => budget.save_atomic_bytes(frontcap_core::write_budget::Class::Ordinary, &path, &bytes),
        None => save_atomic_bytes(&path, &bytes),
    };
    let stdout_file = match publication {
        Ok(()) => Some(Artifact { path, bytes: bytes.len() as u64, sha256: sha256_bytes_hex(&bytes) }),
        Err(error) => { exchange.evidence_failure = Some(error.to_string()); None }
    };
    let mut result = ArtifactExchange { exchange, stdout_file };
    if let Err(error) = save_atomic(&evidence.join("exchange.json"), &result) {
        result.exchange.evidence_failure = Some(error.to_string());
    }
    Ok(result)
}

#[cfg(test)]
#[path = "process_artifact_tests.rs"]
mod tests;
