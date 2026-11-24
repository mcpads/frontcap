//! Hosts that keep unprivileged processes from using user namespaces (Ubuntu 23.10 and later, through
//! AppArmor) refuse the engine's namespace sandbox, and the engine will not start. As the distribution
//! does for Chrome and Chromium, a profile lets named binaries use user namespaces and confines nothing
//! else. It names only a tree that root alone can change, holding verified copies of the pinned engine,
//! so it grants nothing to a binary a user placed.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The kernel's switch; absent where the kernel has no such restriction.
const RESTRICT: &str = "/proc/sys/kernel/apparmor_restrict_unprivileged_userns";

/// Where engines the profile names are installed, by patch set and platform.
pub const ENGINES: &str = "/opt/frontcap/engines";

/// Where the profile is installed.
pub const PROFILE_PATH: &str = "/etc/apparmor.d/frontcap-engine";

/// The line the engine logs before aborting when no sandbox can start.
pub(crate) const NO_SANDBOX: &str = "No usable sandbox";

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("{path} must belong to root and be writable by root alone")]
    NotRootOwned { path: String },
    #[error("cannot install {path} ({source}): run as root, e.g. with sudo")]
    Write { path: String, source: std::io::Error },
    #[error("apparmor_parser could not load the profile: {0}")]
    Load(String),
    #[error("{0:?} is not a file inside the engine's directory")]
    Name(String),
}

/// Whether this host keeps unprivileged processes from using user namespaces.
pub fn userns_restricted() -> bool {
    std::fs::read_to_string(RESTRICT).is_ok_and(|v| v.trim() == "1")
}

/// Where `allow-sandbox` installs the engine of `patchset` for `platform`.
pub fn installed_dir(patchset: &str, platform: &str) -> PathBuf {
    Path::new(ENGINES).join(patchset).join(platform)
}

/// Where `allow-sandbox` installs the pinned unpatched browser of `version` for `platform`.
pub fn installed_base_dir(version: &str, platform: &str) -> PathBuf {
    Path::new(ENGINES).join(format!("{BASE}{version}")).join(platform)
}

/// The prefix that tells the pinned unpatched browser's copies from patched engines'.
const BASE: &str = "base-";

/// Every regular file under `dir`, relative to it.
pub fn files_under(dir: &Path) -> std::io::Result<Vec<String>> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(d) = dirs.pop() {
        for entry in std::fs::read_dir(&d)? {
            let path = entry?.path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.is_file() {
                files.push(path.strip_prefix(dir).expect("under dir").display().to_string());
            }
        }
    }
    files.sort();
    Ok(files)
}

/// The profile letting the installed engines use user namespaces.
pub fn profile() -> String {
    format!(
        "# frontcap's installed engines may use user namespaces for their sandbox; nothing else is confined.\n\
         abi <abi/4.0>,\n\
         include <tunables/global>\n\n\
         profile frontcap-engine {ENGINES}/*/*/{{headless_shell,chrome-headless-shell-*/chrome-headless-shell}} flags=(unconfined) {{\n  userns,\n\n  include if exists <local/frontcap-engine>\n}}\n"
    )
}

/// Checks that `path` and every directory above it belong to root and only root can write them, so no
/// user can replace what the profile names.
pub fn check_root_owned(path: &Path) -> Result<(), SandboxError> {
    for p in path.ancestors() {
        let meta = std::fs::symlink_metadata(p).map_err(|_| SandboxError::NotRootOwned { path: p.display().to_string() })?;
        if meta.uid() != 0 || meta.mode() & 0o022 != 0 || meta.file_type().is_symlink() {
            return Err(SandboxError::NotRootOwned { path: p.display().to_string() });
        }
    }
    Ok(())
}

/// Copies `files` from `from` to `to`, replacing an earlier copy, and removes the copies for the same
/// platform of other patch sets (of other versions, for the unpatched browser).
pub fn install_engine(from: &Path, files: &[String], to: &Path) -> Result<(), SandboxError> {
    let write = |path: &Path| {
        let path = path.display().to_string();
        move |source| SandboxError::Write { path, source }
    };
    if let Some(name) = files.iter().find(|n| !Path::new(n).components().all(|c| matches!(c, std::path::Component::Normal(_)))) {
        return Err(SandboxError::Name(name.clone()));
    }
    let staged = to.with_extension("partial");
    let _ = std::fs::remove_dir_all(&staged);
    for name in files {
        let target = staged.join(name);
        let parent = target.parent().expect("a file in the staged directory");
        std::fs::create_dir_all(parent).map_err(write(parent))?;
        std::fs::copy(from.join(name), &target).map_err(write(&target))?;
        let mode = std::fs::metadata(&target).map_err(write(&target))?.permissions().mode() & 0o755;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).map_err(write(&target))?;
    }
    if let Some(platform) = to.file_name()
        && let Some(engines) = to.parent().and_then(Path::parent)
    {
        let base = |p: &Path| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(BASE));
        let kind = to.parent().is_some_and(base);
        for entry in std::fs::read_dir(engines).map_err(write(engines))?.flatten() {
            if base(&entry.path()) != kind {
                continue;
            }
            let other = entry.path().join(platform);
            if other != to && other.is_dir() {
                std::fs::remove_dir_all(&other).map_err(write(&other))?;
            }
            // A patch set with no platform left goes too (this one holds the staged copy).
            let _ = std::fs::remove_dir(entry.path());
        }
    }
    let _ = std::fs::remove_dir_all(to);
    std::fs::rename(&staged, to).map_err(write(to))
}

/// Installs the profile and loads it.
pub fn install_profile() -> Result<(), SandboxError> {
    std::fs::write(PROFILE_PATH, profile()).map_err(|source| SandboxError::Write { path: PROFILE_PATH.into(), source })?;
    let out = std::process::Command::new("apparmor_parser").args(["-r", PROFILE_PATH]).output()
        .map_err(|e| SandboxError::Load(e.to_string()))?;
    if !out.status.success() {
        return Err(SandboxError::Load(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(())
}

#[cfg(test)]
#[path = "sandbox_tests.rs"]
mod tests;
