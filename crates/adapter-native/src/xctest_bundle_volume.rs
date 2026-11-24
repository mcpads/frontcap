//! One optional filesystem for SDK-created result bundles; control evidence stays outside it.
use crate::storage_volume::{Config, RetirementEvidence, Worker};
use frontcap_core::{process::ProcessIdentity, state::load, write_budget::{Class, save_inherited}};
use serde::{Deserialize, Serialize};
use std::{fs, io, os::unix::fs::MetadataExt, path::{Path, PathBuf}, time::Instant};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    config: Config,
    simulator: String,
    runner_bundle: String,
    directory: PathBuf,
    directory_device: u64,
    directory_inode: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    mount: PathBuf,
    device: u64,
    inode: u64,
    result_bundle: PathBuf,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    plan: Plan,
    root: Root,
    writer: Option<ProcessIdentity>,
    writer_started: bool,
    volume: RetirementEvidence,
}

pub(super) struct BundleVolume {
    plan: Plan,
    root: Option<Root>,
    worker: Option<Worker>,
    completion: Option<Completion>,
    creation_attempted: bool,
    writer_attempted: bool,
}
impl BundleVolume {
    pub fn prepare(config: Config, directory: &Path, target: &Path, simulator: &str, runner_bundle: &str) -> io::Result<Self> {
        config.validate()?;
        if !fs::symlink_metadata(target)?.is_dir() || target.canonicalize()? != target || !directory.starts_with(target) || directory == target {
            return Err(io::Error::other("XCTest bundle owner must belong to the original target root"));
        }
        let metadata = fs::symlink_metadata(directory)?;
        if !metadata.is_dir() || directory.canonicalize()? != directory {
            return Err(io::Error::other("XCTest bundle owner must be a canonical real directory"));
        }
        let plan = Plan { config, simulator: simulator.into(), runner_bundle: runner_bundle.into(), directory: directory.to_owned(),
            directory_device: metadata.dev(), directory_inode: metadata.ino() };
        save_inherited(Class::Ordinary, &directory.join("result-bundle-plan.json"), &plan)?;
        let registry = target.join("xctest-bundle-owners.json");
        match fs::symlink_metadata(&registry) {
            Ok(metadata) if metadata.is_file() && metadata.len() <= 1024 * 1024 => (),
            Ok(_) => return Err(io::Error::other("XCTest bundle owner registry must be a bounded regular file")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        let mut owners: Vec<Plan> = load(&registry)?.unwrap_or_default();
        if owners.len() >= 128 || owners.iter().any(|owner| owner.directory == directory) {
            return Err(io::Error::other("XCTest bundle owner already registered or owner capacity exhausted"));
        }
        owners.push(plan.clone());
        save_inherited(Class::Ordinary, &registry, &owners)?;
        Ok(Self { plan, root: None, worker: None, completion: None, creation_attempted: false, writer_attempted: false })
    }
    pub fn start(&mut self, deadline: Instant) -> io::Result<PathBuf> {
        self.creation_attempted = true;
        self.worker = Some(Worker::start(&self.plan.config, &self.plan.directory.join("result-bundle-volume"), deadline)?);
        let mount = self.worker.as_ref().unwrap().mount.clone();
        let metadata = fs::symlink_metadata(&mount)?;
        if !metadata.is_dir() || metadata.dev() == fs::symlink_metadata(&self.plan.directory)?.dev() {
            return Err(io::Error::other("XCTest bundle requires an independent owned filesystem"));
        }
        let root = Root { result_bundle: mount.join("test.xcresult"), mount, device: metadata.dev(), inode: metadata.ino() };
        // Register the writer's root before the SDK receives its path. This is not a whole-run quota.
        save_inherited(Class::Ordinary, &self.plan.directory.join("result-bundle-root.json"), &root)?;
        let path = root.result_bundle.clone();
        self.root = Some(root);
        Ok(path)
    }
    pub fn writer_starting(&mut self) -> io::Result<()> {
        self.verify_root()?;
        self.writer_attempted = true;
        Ok(())
    }
    fn verify_root(&self) -> io::Result<()> {
        let root = self.root.as_ref().ok_or_else(|| io::Error::other("XCTest bundle root not registered"))?;
        let metadata = fs::symlink_metadata(&root.mount)?;
        if !metadata.is_dir() || (metadata.dev(), metadata.ino()) != (root.device, root.inode) {
            return Err(io::Error::other("XCTest bundle filesystem identity changed"));
        }
        Ok(())
    }
    /// The actual SDK process/group handle must have retired; its exit code is not an SDK success claim.
    pub fn finish(&mut self, writer: Option<ProcessIdentity>, writers_retired: bool, deadline: Instant) -> io::Result<()> {
        if self.completion.is_none() {
            if self.writer_attempted && !writers_retired {
                return Err(io::Error::other("XCTest bundle writers have not retired; preserve original volume"));
            }
            if self.writer_attempted && writer.is_none() {
                return Err(io::Error::other("XCTest bundle writer identity unavailable; preserve original volume"));
            }
            if !self.creation_attempted { return Ok(()); }
            let worker = self.worker.take().ok_or_else(|| io::Error::other("XCTest bundle creation/retirement unresolved; no adoption authority"))?;
            // A ready worker may precede rejected root registration. No SDK writer started then;
            // retain its image using this still-live authority, never a reconstructed path handle.
            let root = match &self.root {
                Some(root) => root.clone(),
                None => {
                    let metadata = fs::symlink_metadata(&worker.mount)?;
                    Root { mount: worker.mount.clone(), result_bundle: worker.mount.join("test.xcresult"), device: metadata.dev(), inode: metadata.ino() }
                }
            };
            let volume = worker.retire_compressed(deadline)?;
            self.completion = Some(Completion { plan: self.plan.clone(), root, writer, writer_started: self.writer_attempted, volume });
        }
        save_inherited(Class::Retirement, &self.plan.directory.join("result-bundle-retirement.json"), self.completion.as_ref().unwrap())
    }
}

fn inspect(directory: &Path, expected: &super::RunnerLaunch, owned_simulator: &str, registered: &Plan, deadline: Option<Instant>) -> io::Result<()> {
    let metadata = fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || directory.canonicalize()? != directory {
        return Err(io::Error::other("XCTest bundle owner directory replaced or linked"));
    }
    let launch: super::RunnerLaunch = load(&directory.join("launch.json"))?
        .ok_or_else(|| io::Error::other("Original acquired XCTest launch missing"))?;
    if frontcap_core::canonical::canonical_of(&launch) != frontcap_core::canonical::canonical_of(expected) {
        return Err(io::Error::other("Acquired XCTest launch differs from original session runner"));
    }
    let simulator: String = load(&directory.join("simulator.json"))?
        .ok_or_else(|| io::Error::other("Original acquired XCTest simulator missing"))?;
    let writer: Option<ProcessIdentity> = load(&directory.join("runner-process.json"))?;
    let plan: Plan = load(&directory.join("result-bundle-plan.json"))?
        .ok_or_else(|| io::Error::other("XCTest bundle plan missing"))?;
    let completed: Completion = load(&directory.join("result-bundle-retirement.json"))?
        .ok_or_else(|| io::Error::other("XCTest bundle retirement unresolved"))?;
    if &plan != registered || launch.result_bundle_volume.as_ref() != Some(&plan.config)
        || launch.app.runner_bundle != plan.runner_bundle || simulator != plan.simulator || simulator != owned_simulator
        || (metadata.dev(), metadata.ino()) != (plan.directory_device, plan.directory_inode)
        || completed.writer != writer || completed.writer_started != writer.is_some()
        || completed.plan != plan || plan.directory != directory
        || completed.root.mount != directory.join("result-bundle-volume/worker/volume/mount")
        || completed.root.result_bundle != completed.root.mount.join("test.xcresult") {
        return Err(io::Error::other("XCTest bundle retirement differs from original owner"));
    }
    if completed.writer_started {
        let root: Root = load(&directory.join("result-bundle-root.json"))?
            .ok_or_else(|| io::Error::other("XCTest bundle original root registration missing"))?;
        if root != completed.root || completed.writer.is_none() {
            return Err(io::Error::other("XCTest bundle writer/root evidence differs"));
        }
    }
    if let Some(writer) = &completed.writer && !writer.has_exited()? {
        return Err(io::Error::other("XCTest bundle writer remains live"));
    }
    match deadline {
        Some(deadline) => crate::storage_volume::inspect_retained_until(&directory.join("result-bundle-volume"), &plan.config, &completed.volume, deadline),
        None => crate::storage_volume::inspect_retained(&directory.join("result-bundle-volume"), &plan.config, &completed.volume),
    }
}

/// The original target retains the owner registry outside all acquired/replaced runner directories.
/// Registry entries are proof requirements only: they never recreate volume or signal authority.
pub(crate) fn inspect_target(target: &Path, expected: &super::RunnerLaunch) -> io::Result<()> {
    inspect_registered(target, Some(expected), None)
}

/// Group cleanup retains its fence if any registered SDK volume lacks completion.
/// Absence preserves generic groups that never opted into result-bundle ownership.
pub(crate) fn inspect_group(target: &Path, deadline: Instant) -> io::Result<()> {
    match fs::symlink_metadata(target.join("xctest-bundle-owners.json")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(_) => (),
    }
    inspect_registered(target, None, Some(deadline))
}

fn inspect_registered(target: &Path, expected: Option<&super::RunnerLaunch>, deadline: Option<Instant>) -> io::Result<()> {
    let original = fs::symlink_metadata(target)?;
    if !original.is_dir() || target.canonicalize()? != target {
        return Err(io::Error::other("XCTest target root must be a canonical real directory"));
    }
    let registry = target.join("xctest-bundle-owners.json");
    let metadata = fs::symlink_metadata(&registry)?;
    if !metadata.is_file() || metadata.len() > 1024 * 1024 {
        return Err(io::Error::other("XCTest bundle owner registry must be a bounded regular file"));
    }
    let owners: Vec<Plan> = load(&registry)?
        .ok_or_else(|| io::Error::other("XCTest bundle owner registry missing"))?;
    if owners.is_empty() || owners.len() > 128 {
        return Err(io::Error::other("XCTest bundle owner registry is empty or exceeds its bound"));
    }
    let simulator: String = load(&target.join("simulator/simulator.json"))?
        .ok_or_else(|| io::Error::other("Original target Simulator identity missing"))?;
    let mut seen = std::collections::BTreeSet::new();
    for owner in &owners {
        if !owner.directory.starts_with(target) || owner.directory == target || !seen.insert(&owner.directory) {
            return Err(io::Error::other("XCTest bundle owner registry escapes target or repeats an owner"));
        }
        let acquired: super::RunnerLaunch;
        let expected = match expected {
            Some(expected) => expected,
            None => {
                acquired = load(&owner.directory.join("launch.json"))?
                    .ok_or_else(|| io::Error::other("Registered XCTest owner launch missing"))?;
                &acquired
            }
        };
        inspect(&owner.directory, expected, &simulator, owner, deadline)?;
    }
    let after = fs::symlink_metadata(target)?;
    let registry_after = fs::symlink_metadata(&registry)?;
    if !after.is_dir() || (original.dev(), original.ino()) != (after.dev(), after.ino())
        || !registry_after.is_file() || (metadata.dev(), metadata.ino(), metadata.len(), metadata.mtime(), metadata.mtime_nsec())
            != (registry_after.dev(), registry_after.ino(), registry_after.len(), registry_after.mtime(), registry_after.mtime_nsec()) {
        return Err(io::Error::other("XCTest target root or registry changed during inspection"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "xctest_bundle_volume_tests.rs"]
pub(crate) mod tests;
