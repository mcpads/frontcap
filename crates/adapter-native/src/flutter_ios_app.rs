//! Frozen standalone Flutter product, launched by the existing owned SDK worker.
pub use crate::products::BundleInventory as Inventory;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    path::PathBuf,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub product: PathBuf,
    pub inventory: Inventory,
}
impl Launch {
    pub fn verify(&self) -> io::Result<()> {
        if !self.product.is_absolute() || self.product.extension().is_none_or(|s| s != "app") {
            return Err(io::Error::other("invalid standalone Flutter product path"));
        }
        if !self.inventory.files.contains_key("Info.plist") {
            return Err(io::Error::other("standalone Flutter product has no Info.plist"));
        }
        self.inventory.verify(&self.product)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LibraryMode { pub product_executable: bool, pub installed_executable: bool }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Containers {
    pub app: PathBuf, pub data: PathBuf, pub data_device: u64, pub data_inode: u64,
    pub installed_library_modes: BTreeMap<String, LibraryMode>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    pub generation: String, pub token: String, pub port: u16, pub scenario: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acquired {
    pub simulator: String, pub bundle: String, pub containers: Containers,
    pub process: frontcap_core::native::flutter_lifecycle::Process, pub endpoint: Endpoint,
}
impl Acquired {
    pub fn validate(&self, ready: &crate::flutter_session::Ready) -> io::Result<()> {
        ready.owner.validate().map_err(io::Error::other)?;
        self.process.validate().map_err(io::Error::other)?;
        let target = frontcap_core::native::external_entry::Target::Ios {
            simulator: self.simulator.to_ascii_lowercase(), bundle: self.bundle.clone(),
        };
        ready.owner.validate_external_target(&target).map_err(io::Error::other)?;
        if ready.external_url_target.as_ref() != Some(&target)
            || !self.simulator.eq_ignore_ascii_case(&ready.owner.device)
            || self.endpoint.generation != ready.owner.generation || self.endpoint.port == 0
            || self.endpoint.port != ready.endpoint.port || self.endpoint.token != ready.endpoint.token
            || self.endpoint.token.len() != 48 || !self.endpoint.token.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            || self.endpoint.scenario != "session" || !self.containers.app.is_absolute() || !self.containers.data.is_absolute()
        { return Err(io::Error::other("installed Flutter evidence differs from acquired owner")); }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restarted {
    pub previous: Acquired, pub current: Acquired, pub ready: crate::flutter_session::Ready,
}
impl Restarted {
    pub fn validate(&self, previous: &Acquired, ready: &crate::flutter_session::Ready)
        -> io::Result<frontcap_core::native::flutter_lifecycle::Replacement> {
        previous.validate(ready)?; self.current.validate(&self.ready)?;
        if &self.previous != previous || self.current.containers != previous.containers
            || self.current.bundle != previous.bundle || self.ready.system_ui != ready.system_ui
            || self.ready.external_url_target != ready.external_url_target
        { return Err(io::Error::other("Flutter restart changed its installation or system runner")); }
        let replacement = frontcap_core::native::flutter_lifecycle::IosReplacement {
            previous: ready.owner.clone(), owner: self.ready.owner.clone(), previous_process: previous.process.clone(),
            process: self.current.process.clone(), installation: frontcap_core::native::flutter_lifecycle::Installation {
                bundle: previous.bundle.clone(), app: previous.containers.app.clone(), data: previous.containers.data.clone(),
                data_device: previous.containers.data_device, data_inode: previous.containers.data_inode,
            },
            entry: None,
        };
        replacement.validate().map_err(io::Error::other)?;
        Ok(frontcap_core::native::flutter_lifecycle::Replacement::Ios(replacement))
    }
}

#[cfg(test)]
#[path = "flutter_ios_app_tests.rs"]
mod tests;
