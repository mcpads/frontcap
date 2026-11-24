//! Android installation/process evidence, independent of the app's UI runtime.
use super::{
    android_app::Owner,
    request::{InvalidRequest, canonical_uuid},
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub fn restart_entry(owner: &Owner, url: Option<&str>)
    -> Result<Option<super::external_entry::Request>, InvalidRequest> {
    let Some(url) = url else { return Ok(None); };
    owner.validate()?;
    let request = super::external_entry::Request {
        target: super::external_entry::Target::Android {
            serial: owner.serial.clone(), package: owner.package.clone(),
        },
        generation: owner.generation.clone(), url: url.into(),
    };
    request.validate()?;
    Ok(Some(request))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Process {
    pub pid: u32,
    pub boot_id: String,
    pub start_ticks: u64,
    pub command: String,
}
impl Process {
    pub fn validate(&self, package: &str) -> Result<(), InvalidRequest> {
        if self.pid == 0
            || self.pid > i32::MAX as u32
            || !canonical_uuid(&self.boot_id)
            || self.start_ticks == 0
            || self.command != package
        {
            return Err(InvalidRequest(
                "Android process evidence differs from application",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    pub package: String,
    pub apk: PathBuf,
    pub apk_sha256: String,
    pub data: PathBuf,
    pub data_device: u64,
    pub data_inode: u64,
}
impl Installation {
    pub fn validate(&self, package: &str, apk_sha256: &str) -> Result<(), InvalidRequest> {
        let digest = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if self.package != package
            || self.apk_sha256 != apk_sha256
            || !digest(apk_sha256)
            || !self.apk.is_absolute()
            || !self.data.is_absolute()
            || self.apk == self.data
            || self.apk.extension().is_none_or(|s| s != "apk")
            || self.data.file_name().is_none_or(|s| s != package)
            || self.data_inode == 0
        {
            return Err(InvalidRequest("Android installation evidence differs"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub previous: Owner,
    pub owner: Owner,
    pub previous_process: Process,
    pub process: Process,
    pub installation: Installation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<super::external_entry::Accepted>,
}
impl Replacement {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.previous.validate()?;
        self.owner.validate()?;
        self.previous_process.validate(&self.previous.package)?;
        self.process.validate(&self.owner.package)?;
        self.installation
            .validate(&self.owner.package, &self.owner.apk_sha256)?;
        if let Some(entry) = &self.entry
            && restart_entry(&self.previous, Some(&entry.request.url))?.as_ref() != Some(&entry.request)
        {
            return Err(InvalidRequest("Android cold entry differs from previous owner"));
        }
        if self.previous.package != self.owner.package
            || self.previous.activity != self.owner.activity
            || self.previous.serial != self.owner.serial
            || self.previous.source_sha256 != self.owner.source_sha256
            || self.previous.apk_sha256 != self.owner.apk_sha256
            || self.previous.generation == self.owner.generation
            || self.previous_process.pid != self.previous.pid
            || self.process.pid != self.owner.pid
            || self.previous_process.boot_id != self.process.boot_id
            || self.previous_process == self.process
        {
            return Err(InvalidRequest("Android replacement lineage differs"));
        }
        Ok(())
    }
}

/// Sampled host focus and same-process identity; neither page lifetime nor service consumption.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transition {
    pub owner: Owner,
    pub requested: super::app_state::State,
    pub process_before: Process,
    pub process_after: Process,
    pub focused_before: String,
    pub focused_after: String,
    /// Background never reads app state after Home. Foreground samples the existing clock file.
    pub now_ns: Option<u64>,
}
impl Transition {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        use super::app_state::State;
        self.owner.validate()?;
        self.process_before.validate(&self.owner.package)?;
        self.process_after.validate(&self.owner.package)?;
        let component = format!("{}/{}", self.owner.package, self.owner.activity);
        let valid_component = |value: &str| value.split_once('/').is_some_and(|(package, activity)|
            super::android_capture::valid_app(package, activity));
        let outside = |value: &str| value.split('/').next() != Some(self.owner.package.as_str());
        let visibility = match self.requested {
            State::Background => self.focused_before == component && outside(&self.focused_after)
                && self.now_ns.is_none(),
            State::Foreground => outside(&self.focused_before) && self.focused_after == component
                && self.now_ns.is_some_and(|n| n <= i64::MAX as u64),
        };
        if self.process_before != self.process_after || self.process_before.pid != self.owner.pid
            || !valid_component(&self.focused_before) || !valid_component(&self.focused_after) || !visibility {
            return Err(InvalidRequest("Android same-process visibility transition is unconfirmed"));
        }
        Ok(())
    }
}
