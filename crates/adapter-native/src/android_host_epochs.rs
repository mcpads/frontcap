//! Read-only Android host process lineage. Retained evidence never grants live ownership.
use super::*;
use frontcap_core::native::android_webview::AppProcess;
use std::collections::HashSet;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    pub owner: AppProcess,
    pub epoch: Option<RequestId>,
}
pub(super) struct Epoch {
    pub ready: host::Ready,
    pub root: PathBuf,
}
fn unlinked(directory: &Path, path: &Path) -> io::Result<()> {
    let mut cursor = directory.to_owned();
    for part in path.strip_prefix(directory).map_err(io::Error::other)?.components() {
        if !matches!(part, std::path::Component::Normal(_)) {
            return Err(io::Error::other("invalid Android host evidence path"));
        }
        cursor.push(part);
        match std::fs::symlink_metadata(&cursor) {
            Ok(meta) if meta.file_type().is_symlink() => return Err(io::Error::other("linked Android host epoch evidence")),
            Ok(_) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
fn required<T: serde::de::DeserializeOwned>(directory: &Path, path: &Path) -> io::Result<T> {
    unlinked(directory, path)?;
    load(path)?.ok_or_else(|| io::Error::other("Android host epoch evidence unavailable"))
}
pub(super) fn binding(directory: &Path, id: &RequestId, initial: &host::Ready) -> io::Result<Binding> {
    let path = directory.join("requests").join(id.as_str()).join("host-binding.json");
    unlinked(directory, &path)?;
    // Older single-process sessions had no binding file. Such records can only read the initial
    // owner; every replacement chain below requires an explicit, immutable restart binding.
    Ok(load(&path)?.unwrap_or_else(|| Binding { owner: initial.owner.clone(), epoch: None }))
}
pub(super) fn resolve(directory: &Path, config: &Config, initial: &host::Ready,
    epoch: Option<&RequestId>) -> io::Result<Option<Epoch>> {
    config.validate_host_ready(initial)?;
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut next = epoch.cloned();
    while let Some(id) = next {
        if chain.len() == 64 || !seen.insert(id.clone()) {
            return Err(io::Error::other("cyclic or exhausted Android host epoch lineage"));
        }
        let request = directory.join("requests").join(id.as_str());
        let binding: Binding = required(directory, &request.join("host-binding.json"))?;
        let operation: Operation = required(directory, &request.join("operation.json"))?;
        let url = match operation {
            Operation::Host { request: host::Operation::Restart { previous, url } } if *previous == binding.owner => url,
            _ => return Err(io::Error::other("Android host epoch lacks original restart binding")),
        };
        next = binding.epoch.clone(); chain.push((id, binding, url));
    }
    let target = directory.join("target");
    let mut state = Epoch { ready: initial.clone(), root: target.clone() };
    for (id, binding, url) in chain.into_iter().rev() {
        if binding.owner != state.ready.owner {
            return Err(io::Error::other("Android restart binding differs from previous epoch"));
        }
        let root = target.join("host-restarts").join(id.as_str());
        for name in ["intent.json", "receipt.json", "terminated.json", "app-process.json",
            "input-retired.json", "contact-driver/owner.json", "contact-driver/driver.jar"] {
            unlinked(directory, &root.join(name))?;
        }
        let Some(replacement) = crate::device::read_host_replacement(&target, &id, &state.ready.owner, url.as_deref())? else {
            return Ok(None);
        };
        if replacement.previous != state.ready {
            return Err(io::Error::other("Android replacement differs from original input owner"));
        }
        config.validate_host_ready(&replacement.ready)?;
        state = Epoch { ready: replacement.ready, root };
    }
    Ok(Some(state))
}

#[cfg(test)]
#[path = "android_host_epochs_tests.rs"]
mod tests;
