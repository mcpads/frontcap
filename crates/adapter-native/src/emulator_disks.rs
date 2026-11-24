//! SDK scratch storage belongs to the live emulator owner, never to the shared AVD.
use frontcap_core::state::save_atomic;
use serde::Serialize;
use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
};

#[derive(Serialize)]
pub(crate) struct TemporaryDisks {
    pub path: PathBuf,
    device: u64,
    inode: u64,
    evidence_directory: PathBuf,
}

#[derive(Serialize)]
struct Entry {
    path: PathBuf,
    kind: &'static str,
    bytes: u64,
    allocated_bytes: u64,
}

#[derive(Serialize)]
struct Cleanup<'a> {
    directory: &'a TemporaryDisks,
    removed: bool,
    entries: &'a [Entry],
}

impl TemporaryDisks {
    pub fn create(directory: &Path, storage_root: Option<&Path>) -> io::Result<Self> {
        let path = match storage_root {
            None => directory.join("emulator-temporary"),
            Some(root) => {
                // A configured parent is routing input, not cleanup authority. Resolve it once
                // and retain the actual child identity before any SDK process can write there.
                if !root.is_absolute() || !fs::symlink_metadata(root)?.is_dir() {
                    return Err(io::Error::other("emulator storage root must be an absolute directory, not a symlink"));
                }
                root.canonicalize()?.join(format!("emulator-{}", frontcap_core::ids::RequestId::generate()))
            }
        };
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        let owned = Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
            evidence_directory: directory.to_owned(),
        };
        if let Err(error) = save_atomic(&directory.join("emulator-temporary.json"), &owned) {
            let _ = fs::remove_dir(&owned.path);
            return Err(error);
        }
        Ok(owned)
    }

    /// Only called after the emulator process group and its ports have retired.
    /// No Drop cleanup: losing the owner does not prove that the SDK stopped using its disks.
    pub fn retire(&self) -> io::Result<()> {
        let metadata = fs::symlink_metadata(&self.path)?;
        if !metadata.is_dir() || metadata.dev() != self.device || metadata.ino() != self.inode {
            return Err(io::Error::other(
                "emulator temporary directory identity changed",
            ));
        }
        let mut entries = Vec::new();
        self.inventory(&self.path, 0, &mut entries)?;
        let evidence = self.evidence_directory.join("emulator-temporary-cleanup.json");
        save_atomic(
            &evidence,
            &Cleanup {
                directory: self,
                removed: false,
                entries: &entries,
            },
        )?;
        // remove_dir_all does not follow symlinks. Only our exclusively created directory is removed.
        fs::remove_dir_all(&self.path)?;
        save_atomic(
            &evidence,
            &Cleanup {
                directory: self,
                removed: true,
                entries: &entries,
            },
        )
    }

    fn inventory(&self, path: &Path, depth: usize, entries: &mut Vec<Entry>) -> io::Result<()> {
        if depth > 32 {
            return Err(io::Error::other(
                "emulator temporary inventory depth exceeded",
            ));
        }
        let mut children = fs::read_dir(path)?.collect::<io::Result<Vec<_>>>()?;
        children.sort_by_key(|child| child.file_name());
        for child in children {
            if entries.len() >= 10_000 {
                return Err(io::Error::other(
                    "emulator temporary inventory count exceeded",
                ));
            }
            let path = child.path();
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.dev() != self.device {
                return Err(io::Error::other(
                    "emulator temporary directory contains another filesystem",
                ));
            }
            entries.push(Entry {
                path: path.strip_prefix(&self.path).unwrap().to_owned(),
                kind: if metadata.is_dir() {
                    "directory"
                } else if metadata.is_file() {
                    "file"
                } else if metadata.is_symlink() {
                    "symlink"
                } else {
                    "other"
                },
                bytes: metadata.len(),
                allocated_bytes: metadata.blocks().saturating_mul(512),
            });
            if metadata.is_dir() {
                self.inventory(&path, depth + 1, entries)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "emulator_disks_tests.rs"]
mod tests;
