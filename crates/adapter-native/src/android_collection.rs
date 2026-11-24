//! Supervised read-only capture for an app whose emulator is already owned by the caller.
use crate::{
    android_capture::{Owner, verify_collection},
    process::exchange_typed_recorded,
};
use frontcap_core::native::android_capture::{Capture,Collection};
use serde::{Deserialize, Serialize};
use std::{io, path::Path, process::Command, sync::atomic::AtomicBool, time::Instant};

pub struct Collector<'a> {
    pub python: &'a Path,
    pub script: &'a Path,
    pub adb: &'a Path,
    pub serial: &'a str,
}
#[derive(Debug, thiserror::Error)]
#[error("Android capture: {message}; cleanup: {cleanup_failure:?}")]
pub struct Error {
    pub message: String,
    /// A failed process-group retirement must remain visible to the target's owner.
    pub cleanup_failure: Option<String>,
}
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self {
            message: error.to_string(),
            cleanup_failure: None,
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    manifest_sha256: String,
    #[serde(default)]
    source_changed: bool,
}

impl Collector<'_> {
    /// Reserve a fresh directory, collect once and independently verify all artifacts. Never
    /// acquire/adopt a device or retry an uncertain collection. The caller keeps device ownership
    /// and serializes this operation with clock and input operations for its whole lifetime.
    pub fn collect(
        &self,
        owner: &Owner<'_>,
        directory: &Path,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Capture, Error> {
        match self.collect_observation(owner,directory,deadline,cancelled)? {
            Collection::Observed(capture)=>Ok(capture),
            Collection::Unavailable(_)=>Err(io::Error::other("source changed during acquisition capture").into()),
        }
    }
    pub fn collect_observation(&self, owner: &Owner<'_>, directory: &Path, deadline: Instant, cancelled: &AtomicBool)
        -> Result<Collection, Error> {
        if ![self.python, self.script, self.adb, directory]
            .iter()
            .all(|p| p.is_absolute())
            || !self
                .serial
                .strip_prefix("emulator-")
                .and_then(|s| s.parse::<u16>().ok())
                .is_some_and(|port| port > 0 && port.is_multiple_of(2))
        {
            return Err(io::Error::other("invalid capture paths or emulator serial").into());
        }
        std::fs::create_dir(directory)?;
        let mut command = Command::new(self.python);
        command
            .arg(self.script)
            .arg("--adb")
            .arg(self.adb)
            .arg("--serial")
            .arg(self.serial)
            .arg("--directory")
            .arg(directory)
            .arg("--timeout")
            .arg(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_secs_f64()
                    .clamp(0.001, 300.0)
                    .to_string(),
            );
        let exchange = exchange_typed_recorded::<_, Reply>(
            &mut command,
            owner,
            deadline,
            cancelled,
            65536,
            &directory.join("exchange"),
        )?;
        if exchange.failure.is_some()
            || exchange.cleanup_failure.is_some()
            || exchange.evidence_failure.is_some()
        {
            return Err(Error {
                message: format!(
                    "operation: {:?}; evidence: {:?}",
                    exchange.failure, exchange.evidence_failure
                ),
                cleanup_failure: exchange.cleanup_failure,
            });
        }
        let reply = exchange
            .reply
            .ok_or_else(|| io::Error::other("missing capture reply"))?;
        Ok(verify_collection(
            &directory.join("capture"),
            owner,
            &reply.manifest_sha256, reply.source_changed,
        )?)
    }
}

/// Revalidate a completed collection without contacting a target or adopting its saved child.
pub fn verify_retained(directory: &Path, owner: &Owner<'_>) -> io::Result<Capture> {
    match verify_retained_observation(directory,owner)? {
        Collection::Observed(capture)=>Ok(capture),
        Collection::Unavailable(_)=>Err(io::Error::other("retained observation has no actionable capture")),
    }
}
pub fn verify_retained_observation(directory: &Path, owner: &Owner<'_>) -> io::Result<Collection> {
    let exchange: crate::process::Exchange<Reply> =
        crate::process::load_exchange(&directory.join("exchange/exchange.json"))?
            .ok_or_else(|| io::Error::other("missing retained collection exchange"))?;
    if exchange.failure.is_some()
        || exchange.cleanup_failure.is_some()
        || exchange.evidence_failure.is_some()
        || exchange.exit_code != Some(0)
    {
        return Err(io::Error::other(
            "collection did not complete with confirmed retirement/evidence",
        ));
    }
    let reply = exchange
        .reply
        .ok_or_else(|| io::Error::other("missing retained collection reply"))?;
    verify_collection(&directory.join("capture"), owner, &reply.manifest_sha256,reply.source_changed)
}
