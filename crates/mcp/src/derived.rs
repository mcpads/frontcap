//! Publish derived artifacts after their immutable bytes and provenance, then atomically update the
//! catalog under a process-shared lock. A killed publisher may leave uncatalogued files, never a
//! catalog entry claiming an unpublished artifact.
use crate::tools::ToolError;
use frontcap_core::{canonical::{canonical_of, sha256_bytes_hex}, state::{save_atomic, save_atomic_bytes}};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::{Component, Path, PathBuf}};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    pub kind: String,
    pub file: String,
    pub sha256: String,
    pub manifest: String,
    pub manifest_sha256: String,
}

type Index = BTreeMap<String, Artifact>;

fn invalid(message: impl Into<String>) -> ToolError {
    ToolError::new("invalid_artifact",message,"inspect derived/index.json and its artifacts; source evidence is independent")
}

fn root(bundle: &Path) -> Result<PathBuf, ToolError> {
    let root = bundle.canonicalize().map_err(|e| invalid(e.to_string()))?;
    let dir = root.join("derived");
    std::fs::create_dir_all(&dir).map_err(|e| invalid(e.to_string()))?;
    if dir.canonicalize().map_err(|e| invalid(e.to_string()))? != dir {
        return Err(invalid("derived must be a directory inside the bundle, not a redirect"));
    }
    Ok(root)
}

fn path(root: &Path, name: &str) -> Result<PathBuf, ToolError> {
    let relative = Path::new(name);
    if relative.components().count()!=2 || !relative.components().all(|c| matches!(c,Component::Normal(_))) || !relative.starts_with("derived") {
        return Err(invalid("artifact paths must name files directly inside derived/"));
    }
    let path = root.join(relative);
    match std::fs::symlink_metadata(&path) {
        Ok(m) if !m.is_file() => return Err(invalid("artifact path is not a regular file")),
        Err(e) if e.kind()!=std::io::ErrorKind::NotFound => return Err(invalid(e.to_string())),
        _ => {}
    }
    Ok(path)
}

fn load(root: &Path) -> Result<Index, ToolError> {
    let p=path(root,"derived/index.json")?;
    match std::fs::read(p) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e|invalid(format!("invalid artifact index: {e}"))),
        Err(e) if e.kind()==std::io::ErrorKind::NotFound => Ok(Index::new()),
        Err(e) => Err(invalid(e.to_string())),
    }
}

pub(crate) fn publish<T: Serialize>(bundle: &Path, kind: &str, file: &str, bytes: &[u8], manifest: &str, metadata: &T) -> Result<(),ToolError> {
    let root=root(bundle)?;
    let lock=std::fs::OpenOptions::new().create(true).truncate(false).write(true)
        .open(path(&root,"derived/.index.lock")?).map_err(|e|invalid(e.to_string()))?;
    lock.lock().map_err(|e|invalid(e.to_string()))?;
    let mut entries=load(&root)?;
    let metadata=canonical_of(metadata);
    let entry=Artifact { kind:kind.into(),file:file.into(),sha256:sha256_bytes_hex(bytes),manifest:manifest.into(),manifest_sha256:sha256_bytes_hex(metadata.as_bytes()) };
    // Do not overwrite a different object at a retained identity, even if the catalog was lost.
    for (name,content) in [(file,bytes),(manifest,metadata.as_bytes())] {
        let p=path(&root,name)?;
        match std::fs::read(&p) {
            Ok(existing) if existing!=content => return Err(invalid(format!("artifact identity collision or corruption: {name}"))),
            Ok(_) => {},
            Err(e) if e.kind()==std::io::ErrorKind::NotFound => save_atomic_bytes(&p,content).map_err(|e|invalid(e.to_string()))?,
            Err(e) => return Err(invalid(e.to_string())),
        }
    }
    if entries.get(manifest).is_some_and(|old|old!=&entry) {
        return Err(invalid("artifact index disagrees with the published identity"));
    }
    entries.insert(manifest.into(),entry);
    save_atomic(&root.join("derived/index.json"),&entries).map_err(|e|invalid(e.to_string()))
}

/// No directory creation, browser contact or implicit catalog repair on reads.
pub(crate) fn list(bundle: &Path) -> Result<Vec<Artifact>,ToolError> {
    let root=bundle.canonicalize().map_err(|e|invalid(e.to_string()))?;
    let dir=root.join("derived");
    match std::fs::symlink_metadata(&dir) {
        Err(e) if e.kind()==std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(invalid(e.to_string())),
        Ok(_) => {}
    }
    if dir.canonicalize().map_err(|e|invalid(e.to_string()))? != dir { return Err(invalid("redirected derived directory")); }
    let entries=load(&root)?;
    for (key,entry) in &entries {
        if key!=&entry.manifest { return Err(invalid("artifact index key disagrees with manifest")); }
        for (name,hash) in [(&entry.file,&entry.sha256),(&entry.manifest,&entry.manifest_sha256)] {
            let bytes=std::fs::read(path(&root,name)?).map_err(|e|invalid(e.to_string()))?;
            if &sha256_bytes_hex(&bytes)!=hash { return Err(invalid(format!("artifact hash mismatch: {name}"))); }
        }
    }
    Ok(entries.into_values().collect())
}

#[cfg(test)]
#[path="derived_tests.rs"]
mod tests;
