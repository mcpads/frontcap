//! Standalone products and same-install evidence for the two Flutter host platforms.
use crate::{flutter_android_app as android, flutter_ios_app as ios, flutter_session::Ready};
use frontcap_core::native::{flutter::Platform, flutter_lifecycle::Replacement};
use serde::{Deserialize, Serialize};
use std::io;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Launch {
    Ios(ios::Launch),
    Android(android::Launch),
}
impl Launch {
    pub fn verify(&self, platform: &Platform) -> io::Result<()> {
        match (self, platform) {
            (Self::Ios(app), Platform::Ios) => app.verify(),
            (Self::Android(app), Platform::Android) => app.verify(),
            _ => Err(io::Error::other(
                "standalone Flutter product platform differs",
            )),
        }
    }
    pub fn validate_acquired(&self, acquired: &Acquired, ready: &Ready) -> io::Result<()> {
        acquired.validate(ready)?;
        match (self, acquired) {
            (Self::Ios(_), Acquired::Ios(_)) => Ok(()),
            (Self::Android(app), Acquired::Android(actual))
                if app.package == actual.package
                    && app.activity == actual.activity
                    && app.apk_sha256 == actual.installation.apk_sha256 =>
            {
                Ok(())
            }
            _ => Err(io::Error::other(
                "installed Flutter app differs from frozen product",
            )),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Acquired {
    Ios(ios::Acquired),
    Android(android::Acquired),
}
impl Acquired {
    pub fn validate(&self, ready: &Ready) -> io::Result<()> {
        match self {
            Self::Ios(app) => app.validate(ready),
            Self::Android(app) => app.validate(ready),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Restarted {
    pub previous: Acquired,
    pub current: Acquired,
    pub ready: Ready,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<frontcap_core::native::external_entry::Accepted>,
}
impl Restarted {
    pub fn validate(&self, previous: &Acquired, ready: &Ready) -> io::Result<Replacement> {
        if &self.previous != previous {
            return Err(io::Error::other("Flutter restart previous app differs"));
        }
        match (previous, &self.current) {
            (Acquired::Ios(before), Acquired::Ios(after)) => {
                let result = ios::Restarted {
                    previous: before.clone(),
                    current: after.clone(),
                    ready: self.ready.clone(),
                }.validate(before, ready)?;
                let Replacement::Ios(mut replacement) = result else { unreachable!() };
                replacement.entry = self.entry.clone();
                replacement.validate().map_err(io::Error::other)?;
                Ok(Replacement::Ios(replacement))
            },
            (Acquired::Android(before), Acquired::Android(after)) => {
                after.replacement(before, ready, &self.ready, self.entry.clone())
            }
            _ => Err(io::Error::other("Flutter restart changed host platform")),
        }
    }
}
