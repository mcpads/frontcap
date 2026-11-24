use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Artifact {
    pub url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Pin {
    pub version: String,
    pub revision: u64,
    pub artifacts: BTreeMap<String, Artifact>,
}

#[derive(Debug, thiserror::Error)]
pub enum PinError {
    #[error("cannot read pin file {path}: {source}")]
    Read { path: String, source: std::io::Error },
    #[error("invalid pin file: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("artifact {platform} has an invalid sha256")]
    InvalidSha { platform: String },
    #[error("unsupported host platform {os}/{arch}")]
    UnsupportedPlatform { os: &'static str, arch: &'static str },
}

pub fn parse_pin(text: &str) -> Result<Pin, PinError> {
    let pin: Pin = toml::from_str(text)?;
    for (platform, artifact) in &pin.artifacts {
        let ok = artifact.sha256.len() == 64
            && artifact.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !ok {
            return Err(PinError::InvalidSha { platform: platform.clone() });
        }
    }
    Ok(pin)
}

pub fn load_pin(repo_root: &Path) -> Result<Pin, PinError> {
    let path = repo_root.join("browsers.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|source| PinError::Read { path: path.display().to_string(), source })?;
    parse_pin(&text)
}

pub fn current_platform() -> Result<&'static str, PinError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("mac-arm64"),
        ("macos", "x86_64") => Ok("mac-x64"),
        ("linux", "x86_64") => Ok("linux64"),
        ("linux", "aarch64") => Ok("linux-arm64"),
        (os, arch) => Err(PinError::UnsupportedPlatform { os, arch }),
    }
}

#[cfg(test)]
#[path = "pin_tests.rs"]
mod tests;
