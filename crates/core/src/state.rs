use crate::canonical::canonical_of;
use serde::{Serialize, de::DeserializeOwned};
use std::io::Write;
use std::path::Path;

/// Replaces `path` with `value` whole or not at all: written to a temporary file of its own beside it
/// (`.<name>.tmp-<pid>-<n>`, created exclusively, so concurrent saves never share one), synced, renamed
/// over it, and the directory synced. Concurrent saves of one path leave one of them, complete.
/// The number of this process's next temporary name.
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn save_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    save_atomic_bytes(path, canonical_of(value).as_bytes())
}

/// Atomically preserves arbitrary evidence bytes, including incomplete JSONL tails.
pub fn save_atomic_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("state");
    // A name a crashed process (whose pid this one reuses) left is skipped.
    let (tmp, mut f) = loop {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = dir.join(format!(".{name}.tmp-{}-{n}", std::process::id()));
        match std::fs::File::options().write(true).create_new(true).open(&tmp) {
            Ok(f) => break (tmp, f),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    };
    let written: std::io::Result<()> = (|| {
        #[cfg(feature = "fault-injection")]
        if storage_fault_matches(path, "after_partial_write") || storage_fault_matches(path, "no_space_after_partial_write") {
            f.write_all(&bytes[..bytes.len() / 2])?;
            storage_fault(path, "after_partial_write");
            return Err(std::io::Error::from_raw_os_error(28)); // ENOSPC on supported Unix hosts.
        }
        f.write_all(bytes)?;
        f.sync_all()?;
        #[cfg(feature = "fault-injection")]
        storage_fault(path, "after_sync");
        std::fs::rename(&tmp, path)?;
        #[cfg(feature = "fault-injection")]
        storage_fault(path, "after_rename");
        Ok(())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    #[cfg(unix)]
    std::fs::File::open(dir)?.sync_all()?;
    Ok(())
}

#[cfg(feature = "fault-injection")]
fn storage_fault_matches(path: &Path, point: &str) -> bool {
    std::env::var("FRONTCAP_STATE_FAULT").ok().is_some_and(|value| {
        path.file_name().and_then(|n| n.to_str()).is_some_and(|name| value == format!("{name}@{point}"))
    })
}

#[cfg(feature = "fault-injection")]
fn storage_fault(path: &Path, point: &str) {
    if storage_fault_matches(path, point) { std::process::abort(); }
}

/// Recognizes unpublished atomic-write artifacts without treating them as their destination file.
pub fn is_atomic_temporary(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else { return false };
    let Some((stem, suffix)) = name.rsplit_once(".tmp-") else { return false };
    let Some((pid, counter)) = suffix.split_once('-') else { return false };
    stem.starts_with('.') && stem.len() > 1 && !pid.is_empty() && !counter.is_empty()
        && pid.bytes().all(|c| c.is_ascii_digit()) && counter.bytes().all(|c| c.is_ascii_digit())
}

pub fn load<T: DeserializeOwned>(path: &Path) -> std::io::Result<Option<T>> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
#[path = "state_tests.rs"]
mod tests;
