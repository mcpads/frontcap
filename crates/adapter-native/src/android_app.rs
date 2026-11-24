//! Profile-selected Android source View app attachment on an owned emulator.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub launch_extras: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_url_extra: Option<String>,
    pub package: String,
    pub activity: String,
    pub capture_script: PathBuf,
    pub clock_script: PathBuf,
    pub edit_script: PathBuf,
    pub tap_script: PathBuf,
    pub scroll_script: PathBuf,
}
impl Profile {
    pub fn validate(&self) -> io::Result<()> {
        if !frontcap_core::native::android_capture::valid_app(&self.package, &self.activity)
            || !self.capture_script.is_absolute() || !self.clock_script.is_absolute() || !self.edit_script.is_absolute() || !self.tap_script.is_absolute() || !self.scroll_script.is_absolute()
        {
            return Err(io::Error::other("invalid Android View application profile"));
        }
        if self.launch_extras.len() > 16
            || self.launch_extras.iter().any(|(key, value)| !valid_extra_key(key)
                || value.len() > 2048 || value.chars().any(char::is_control))
            || self.service_url_extra.as_ref().is_some_and(|key| !valid_extra_key(key))
        {
            return Err(io::Error::other("invalid Android launch extras"));
        }
        Ok(())
    }
}
fn valid_extra_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64
        && key.as_bytes()[0].is_ascii_alphabetic()
        && key.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.".contains(&c))
}
/// adb joins shell arguments; quote each string value for the device shell.
pub(crate) fn launch_arguments(component: &str, profile: Option<&Profile>) -> Vec<String> {
    let mut args: Vec<String> = ["shell", "am", "start", "-W", "-n", component]
        .into_iter().map(str::to_owned).collect();
    append_launch_extras(&mut args, profile);
    args
}
pub(crate) fn append_launch_extras(args: &mut Vec<String>, profile: Option<&Profile>) {
    if let Some(profile) = profile {
        for (key, value) in &profile.launch_extras {
            args.extend(["--es".into(), key.clone(), format!("'{}'", value.replace('\'', "'\"'\"'"))]);
        }
    }
}
#[cfg(test)]
#[path = "android_app_tests.rs"]
mod tests;
pub use frontcap_core::native::android_app::Owner as Ready;
pub fn capture_owner(ready: &Ready) -> crate::android_capture::Owner<'_> {
    crate::android_capture::Owner {
        package: &ready.package,
        activity: &ready.activity,
        generation: &ready.generation,
        pid: ready.pid,
        source_sha256: &ready.source_sha256,
    }
}
// Acquisition reads the source clock identity and initial time. Later clock operations require receipts.
#[derive(Deserialize)]
pub(crate) struct InitialClock {
    pub generation: String,
    pub pid: u32,
    pub status: String,
    pub last_id: u64,
    pub now_ns: u64,
}
impl InitialClock {
    pub fn validate(&self) -> io::Result<()> {
        if self.status != "ready"
            || self.last_id != 0
            || self.now_ns != 0
            || self.pid == 0
            || self.pid > i32::MAX as u32
            || !frontcap_core::native::request::canonical_uuid(&self.generation)
        {
            return Err(io::Error::other(
                "Android View app has invalid identity or existing clock history",
            ));
        }
        Ok(())
    }
}
