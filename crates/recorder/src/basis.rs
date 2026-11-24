//! Saved bases in the workspace (`saved/<saved_id>/`). A basis is
//! written once, into a private directory renamed into place, and only read afterwards. Every read
//! recomputes the digest of the manifest and of each file it names, so a changed basis is refused
//! rather than used.
use crate::launcher::Workspace;
use frontcap_core::basis::{BasisKind, BasisManifest, BasisOrigin, CheckpointSource, id_of};
use frontcap_core::canonical::{canonical_of, sha256_bytes_hex};
use frontcap_core::ids::SavedId;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct SavedBasis {
    pub id: SavedId,
    pub sha256: String,
    pub manifest: BasisManifest,
    pub dir: PathBuf,
}

impl SavedBasis {
    /// The kept copy of a file the manifest names.
    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The process image a checkpoint basis restores, by name among the workspace's images.
    pub fn checkpoint_image(&self) -> Option<&str> {
        self.manifest.checkpoint.as_ref().map(|_| self.id.as_str())
    }

    /// The finalized run a replay basis repeats: kept in the basis, or among the workspace's runs.
    pub fn replay_source(&self, ws: &Workspace) -> Option<PathBuf> {
        let source = self.manifest.replay.as_ref()?;
        Some(if source.kept { self.dir.join("bundle").join(source.run.as_str()) } else { ws.runs().join(source.run.as_str()) })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BasisError {
    #[error("no saved basis {0}")]
    NotFound(SavedId),
    #[error("saved basis {id} is not intact: {reason}")]
    Corrupt { id: SavedId, reason: String },
    #[error("a branch of saved basis {0} is being constructed")]
    InUse(SavedId),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// The name under which a file of `bytes` is kept: `files/<sha256>.<extension>`.
pub fn file_name(bytes: &[u8], extension: &str) -> String {
    format!("files/{}.{extension}", sha256_bytes_hex(bytes))
}

fn kept_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("files/") else { return false };
    let Some((hex, extension)) = rest.split_once('.') else { return false };
    hex.len() == 64
        && hex.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && !extension.is_empty()
        && extension.bytes().all(|b| b.is_ascii_lowercase())
}

fn write_synced(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Saves `manifest` with the contents of the files it names (`(name, bytes)` each), or returns the
/// basis already saved with the same content.
pub fn save(ws: &Workspace, manifest: BasisManifest, files: Vec<(String, Vec<u8>)>) -> Result<SavedBasis, BasisError> {
    save_with(ws, manifest, files, None)
}

/// Saves a replay basis whose prefix `bundle` (a finalized run directory) is moved into it.
pub fn save_kept(ws: &Workspace, manifest: BasisManifest, bundle: &Path) -> Result<SavedBasis, BasisError> {
    save_with(ws, manifest, Vec::new(), Some(bundle))
}

fn save_with(ws: &Workspace, manifest: BasisManifest, files: Vec<(String, Vec<u8>)>, bundle: Option<&Path>) -> Result<SavedBasis, BasisError> {
    let sha256 = manifest.sha256();
    let id = id_of(&sha256);
    let corrupt = |reason: String| BasisError::Corrupt { id: id.clone(), reason };
    manifest.validate().map_err(|e| corrupt(e.to_string()))?;
    for (name, bytes) in &files {
        if manifest.files.get(name).map(String::as_str) != Some(sha256_bytes_hex(bytes).as_str()) || !kept_name(name) {
            return Err(corrupt(format!("file {name} does not match the manifest")));
        }
    }
    if manifest.files.len() != files.len() {
        return Err(corrupt("the manifest names files that were not supplied".into()));
    }
    let dir = ws.saved().join(id.as_str());
    if dir.exists() {
        return load(ws, &id);
    }
    std::fs::create_dir_all(ws.saved())?;
    let private = ws.saved().join(format!(".{}.{}", id.as_str(), frontcap_core::ids::LaunchId::generate().as_str()));
    std::fs::create_dir_all(private.join("files"))?;
    let written = (|| -> std::io::Result<()> {
        for (name, bytes) in &files {
            write_synced(&private.join(name), bytes)?;
        }
        if let (Some(bundle), Some(run)) = (bundle, manifest.replay.as_ref().map(|r| &r.run)) {
            std::fs::create_dir_all(private.join("bundle"))?;
            std::fs::rename(bundle, private.join("bundle").join(run.as_str()))?;
        }
        std::fs::File::open(private.join("files"))?.sync_all()?;
        write_synced(&private.join("manifest.json"), canonical_of(&manifest).as_bytes())?;
        write_synced(&private.join("sha256"), sha256.as_bytes())?;
        std::fs::File::open(&private)?.sync_all()?;
        match std::fs::rename(&private, &dir) {
            Ok(()) => std::fs::File::open(ws.saved())?.sync_all(),
            // Another save of the same content won the rename; its copy is equal by identity.
            Err(_) if dir.exists() => std::fs::remove_dir_all(&private),
            Err(e) => Err(e),
        }
    })();
    if written.is_err() {
        let _ = std::fs::remove_dir_all(&private);
    }
    written?;
    load(ws, &id)
}

/// Saves the process image a recorder kept under the workspace's images as `staging` (`Call::Image`) as a
/// checkpoint basis: the image is pinned by its tree's digest and renamed to the basis's saved ID.
pub fn save_checkpoint(ws: &Workspace, staging: &str, origin: BasisOrigin, excluded: Vec<String>, verify: Vec<frontcap_core::basis::Verification>) -> Result<SavedBasis, BasisError> {
    let refused = |e: String| BasisError::Io(std::io::Error::other(e));
    let staged = crate::image::image_dir(ws, staging).map_err(refused)?;
    crate::image::private(&staged).map_err(refused)?;
    let (tree_sha256, bytes) = tree_digest(&staged)?;
    let manifest = BasisManifest {
        kind: BasisKind::Checkpoint, recipe: None, replay: None, checkpoint: Some(CheckpointSource { tree_sha256, bytes }),
        files: Default::default(), origin: Some(origin), excluded, verify,
    };
    let image = crate::image::image_dir(ws, manifest.saved_id().as_str()).map_err(refused)?;
    if image.exists() {
        // The same image saved before: its copy is equal by identity.
        std::fs::remove_dir_all(&staged)?;
    } else {
        std::fs::rename(&staged, &image)?;
    }
    save_with(ws, manifest, Vec::new(), None)
}

/// The digest of a directory tree, over every entry in path order: its path, type, mode, owner and
/// content (a file's bytes, a link's target); and the bytes of its files.
fn tree_digest(root: &Path) -> std::io::Result<(String, u64)> {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    fn walk(root: &Path, rel: &Path, tree: &mut Sha256, bytes: &mut u64) -> std::io::Result<()> {
        let mut entries = std::fs::read_dir(root.join(rel))?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = rel.join(entry.file_name());
            let meta = std::fs::symlink_metadata(root.join(&path))?;
            let name = path.as_os_str().as_bytes();
            tree.update((name.len() as u64).to_le_bytes());
            tree.update(name);
            for n in [meta.mode(), meta.uid(), meta.gid()] {
                tree.update(n.to_le_bytes());
            }
            let kind = meta.file_type();
            if kind.is_dir() {
                tree.update(b"d");
                walk(root, &path, tree, bytes)?;
            } else if kind.is_symlink() {
                let target = std::fs::read_link(root.join(&path))?;
                tree.update(b"l");
                tree.update(Sha256::digest(target.as_os_str().as_bytes()));
            } else if kind.is_file() {
                let mut content = Sha256::new();
                let mut file = std::fs::File::open(root.join(&path))?;
                let mut buffer = vec![0; 1 << 20];
                loop {
                    let n = std::io::Read::read(&mut file, &mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    content.update(&buffer[..n]);
                }
                *bytes += meta.len();
                tree.update(b"f");
                tree.update(content.finalize());
            } else {
                return Err(std::io::Error::other(format!("{} is neither a file, a directory nor a link", path.display())));
            }
        }
        Ok(())
    }
    let mut tree = Sha256::new();
    let mut bytes = 0;
    walk(root, Path::new(""), &mut tree, &mut bytes)?;
    Ok((tree.finalize().iter().map(|b| format!("{b:02x}")).collect(), bytes))
}

/// A branch's use of a saved basis while it constructs its child: deleting the basis is refused meanwhile.
pub struct InUse(std::fs::File);

impl Drop for InUse {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn opened(ws: &Workspace, id: &SavedId) -> Result<std::fs::File, BasisError> {
    std::fs::File::open(ws.saved().join(id.as_str())).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => BasisError::NotFound(id.clone()),
        _ => e.into(),
    })
}

/// Holds `id` against deletion until the guard drops. Load it after: one deleted meanwhile is not found.
pub fn hold(ws: &Workspace, id: &SavedId) -> Result<InUse, BasisError> {
    let dir = opened(ws, id)?;
    dir.lock_shared()?;
    Ok(InUse(dir))
}

/// Deletes a saved basis with what it keeps (files, a replay prefix, a checkpoint's image), unless a
/// branch of it is being constructed. It is renamed out of reach first, so it is never found half
/// removed; what a crash left half removed is removed by the next deletion.
pub fn delete(ws: &Workspace, id: &SavedId) -> Result<(), BasisError> {
    let dir = opened(ws, id)?;
    match dir.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Err(BasisError::InUse(id.clone())),
        Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
    }
    let removed = |root: &Path| root.join(format!(".deleted.{}.{}", id.as_str(), frontcap_core::ids::LaunchId::generate().as_str()));
    std::fs::rename(ws.saved().join(id.as_str()), removed(&ws.saved()))?;
    std::fs::File::open(ws.saved())?.sync_all()?;
    let image = ws.images().join(id.as_str());
    if image.exists() {
        std::fs::rename(&image, removed(&ws.images()))?;
    }
    drop(dir);
    for root in [ws.saved(), ws.images()] {
        for entry in std::fs::read_dir(&root).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().starts_with(".deleted.") {
                std::fs::remove_dir_all(entry.path())?;
            }
        }
    }
    Ok(())
}

