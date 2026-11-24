//! Original Simulator creator holds the data-volume worker until device deletion is proved.
use crate::{products::BundleInventory, storage_volume::{Config, Worker, RetirementEvidence}};
use frontcap_core::{state::load, write_budget::{Class, WriteBudget, save_inherited}};
use serde::{Deserialize, Serialize};
use std::{fs, io, os::unix::fs::{MetadataExt, symlink}, path::{Path, PathBuf}, process::Command,
    sync::atomic::{AtomicBool, Ordering}, time::Instant};

pub(super) struct DataVolume {
    worker: Worker,
    source: PathBuf,
    device_root: PathBuf,
    original_device: u64,
    original_inode: u64,
    copy_retired: bool,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Relocation {
    source: PathBuf,
    destination: PathBuf,
    backup: PathBuf,
    original_device: u64,
    original_inode: u64,
    initial: BundleInventory,
}

// Read-only path validation for the live Simulator client; this grants no retirement authority.
pub(crate) fn validate_container(directory: &Path, udid: &str, container: &Path) -> io::Result<()> {
    let proof: Relocation = load(&directory.join("data-relocation.json"))?
        .ok_or_else(|| io::Error::other("owned Simulator data relocation missing"))?;
    let launch: crate::simulator::SimulatorLaunch = load(&directory.join("launch.json"))?
        .ok_or_else(|| io::Error::other("original Simulator launch missing"))?;
    let expected = launch.data_volume_directory(directory).join("worker/volume/mount/device-data");
    if proof.destination != expected || expected.canonicalize()? != expected
        || proof.source.file_name().and_then(|s| s.to_str()) != Some("data")
        || proof.source.parent().and_then(Path::file_name).and_then(|s| s.to_str()) != Some(udid)
        || !fs::symlink_metadata(&proof.source)?.is_symlink()
        || proof.source.canonicalize()? != expected
        || container == expected || !container.starts_with(&expected) {
        return Err(io::Error::other("container is outside the owned relocated Simulator data"));
    }
    Ok(())
}
impl DataVolume {
    pub fn start(config: &Config, source: &Path, udid: &str, directory: &Path, deadline: Instant) -> io::Result<Self> {
        let device_root = source.parent().ok_or_else(|| io::Error::other("Simulator data parent missing"))?;
        if !source.is_absolute() || source.file_name().and_then(|s| s.to_str()) != Some("data")
            || device_root.file_name().and_then(|s| s.to_str()) != Some(udid)
            || !fs::symlink_metadata(source)?.is_dir() || source.canonicalize()? != source {
            return Err(io::Error::other("created Simulator data path is not an original canonical directory"));
        }
        let original = fs::symlink_metadata(source)?;
        let worker = Worker::start(config, directory, deadline)?;
        Ok(Self { worker, source: source.to_owned(), device_root: device_root.to_owned(),
            original_device: original.dev(), original_inode: original.ino(), copy_retired: true })
    }
    pub fn relocate(&mut self, directory: &Path, deadline: Instant, cancelled: &AtomicBool) -> io::Result<()> {
        let mut check = || {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
                Err(io::Error::other("Simulator data preparation cancelled or expired"))
            } else { Ok(()) }
        };
        check()?;
        if fs::symlink_metadata(&self.worker.mount)?.dev() == self.original_device {
            return Err(io::Error::other("Simulator data volume is not a separate filesystem"));
        }
        let initial = BundleInventory::capture_checked(&self.source, &mut check)?;
        let bytes = initial.files.values().try_fold(0_u64, |sum, file| sum.checked_add(file.bytes)
            .ok_or_else(|| io::Error::other("Simulator initial data size overflow")))?;
        if let Some(budget) = WriteBudget::from_environment()? { budget.charge(Class::Ordinary, bytes)?; }
        let destination = self.worker.mount.join("device-data");
        let backup = self.device_root.join(format!("data-original-{}", frontcap_core::ids::RequestId::generate()));
        let proof = Relocation { source: self.source.clone(), destination: destination.clone(), backup: backup.clone(),
            original_device: self.original_device, original_inode: self.original_inode, initial };
        save_inherited(Class::Ordinary, &directory.join("data-relocation-plan.json"), &proof)?;
        let mut command = Command::new("/usr/bin/ditto"); command.arg(&self.source).arg(&destination);
        self.copy_retired = false;
        let exchange = crate::process::capture_recorded(&mut command, deadline, cancelled, 4 * 1024 * 1024,
            &directory.join("data-copy"))?;
        self.copy_retired = exchange.cleanup_failure.is_none();
        if let Some(error) = exchange.failure.or(exchange.cleanup_failure).or(exchange.evidence_failure) {
            return Err(io::Error::other(error));
        }
        if BundleInventory::capture_checked(&destination, &mut check)? != proof.initial
            || BundleInventory::capture_checked(&self.source, &mut check)? != proof.initial {
            return Err(io::Error::other("Simulator data changed during relocation"));
        }
        let original = fs::symlink_metadata(&self.source)?;
        if !original.is_dir() || original.dev() != self.original_device || original.ino() != self.original_inode
            || backup.try_exists()? {
            return Err(io::Error::other("Simulator data identity changed before relocation"));
        }
        check()?;
        fs::rename(&self.source, &backup)?;
        symlink(&destination, &self.source)?;
        save_inherited(Class::Ordinary, &directory.join("data-relocation.json"), &proof)?;
        // The creator owns this exact pre-boot directory; it is no longer an SDK data path.
        fs::remove_dir_all(backup)?;
        Ok(())
    }
    pub fn retire(self, directory: &Path, deadline: Instant) -> io::Result<()> {
        if !self.copy_retired { return Err(io::Error::other("Simulator data copy child retirement unresolved")); }
        match fs::symlink_metadata(&self.device_root) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            _ => return Err(io::Error::other("created Simulator directory remains; data writers unproven")),
        }
        let proof = self.worker.retire(deadline)?;
        save_inherited(Class::Retirement, &directory.join("data-volume-retirement.json"), &proof)
    }
}

pub(crate) fn inspect_retirement(directory: &Path) -> io::Result<()> {
    let launch: crate::simulator::SimulatorLaunch = load(&directory.join("launch.json"))?
        .ok_or_else(|| io::Error::other("original Simulator launch missing"))?;
    let volume_directory = launch.data_volume_directory(directory);
    if let Some(config) = launch.data_volume {
        let attempted: bool = load(&directory.join("data-volume-attempted.json"))?
            .ok_or_else(|| io::Error::other("Simulator data-volume acquisition disposition missing"))?;
        if !attempted {
            if volume_directory.try_exists()? {
                return Err(io::Error::other("unattempted Simulator data volume has worker evidence"));
            }
            return Ok(());
        }
        let proof: RetirementEvidence = load(&directory.join("data-volume-retirement.json"))?
            .ok_or_else(|| io::Error::other("Simulator data-volume retirement missing"))?;
        crate::storage_volume::inspect(&volume_directory, &config, &proof)?;
    }
    Ok(())
}
