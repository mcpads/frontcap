//! File-backed diagnostic streams. Deserialization alone never opens referenced paths.
use super::Exchange;
use frontcap_core::{
    canonical::sha256_bytes_hex,
    state::{load, save_atomic_bytes},
    write_budget::{Class, WriteBudget},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::OpenOptions,
    io::{self, Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

const STREAM_LIMIT: u64 = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    file: String,
    bytes: u64,
    sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    gzip: Option<Compressed>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Compressed {
    bytes: u64,
    sha256: String,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Stream {
    Inline(Vec<u8>),
    File(File),
}

struct PreparedStream {
    record: Stream,
    payload: Option<(String, Vec<u8>)>,
}

impl Stream {
    fn prepare(name: &str, bytes: &[u8]) -> io::Result<PreparedStream> {
        if bytes.len() as u64 > STREAM_LIMIT {
            return Err(io::Error::other(
                "diagnostic stream exceeds evidence read limit",
            ));
        }
        // Empty streams need neither a file nor a descriptor.
        if bytes.is_empty() {
            return Ok(PreparedStream { record: Self::Inline(Vec::new()), payload: None });
        }
        let mut stored = None;
        if bytes.len() >= 64 * 1024 {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(bytes)?;
            let compressed = encoder.finish()?;
            if compressed.len() < bytes.len() {
                stored = Some(compressed);
            }
        }
        let filename = if stored.is_some() {
            format!("{name}.gz")
        } else {
            name.into()
        };
        let gzip = stored.as_ref().map(|data| Compressed {
            bytes: data.len() as u64,
            sha256: sha256_bytes_hex(data),
        });
        let data = stored.unwrap_or_else(|| bytes.to_vec());
        Ok(PreparedStream {
            record: Self::File(File { file: filename.clone(), bytes: bytes.len() as u64,
                sha256: sha256_bytes_hex(bytes), gzip }),
            payload: Some((filename, data)),
        })
    }

    fn read(self, root: &Path, name: &str) -> io::Result<Vec<u8>> {
        match self {
            Self::Inline(bytes) => Ok(bytes), // Preserve original pre-file-backed records.
            Self::File(record) => {
                let expected_name = if record.gzip.is_some() {
                    format!("{name}.gz")
                } else {
                    name.into()
                };
                let stored_bytes = record.gzip.as_ref().map_or(record.bytes, |gzip| gzip.bytes);
                let stored_digest = record
                    .gzip
                    .as_ref()
                    .map_or(&record.sha256, |gzip| &gzip.sha256);
                if record.file != expected_name
                    || record.bytes > STREAM_LIMIT
                    || stored_bytes > STREAM_LIMIT
                    || root.symlink_metadata()?.file_type().is_symlink()
                {
                    return Err(io::Error::other("diagnostic stream binding differs"));
                }
                let file = OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                    .open(root.join(&record.file))?;
                let metadata = file.metadata()?;
                if !metadata.is_file() || metadata.len() != stored_bytes {
                    return Err(io::Error::other("diagnostic stream type or size changed"));
                }
                let mut bytes = Vec::new();
                file.take(stored_bytes + 1).read_to_end(&mut bytes)?;
                if bytes.len() as u64 != stored_bytes || sha256_bytes_hex(&bytes) != *stored_digest
                {
                    return Err(io::Error::other("stored diagnostic stream digest changed"));
                }
                if record.gzip.is_some() {
                    let mut decoded = Vec::new();
                    flate2::read::GzDecoder::new(bytes.as_slice())
                        .take(record.bytes + 1)
                        .read_to_end(&mut decoded)?;
                    bytes = decoded;
                }
                if bytes.len() as u64 != record.bytes || sha256_bytes_hex(&bytes) != record.sha256 {
                    return Err(io::Error::other("diagnostic stream digest changed"));
                }
                Ok(bytes)
            }
        }
    }
}

/// Publish streams before the manifest. Callers retain the in-memory result on any error.
#[cfg(test)]
fn save_exchange<R: Serialize>(path: &Path, result: &Exchange<R>) -> io::Result<()> {
    save_exchange_with_budget(path, result, None)
}

pub(crate) fn save_exchange_with_budget<R: Serialize>(path: &Path, result: &Exchange<R>, budget: Option<&WriteBudget>) -> io::Result<()> {
    save_exchange_class(path, result, budget, super::write_class())
}

pub(crate) fn save_exchange_class<R: Serialize>(path: &Path, result: &Exchange<R>, budget: Option<&WriteBudget>, class: Class) -> io::Result<()> {
    let root = path
        .parent()
        .ok_or_else(|| io::Error::other("exchange parent missing"))?;
    let stdout = Stream::prepare("stdout.bin", &result.stdout)?;
    let stderr = Stream::prepare("stderr.bin", &result.stderr)?;
    let stored = Exchange {
        pid: result.pid,
        stdout: stdout.record,
        stderr: stderr.record,
        reply: result.reply.as_ref(),
        failure: result.failure.clone(),
        cleanup_failure: result.cleanup_failure.clone(),
        exit_code: result.exit_code,
        signal_error: result.signal_error.clone(),
        evidence_failure: result.evidence_failure.clone(),
    };
    let manifest = frontcap_core::canonical::canonical_of(&stored);
    let payloads = [stdout.payload, stderr.payload];
    if let Some(budget) = budget {
        let bytes = manifest.len() as u64 + payloads.iter().flatten().map(|(_, data)| data.len() as u64).sum::<u64>();
        budget.charge(class, bytes)?;
    }
    for (name, data) in payloads.into_iter().flatten() {
        save_atomic_bytes(&root.join(name), &data)?;
    }
    save_atomic_bytes(path, manifest.as_bytes())
}

/// Read the original exchange and verify sibling stream files without executing a helper.
/// Numeric-array records remain readable; paths in new descriptors must match fixed filenames.
pub fn load_exchange<R: DeserializeOwned>(path: &Path) -> io::Result<Option<Exchange<R>>> {
    let Some(stored): Option<Exchange<R, Stream>> = load(path)? else {
        return Ok(None);
    };
    let root = path
        .parent()
        .ok_or_else(|| io::Error::other("exchange parent missing"))?;
    Ok(Some(Exchange {
        pid: stored.pid,
        stdout: stored.stdout.read(root, "stdout.bin")?,
        stderr: stored.stderr.read(root, "stderr.bin")?,
        reply: stored.reply,
        failure: stored.failure,
        cleanup_failure: stored.cleanup_failure,
        exit_code: stored.exit_code,
        signal_error: stored.signal_error,
        evidence_failure: stored.evidence_failure,
    }))
}

/// Follow the hashed manifest's stream bindings when checking a receipt or retirement proof.
pub(crate) fn validate_exchange_files(path: &Path) -> io::Result<()> {
    if path.file_name().is_some_and(|name| name == "exchange.json") {
        // Binary captures have their own wrapper and separately bound artifact proof.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Record {
            Diagnostic(Box<Exchange<serde::de::IgnoredAny, Stream>>),
            Artifact(Box<super::ArtifactExchange>),
        }
        match load::<Record>(path)?.ok_or_else(|| io::Error::other("helper exchange missing"))? {
            Record::Diagnostic(record) => {
                let root = path.parent().unwrap();
                record.stdout.read(root, "stdout.bin")?;
                record.stderr.read(root, "stderr.bin")?;
            }
            Record::Artifact(record) => {
                let _ = record;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "process_evidence_tests.rs"]
mod tests;
