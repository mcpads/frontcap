use crate::event::TaintRecord;
use crate::ids::{LaunchId, RunId};
use crate::profile::DeterminismProfile;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("bundle {0} already exists")]
    Exists(String),
    #[error("bundle {0} not found")]
    NotFound(String),
    #[error("io error at {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("invalid manifest: {0}")]
    Json(#[from] serde_json::Error),
    #[error("bundle is already finalized")]
    AlreadyFinalized,
    #[error("bundle contains a symbolic link: {0}")]
    Symlink(String),
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> BundleError + '_ {
    move |source| BundleError::Io { path: path.display().to_string(), source }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    None,
    Time,
    Frame,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Finalization {
    Open,
    Finalized,
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Determinism {
    pub level: Level,
    pub taints: Vec<TaintRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub run: RunId,
    pub launch: LaunchId,
    /// Engine product string, e.g. `HeadlessChrome/154.0.8037.92`.
    pub engine: String,
    pub engine_revision: String,
    pub host_class: String,
    pub profile: DeterminismProfile,
    pub determinism: Determinism,
    pub replayable: bool,
    pub missing_inputs: Vec<String>,
    pub finalization: Finalization,
    /// Selected immutable control-evidence snapshot for offline finalization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_snapshot: Option<String>,
    /// Relative path → SHA-256 of every source member, fixed at finalization.
    pub members: BTreeMap<String, String>,
}

/// Immutable execution identity saved at launch, before a final manifest exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionMetadata {
    pub run: RunId,
    pub launch: LaunchId,
    pub engine: String,
    pub engine_revision: String,
    pub profile: DeterminismProfile,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Inspection {
    pub finalized: bool,
    pub mismatched: Vec<String>,
    pub missing: Vec<String>,
    pub unexpected: Vec<String>,
}

pub struct Bundle {
    root: PathBuf,
}

impl Bundle {
    pub fn create(runs_root: &Path, run: &RunId) -> Result<Bundle, BundleError> {
        let root = runs_root.join(run.as_str());
        if root.exists() {
            return Err(BundleError::Exists(run.to_string()));
        }
        let b = Bundle { root };
        for d in [b.root.clone(), b.rules_dir(), b.screenshots_dir(), b.derived_dir(), b.root.join("inputs/bodies"), b.root.join("inputs/uploads")] {
            std::fs::create_dir_all(&d).map_err(io(&d))?;
        }
        Ok(b)
    }

    pub fn open(runs_root: &Path, run: &RunId) -> Result<Bundle, BundleError> {
        let root = runs_root.join(run.as_str());
        if !root.is_dir() {
            return Err(BundleError::NotFound(run.to_string()));
        }
        Ok(Bundle { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn trace(&self) -> PathBuf {
        self.root.join("trace.jsonl")
    }
    pub fn evidence(&self) -> PathBuf {
        self.root.join("evidence.jsonl")
    }
    pub fn commands(&self) -> PathBuf {
        self.root.join("inputs/commands.jsonl")
    }
    pub fn observations(&self) -> PathBuf {
        self.root.join("inputs/observations.jsonl")
    }
    pub fn rules_dir(&self) -> PathBuf {
        self.root.join("rules")
    }
    pub fn screenshots_dir(&self) -> PathBuf {
        self.root.join("screenshots")
    }
    /// Recorded frames, one PNG each.
    pub fn frames_dir(&self) -> PathBuf {
        self.root.join("frames")
    }
    pub fn derived_dir(&self) -> PathBuf {
        self.root.join("derived")
    }
    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    pub fn write_manifest(&self, m: &Manifest) -> Result<(), BundleError> {
        let p = self.manifest_path();
        crate::state::save_atomic(&p, m).map_err(io(&p))
    }

    fn read_manifest(&self) -> Result<Option<Manifest>, BundleError> {
        let p = self.manifest_path();
        crate::state::load(&p).map_err(io(&p))
    }

    /// Source members: every regular file except `manifest.json` and anything under `derived/`.
    fn hash_members(&self) -> Result<BTreeMap<String, String>, BundleError> {
        let mut out = BTreeMap::new();
        let mut stack = vec![self.root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).map_err(io(&dir))? {
                let entry = entry.map_err(io(&dir))?;
                let path = entry.path();
                let rel = path.strip_prefix(&self.root).expect("under root").to_string_lossy().replace('\\', "/");
                // `save_atomic` leaves `.manifest.json.tmp-<pid>` only if it failed mid-write.
                if rel == "manifest.json" || rel.starts_with(".manifest.json.tmp-") || rel == "derived" || rel.starts_with("derived/") {
                    continue;
                }
                let kind = entry.file_type().map_err(io(&path))?;
                if kind.is_symlink() {
                    return Err(BundleError::Symlink(rel));
                }
                if kind.is_dir() {
                    stack.push(path);
                } else {
                    let bytes = std::fs::read(&path).map_err(io(&path))?;
                    let hex: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
                    out.insert(rel, hex);
                }
            }
        }
        Ok(out)
    }

    pub fn finalize(&self, mut manifest: Manifest) -> Result<Manifest, BundleError> {
        if let Some(existing) = self.read_manifest()?
            && existing.finalization == Finalization::Finalized
        {
            return Err(BundleError::AlreadyFinalized);
        }
        manifest.members = self.hash_members()?;
        manifest.finalization = Finalization::Finalized;
        self.write_manifest(&manifest)?;
        Ok(manifest)
    }

    pub fn inspect(&self) -> Result<Inspection, BundleError> {
        let Some(m) = self.read_manifest()? else {
            return Ok(Inspection { finalized: false, mismatched: vec![], missing: vec![], unexpected: vec![] });
        };
        let finalized = m.finalization == Finalization::Finalized;
        let actual = self.hash_members()?;
        let mut mismatched = Vec::new();
        let mut missing = Vec::new();
        for (k, v) in &m.members {
            match actual.get(k) {
                Some(h) if h == v => {}
                Some(_) => mismatched.push(k.clone()),
                None => missing.push(k.clone()),
            }
        }
        let unexpected = if finalized {
            actual.keys().filter(|k| !m.members.contains_key(*k)).cloned().collect()
        } else {
            Vec::new()
        };
        Ok(Inspection { finalized, mismatched, missing, unexpected })
    }
}

#[cfg(test)]
#[path = "bundle_tests.rs"]
mod tests;