/// Reads and verifies a saved basis.
pub fn load(ws: &Workspace, id: &SavedId) -> Result<SavedBasis, BasisError> {
    let dir = ws.saved().join(id.as_str());
    if !dir.is_dir() {
        return Err(BasisError::NotFound(id.clone()));
    }
    let corrupt = |reason: String| BasisError::Corrupt { id: id.clone(), reason };
    let text = std::fs::read_to_string(dir.join("manifest.json")).map_err(|e| corrupt(format!("manifest: {e}")))?;
    let manifest: BasisManifest = serde_json::from_str(&text).map_err(|e| corrupt(format!("manifest: {e}")))?;
    manifest.validate().map_err(|e| corrupt(e.to_string()))?;
    let sha256 = manifest.sha256();
    if &id_of(&sha256) != id {
        return Err(corrupt("its manifest no longer matches its identity".into()));
    }
    let recorded = std::fs::read_to_string(dir.join("sha256")).map_err(|e| corrupt(format!("digest: {e}")))?;
    if recorded.trim() != sha256 {
        return Err(corrupt("its manifest digest differs from the recorded one".into()));
    }
    if let Some(source) = manifest.replay.as_ref().filter(|r| r.kept) {
        let bytes = std::fs::read(dir.join("bundle").join(source.run.as_str()).join("manifest.json"))
            .map_err(|e| corrupt(format!("kept run {}: {e}", source.run)))?;
        if sha256_bytes_hex(&bytes) != source.manifest_sha256 {
            return Err(corrupt(format!("kept run {} changed", source.run)));
        }
    }
    if let Some(source) = &manifest.checkpoint {
        // Restored with the privileges of a recorder that runs cells: only its own user's image, unchanged.
        let image = crate::image::image_dir(ws, id.as_str()).map_err(corrupt)?;
        crate::image::private(&image).map_err(corrupt)?;
        let (tree, _) = tree_digest(&image).map_err(|e| corrupt(format!("its image: {e}")))?;
        if tree != source.tree_sha256 {
            return Err(corrupt("its image changed".into()));
        }
    }
    for (name, digest) in &manifest.files {
        if !kept_name(name) {
            return Err(corrupt(format!("file name {name} is not a kept file")));
        }
        let bytes = std::fs::read(dir.join(name)).map_err(|e| corrupt(format!("{name}: {e}")))?;
        if &sha256_bytes_hex(&bytes) != digest {
            return Err(corrupt(format!("{name} changed")));
        }
    }
    Ok(SavedBasis { id: id.clone(), sha256, manifest, dir })
}

#[cfg(test)]
#[path = "basis_tests.rs"]
mod tests;
