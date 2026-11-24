//! Small terminal records routed to the recorder filesystem, not a reserved-capacity guarantee.
use frontcap_core::{canonical::sha256_bytes_hex, state::save_atomic};
use serde::{Deserialize, Serialize};
use std::{fs, io::{self, Read, Write}, os::unix::fs::{MetadataExt, OpenOptionsExt}, path::{Path, PathBuf}};

pub const ROOT_ENV: &str = "FRONTCAP_NATIVE_CONTROL_ROOT";
pub const RESULT_LIMIT: u64 = 64 * 1024;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Location { directory: PathBuf, device: u64, inode: u64 }
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration { session: PathBuf, location: Location }
#[derive(Debug, Serialize)]
pub struct Inspection {
    pub directory: PathBuf,
    pub evidence_only: bool,
    pub worker_lifetime_verified: bool,
    pub result: Option<crate::flutter_session::ResultRecord>,
    pub error_count: usize,
    pub details_truncated: bool,
}

/// Read registered terminal diagnostics without accessing the target filesystem or adopting a worker.
/// This deliberately does not establish lifetime, retirement, or request completion.
pub fn inspect_root(root: &Path) -> io::Result<Option<Inspection>> {
    let path = root.join("flutter-worker-control.json");
    let Some(registered) = read_bounded::<Registration>(&path, 16 * 1024)? else { return Ok(None); };
    if !registered.session.is_absolute() || registered.location.directory != root.canonicalize()?.join(
        format!("flutter-control-{}", sha256_bytes_hex(registered.session.as_os_str().as_encoded_bytes()))) {
        return Err(io::Error::other("worker control registration belongs to another runtime"));
    }
    let result = checked_result(&registered.location)?;
    let mut result: Option<crate::flutter_session::ResultRecord> = read_bounded(&result, RESULT_LIMIT)?;
    let mut details_truncated = false;
    let error_count = result.as_ref().map_or(0, |value| value.retirement.errors.len());
    if let Some(value) = &mut result {
        details_truncated = value.retirement.errors.len() > 4;
        value.retirement.errors.truncate(4);
        for text in value.retirement.errors.iter_mut().chain(value.error.iter_mut())
            .chain(value.retirement.signal_error.iter_mut()) {
            let end = text.char_indices().nth(512).map(|(index, _)| index);
            if let Some(end) = end { text.truncate(end); details_truncated = true; }
        }
    }
    Ok(Some(Inspection { directory: registered.location.directory, evidence_only: true,
        worker_lifetime_verified: false, result, error_count, details_truncated }))
}

pub(crate) fn create(session: &Path) -> io::Result<Option<PathBuf>> {
    let Some(root) = std::env::var_os(ROOT_ENV) else { return Ok(None); };
    create_in(session, Path::new(&root)).map(Some)
}
fn create_in(session: &Path, root: &Path) -> io::Result<PathBuf> {
    let metadata = fs::symlink_metadata(root)?;
    if !root.is_absolute() || !metadata.is_dir() {
        return Err(io::Error::other("worker control root must be an absolute non-linked directory"));
    }
    let root = root.canonicalize()?;
    let registry_path = root.join("flutter-worker-control.json");
    if registry_path.try_exists()? { return Err(io::Error::other("worker control already registered")); }
    let session = session.canonicalize()?;
    let name = sha256_bytes_hex(session.as_os_str().as_encoded_bytes());
    let directory = root.join(format!("flutter-control-{name}"));
    fs::create_dir(&directory)?;
    let metadata = fs::symlink_metadata(&directory)?;
    let location = Location { directory: directory.clone(), device: metadata.dev(), inode: metadata.ino() };
    let registration = Registration { session: session.clone(), location };
    let mut registry = fs::File::create_new(registry_path)?;
    registry.write_all(&serde_json::to_vec(&registration)?)?;
    registry.sync_all()?;
    save_atomic(&session.join("worker-control.json"), &registration.location)?;
    Ok(directory)
}
pub(crate) fn result_path(session: &Path) -> io::Result<PathBuf> {
    let Some(location) = read_bounded::<Location>(&session.join("worker-control.json"), 16 * 1024)? else {
        return Ok(session.join("worker/result.json"));
    };
    checked_result(&location)
}
pub(crate) fn read_result(session: &Path) -> io::Result<Option<crate::flutter_session::ResultRecord>> {
    read_bounded(&result_path(session)?, RESULT_LIMIT)
}
fn read_bounded<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> io::Result<Option<T>> {
    let file = match fs::OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > limit {
        return Err(io::Error::other("invalid or oversized worker control record"));
    }
    let mut data = Vec::new();
    file.take(limit + 1).read_to_end(&mut data)?;
    if data.len() as u64 > limit { return Err(io::Error::other("worker control record grew beyond limit")); }
    serde_json::from_slice(&data).map(Some).map_err(io::Error::other)
}
fn checked_result(location: &Location) -> io::Result<PathBuf> {
    let metadata = fs::symlink_metadata(&location.directory)?;
    if !location.directory.is_absolute() || !metadata.is_dir()
        || metadata.dev() != location.device || metadata.ino() != location.inode {
        return Err(io::Error::other("worker control directory identity changed"));
    }
    let result = location.directory.join("result.json");
    match fs::symlink_metadata(&result) {
        Ok(metadata) if !metadata.is_file() || metadata.len() > RESULT_LIMIT =>
            Err(io::Error::other("invalid worker control result file")),
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(result),
    }
}

#[cfg(test)]
#[path = "worker_control_tests.rs"]
mod tests;
