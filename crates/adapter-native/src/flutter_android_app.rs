//! Frozen single-APK Flutter attachment and guest-process replacement evidence.
use crate::{flutter_ios_app::Endpoint, flutter_session::Ready};
use frontcap_core::native::{
    android_capture::valid_app,
    android_lifecycle::{Installation, Process},
    external_entry::Target,
    flutter_lifecycle::{AndroidReplacement, Replacement},
};
use serde::{Deserialize, Serialize};
use std::{io, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub product: PathBuf,
    pub apk_sha256: String,
    pub package: String,
    pub activity: String,
}
impl Launch {
    pub fn verify(&self) -> io::Result<()> {
        if !self.product.is_absolute()
            || self.product.extension().is_none_or(|s| s != "apk")
            || !valid_app(&self.package, &self.activity)
            || crate::products::read_file(&self.product, None)?.sha256 != self.apk_sha256
        {
            return Err(io::Error::other(
                "invalid or changed standalone Flutter APK",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forwarding {
    pub serial: String,
    pub host_port: u16,
    pub device_port: u16,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acquired {
    pub serial: String,
    pub package: String,
    pub activity: String,
    pub installation: Installation,
    pub process: Process,
    pub endpoint: Endpoint,
    pub forwarding: Forwarding,
}
impl Acquired {
    pub fn validate(&self, ready: &Ready) -> io::Result<()> {
        let target = Target::Android {
            serial: self.serial.clone(),
            package: self.package.clone(),
        };
        ready
            .owner
            .validate_external_target(&target)
            .map_err(io::Error::other)?;
        self.installation
            .validate(&self.package, &self.installation.apk_sha256)
            .map_err(io::Error::other)?;
        self.process
            .validate(&self.package)
            .map_err(io::Error::other)?;
        if !valid_app(&self.package, &self.activity)
            || ready.external_url_target.as_ref() != Some(&target)
            || ready.system_ui.is_some()
            || self.endpoint.generation != ready.owner.generation
            || self.endpoint.scenario != "session"
            || self.endpoint.port == 0
            || self.endpoint.token != ready.endpoint.token
            || self.endpoint.token.len() != 48
            || !self
                .endpoint
                .token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.forwarding.serial != self.serial
            || self.forwarding.host_port == 0
            || self.forwarding.host_port != ready.endpoint.port
            || self.forwarding.device_port != self.endpoint.port
        {
            return Err(io::Error::other(
                "installed Android Flutter evidence differs from ready owner",
            ));
        }
        Ok(())
    }
    pub fn replacement(
        &self,
        previous: &Self,
        before: &Ready,
        after: &Ready,
        entry: Option<frontcap_core::native::external_entry::Accepted>,
    ) -> io::Result<Replacement> {
        previous.validate(before)?;
        self.validate(after)?;
        if self.installation != previous.installation
            || self.activity != previous.activity
            || before.external_url_target != after.external_url_target
        {
            return Err(io::Error::other(
                "Flutter Android restart changed its installation or entry",
            ));
        }
        let result = AndroidReplacement {
            previous: before.owner.clone(),
            owner: after.owner.clone(),
            previous_process: previous.process.clone(),
            process: self.process.clone(),
            installation: self.installation.clone(),
            entry,
        };
        result.validate().map_err(io::Error::other)?;
        Ok(Replacement::Android(result))
    }
}
