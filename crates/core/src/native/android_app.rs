//! Acquired Android source View app identity, shared with recorder clients.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub package: String,
    pub activity: String,
    pub serial: String,
    pub generation: String,
    pub pid: u32,
    pub source_sha256: String,
    pub apk_sha256: String,
}
impl Owner {
    pub fn identity(&self) -> super::request::Identity {
        super::request::Identity {target:super::request::Target::AndroidViews,device:self.serial.clone(),generation:self.generation.clone(),pid:self.pid}
    }

    pub fn validate(&self) -> Result<(), super::request::InvalidRequest> {
        let hash = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        let port = self
            .serial
            .strip_prefix("emulator-")
            .and_then(|p| p.parse::<u16>().ok());
        if !super::android_capture::valid_app(&self.package, &self.activity)
            || !super::request::canonical_uuid(&self.generation)
            || self.pid == 0
            || self.pid > i32::MAX as u32
            || !hash(&self.source_sha256)
            || !hash(&self.apk_sha256)
            || !port.is_some_and(|p| {
                p > 0 && p.is_multiple_of(2) && self.serial == format!("emulator-{p}")
            })
        {
            return Err(super::request::InvalidRequest("invalid Android View owner"));
        }
        Ok(())
    }
}
