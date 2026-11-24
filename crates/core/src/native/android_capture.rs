//! Sequential Android source/screen capture. It grants no observed-input authority.
use super::{
    android_views::Catalog,
    observation::Artifact,
    request::{InvalidRequest, canonical_uuid},
};
use serde::{Deserialize, Serialize};
pub const FILES: [&str; 13] = [
    "views-before.json",
    "views-after.json",
    "clock-before.json",
    "clock-after.json",
    "pid-before.txt",
    "pid-after.txt",
    "window-before.txt",
    "window-after.txt",
    "screen.png",
    "views-before-request.json",
    "views-after-request.json",
    "views-before-submission.txt",
    "views-after-submission.txt",
];
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub capture_id: String,
    pub package: String,
    pub activity: String,
    pub generation: String,
    pub pid: u32,
    pub source_sha256: String,
    pub now_ns: u64,
    pub before_request: String,
    pub after_request: String,
    pub catalog: Catalog,
    pub size: [u32; 2],
    pub started_monotonic_ns: u64,
    pub finished_monotonic_ns: u64,
    pub artifacts: Vec<Artifact>,
}
fn name(value: &str, class: bool) -> bool {
    value.len() <= 512
        && value.contains('.')
        && value.split('.').all(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || (class && c == '$'))
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || (class && c == '$'))
        })
}
/// Android application and fully qualified Activity names, safe for argument-based commands.
pub fn valid_app(package: &str, activity: &str) -> bool {
    name(package, false) && name(activity, true)
}
impl Capture {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        let hash = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        if !canonical_uuid(&self.capture_id)
            || !canonical_uuid(&self.generation)
            || !canonical_uuid(&self.before_request)
            || !canonical_uuid(&self.after_request)
            || self.before_request == self.after_request
            || !valid_app(&self.package, &self.activity)
            || !hash(&self.source_sha256)
            || self.pid == 0
            || self.pid > i32::MAX as u32
            || self.now_ns > i64::MAX as u64
            || self.started_monotonic_ns > self.finished_monotonic_ns
            || !self.catalog.window_focused
            || self.size.contains(&0)
            || self.size.iter().any(|n| *n > 8192)
            || u64::from(self.size[0]) * u64::from(self.size[1]) > 16000000
            || self.artifacts.len() != FILES.len()
        {
            return Err(InvalidRequest("invalid Android capture identity or bounds"));
        }
        self.catalog.validate()?;
        for file in FILES {
            let entries: Vec<_> = self.artifacts.iter().filter(|a| a.path == file).collect();
            if entries.len() != 1
                || !hash(&entries[0].sha256)
                || entries[0].bytes == 0
                || entries[0].bytes > 16 * 1024 * 1024
            {
                return Err(InvalidRequest("invalid Android capture inventory"));
            }
        }
        Ok(())
    }
}

/// A verified read whose source brackets disagree. Diagnostic pixels/targets grant no input authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unavailable {
    pub capture_id: String,
    pub generation: String,
    pub pid: u32,
    pub now_ns: u64,
    pub started_monotonic_ns: u64,
    pub finished_monotonic_ns: u64,
    pub manifest_sha256: String,
    pub reason: UnavailableReason,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum UnavailableReason { SourceChanged }
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Collection {
    Observed(Capture),
    Unavailable(Unavailable),
}
