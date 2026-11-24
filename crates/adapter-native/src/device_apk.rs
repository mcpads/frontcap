//! Preserve a verified APK independently of later source edits without forcing a userspace rewrite.
use frontcap_core::write_budget::{Class, WriteBudget};
use std::{fs::{self, File}, io, os::unix::fs::{DirBuilderExt, PermissionsExt}, path::Path};

pub(super) fn verify(path: &Path, expected: &str) -> io::Result<()> {
    // Existing Android launch inputs may name a symlink. Resolve it before the regular-file reader.
    let file = crate::products::read_file(&path.canonicalize()?, None)?;
    if file.sha256 != expected {
        return Err(io::Error::other("APK differs from frozen digest"));
    }
    Ok(())
}

pub(super) fn snapshot(source: &Path, destination: &Path, expected: &str) -> io::Result<()> {
    let budget = WriteBudget::from_environment()?;
    snapshot_with_budget(source, destination, expected, budget.as_ref())
}
fn snapshot_with_budget(source: &Path, destination: &Path, expected: &str, budget: Option<&WriteBudget>) -> io::Result<()> {
    match destination.symlink_metadata() {
        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
        Err(error) => return Err(error),
        Ok(_) => return Err(io::Error::other("frozen APK already exists")),
    }
    let parent = destination.parent().ok_or_else(|| io::Error::other("APK destination parent absent"))?;
    let source = source.canonicalize()?;
    let admitted = crate::products::read_file(&source, None)?;
    if admitted.sha256 != expected {
        return Err(io::Error::other("APK differs before copy admission"));
    }
    // Charge logical staged payload even when the filesystem can clone its physical blocks.
    // Admission is conservative: later copy, verification or publication failures never refund it.
    if let Some(budget) = budget { budget.charge(Class::Ordinary, admitted.bytes)?; }
    let staging = parent.join(".apk-copy");
    fs::DirBuilder::new().mode(0o700).create(&staging)?;
    let partial = staging.join("app.apk");
    let copied = (|| {
        // std::fs::copy can use filesystem cloning (fclonefileat on macOS), with portable fallback.
        // The destination is absent so a clone can create its own independent inode.
        fs::copy(&source, &partial)?;
        let file = crate::products::read_file(&partial, None)?;
        if file.sha256 != expected || file.bytes != admitted.bytes {
            return Err(io::Error::other("APK changed before snapshot publication"));
        }
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o600))?;
        File::open(&partial)?.sync_all()?;
        // Publish the verified staged inode without overwriting an existing file. This link is
        // internal to the new snapshot; the source file is never hard-linked into a run.
        fs::hard_link(&partial, destination)?;
        File::open(parent)?.sync_all()
    })();
    let cleanup = (|| {
        match fs::remove_file(&partial) {
            Ok(()) => (),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        fs::remove_dir(&staging)
    })();
    copied?;
    cleanup
}

#[cfg(test)]
#[path = "device_apk_tests.rs"]
mod tests;
