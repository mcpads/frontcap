use crate::pin::Pin;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("no artifact pinned for platform {0}")]
    NoArtifact(String),
    #[error("io error at {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("download failed for {url}: curl exited with {status}")]
    Download { url: String, status: String },
    #[error("sha256 mismatch for {path}: expected {expected}, got {actual}")]
    ShaMismatch { path: String, expected: String, actual: String },
    #[error("cannot extract {path}: {message}")]
    Extract { path: String, message: String },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> InstallError + '_ {
    move |source| InstallError::Io { path: path.display().to_string(), source }
}

pub fn binary_path(repo_root: &Path, version: &str, platform: &str) -> PathBuf {
    repo_root
        .join("browsers")
        .join(version)
        .join(platform)
        .join(format!("chrome-headless-shell-{platform}"))
        .join("chrome-headless-shell")
}

pub fn sha256_hex(path: &Path) -> Result<String, InstallError> {
    // Read in pieces: an engine binary is hundreds of megabytes.
    let mut file = std::fs::File::open(path).map_err(io(path))?;
    let mut hasher = Sha256::new();
    let mut piece = vec![0u8; 1 << 20];
    loop {
        match std::io::Read::read(&mut file, &mut piece) {
            Ok(0) => break,
            Ok(n) => hasher.update(&piece[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io(path)(e)),
        }
    }
    Ok(hasher.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

pub fn verify_sha256(path: &Path, expected: &str) -> Result<(), InstallError> {
    let actual = sha256_hex(path)?;
    if actual != expected {
        return Err(InstallError::ShaMismatch {
            path: path.display().to_string(),
            expected: expected.to_string(),
            actual,
        });
    }
    Ok(())
}

/// Name of the completion marker written after a verified extraction; it holds the archive's SHA-256.
const MARKER: &str = ".installed";

/// The pinned browser's executable when it is installed: where the host restricts user namespaces, the
/// copy `allow-sandbox` installed, since only it may start its sandbox.
pub fn installed_binary(repo_root: &Path, pin: &Pin, platform: &str) -> Option<PathBuf> {
    let artifact = pin.artifacts.get(platform)?;
    let complete = |dir: &Path| std::fs::read_to_string(dir.join(MARKER)).is_ok_and(|m| m.trim() == artifact.sha256);
    let executable = |dir: &Path| dir.join(format!("chrome-headless-shell-{platform}")).join("chrome-headless-shell");
    let dir = repo_root.join("browsers").join(&pin.version).join(platform);
    if crate::sandbox::userns_restricted() {
        let copy = crate::sandbox::installed_base_dir(&pin.version, platform);
        if complete(&copy) && executable(&copy).is_file() {
            return Some(executable(&copy));
        }
    }
    (complete(&dir) && executable(&dir).is_file()).then(|| executable(&dir))
}

pub fn install(repo_root: &Path, pin: &Pin, platform: &str) -> Result<PathBuf, InstallError> {
    let artifact = pin
        .artifacts
        .get(platform)
        .ok_or_else(|| InstallError::NoArtifact(platform.to_string()))?;
    if let Some(binary) = installed_binary(repo_root, pin, platform) {
        return Ok(binary);
    }
    let dir = repo_root.join("browsers").join(&pin.version).join(platform);
    let binary = binary_path(repo_root, &pin.version, platform);
    // Anything else at the destination is an incomplete or foreign install; it is never trusted.
    let parent = dir.parent().expect("platform dir has a parent");
    std::fs::create_dir_all(parent).map_err(io(parent))?;
    let staging = parent.join(format!(".{platform}.partial-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(io(&staging))?;
    }
    std::fs::create_dir_all(&staging).map_err(io(&staging))?;
    let zip_path = staging.join("download.zip");
    let status = Command::new("curl")
        .args(["--fail", "--location", "--silent", "--show-error", "--output"])
        .arg(&zip_path)
        .arg(&artifact.url)
        .status()
        .map_err(io(&zip_path))?;
    if !status.success() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(InstallError::Download { url: artifact.url.clone(), status: status.to_string() });
    }
    if let Err(e) = verify_sha256(&zip_path, &artifact.sha256) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    let extracted = (|| {
        let file = std::fs::File::open(&zip_path).map_err(io(&zip_path))?;
        let mut archive = zip::ZipArchive::new(file).map_err(|e| InstallError::Extract {
            path: zip_path.display().to_string(),
            message: e.to_string(),
        })?;
        archive.extract(&staging).map_err(|e| InstallError::Extract {
            path: zip_path.display().to_string(),
            message: e.to_string(),
        })?;
        std::fs::remove_file(&zip_path).map_err(io(&zip_path))?;
        let staged_binary = staging.join(format!("chrome-headless-shell-{platform}")).join("chrome-headless-shell");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&staged_binary).map_err(io(&staged_binary))?.permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&staged_binary, perm).map_err(io(&staged_binary))?;
        }
        let staged_marker = staging.join(MARKER);
        std::fs::write(&staged_marker, &artifact.sha256).map_err(io(&staged_marker))
    })();
    if let Err(e) = extracted {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(io(&dir))?;
    }
    std::fs::rename(&staging, &dir).map_err(io(&dir))?;
    Ok(binary)
}

#[cfg(test)]
#[path = "install_tests.rs"]
mod tests;
