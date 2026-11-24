//! Document-bound console calls observed by the platform's main-realm attachment.
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub document: String,
    pub received: u64,
    pub dropped: u64,
    pub failure: Option<String>,
    pub entries: Vec<Entry>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Entry {
    pub sequence: u64,
    pub elapsed_ms: f64,
    pub level: Level,
    pub text: String,
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level { Log, Debug, Info, Warn, Error }
impl History {
    pub(crate) fn valid(&self) -> bool {
        super::request::canonical_uuid(&self.document)
            && self.received <= 9_007_199_254_740_991
            && self.entries.len() <= 32
            && self.received.checked_sub(self.dropped) == Some(self.entries.len() as u64)
            && self.failure.as_ref().is_none_or(|s| s.len() <= 256)
            && self.entries.iter().enumerate().all(|(i,e)|
                e.sequence == self.dropped + i as u64 + 1
                && e.elapsed_ms.is_finite() && e.elapsed_ms >= 0.0
                && e.text.encode_utf16().count() <= 1024)
    }
}
#[cfg(test)]
#[path = "webview_console_tests.rs"]
mod tests;
