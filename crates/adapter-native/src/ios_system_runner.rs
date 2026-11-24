//! Frozen system-only XCTest products shared by native and Flutter hosts.
use serde::{Serialize, Deserialize};
use std::{io, path::PathBuf};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    pub xcodebuild: PathBuf,
    pub products: PathBuf,
    pub manifest: crate::products::ProductsManifest,
}
impl Launch {
    pub fn validate(&self) -> io::Result<()> {
        self.manifest.validate().map_err(io::Error::other)?;
        if !self.xcodebuild.is_absolute()
            || !self.products.is_absolute()
            || self.manifest.role != crate::products::ProductRole::ExternalRunner
            || self.manifest.names.app != "SystemUI"
            || self.manifest.names.tests != "SystemUITests"
        {
            return Err(io::Error::other("invalid frozen system runner products"));
        }
        Ok(())
    }
}
