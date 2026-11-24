//! Revalidate retained terminal receipt bytes without contacting a target or replaying a request.
use crate::request::{
    ClockRejection, ClockRequest, Completion,
};
use frontcap_core::canonical::sha256_bytes_hex;
use serde::Deserialize;
use std::{io, path::Path};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClockReceipt {
    request: ClockRequest,
    generation: String,
    pid: u32,
    last_id: u64,
    status: String,
    before_ns: u64,
    now_ns: u64,
    applied: bool,
    error: Option<ClockRejection>,
    #[serde(rename = "replayed")]
    _replayed: Option<bool>,
    #[serde(rename = "observation")]
    // Clock diagnostics are app-owned and are not UI observation evidence.
    _observation: Option<serde::de::IgnoredAny>,
}
pub fn verify(root: &Path, expected: &Completion) -> io::Result<()> {
    let digest = match expected {
        Completion::ViewScroll { receipt_sha256, .. }
        | Completion::ViewTap { receipt_sha256, .. }
        | Completion::ViewText { receipt_sha256, .. }
        | Completion::Clock { receipt_sha256, .. }
        | Completion::ClockRejected { receipt_sha256, .. } => receipt_sha256,
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err(io::Error::other("invalid receipt digest"));
    }
    let path = root.join(format!("{digest}.json"));
    let metadata = std::fs::symlink_metadata(&path)?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
        return Err(io::Error::other("invalid receipt file/size"));
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() > 16 * 1024 * 1024 || sha256_bytes_hex(&bytes) != *digest {
        return Err(io::Error::other(
            "receipt bytes differ from committed evidence",
        ));
    }
    let actual = match expected {
        Completion::ViewScroll {..} => Completion::ViewScroll {
            receipt:serde_json::from_slice(&bytes)?,receipt_sha256:digest.clone(),
        },
        Completion::ViewTap {..} => Completion::ViewTap {
            receipt:serde_json::from_slice(&bytes)?,receipt_sha256:digest.clone(),
        },
        Completion::ViewText {..} => Completion::ViewText {
            receipt:serde_json::from_slice(&bytes)?,receipt_sha256:digest.clone(),
        },
        Completion::Clock { .. } | Completion::ClockRejected { .. } => {
            let r: ClockReceipt = serde_json::from_slice(&bytes)?;
            if r.status == "ok" && r.error.is_none() {
                Completion::Clock {
                    request: r.request,
                    generation: r.generation,
                    pid: r.pid,
                    last_id: r.last_id,
                    before_ns: r.before_ns,
                    now_ns: r.now_ns,
                    applied: r.applied,
                    receipt_sha256: digest.clone(),
                }
            } else if r.status == "rejected" && r.error.is_some() {
                Completion::ClockRejected {
                    request: r.request,
                    generation: r.generation,
                    pid: r.pid,
                    last_id: r.last_id,
                    before_ns: r.before_ns,
                    now_ns: r.now_ns,
                    applied: r.applied,
                    error: r.error.unwrap(),
                    receipt_sha256: digest.clone(),
                }
            } else {
                return Err(io::Error::other(
                    "receipt is not a recognized terminal clock outcome",
                ));
            }
        }
    };
    if &actual != expected {
        return Err(io::Error::other(
            "receipt fields differ from committed completion",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "receipt_tests.rs"]
mod tests;
