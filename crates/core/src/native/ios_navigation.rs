//! Main-frame document-start notifications retained outside their WKWebView documents.
use serde::{Deserialize, Serialize};
use super::request::canonical_uuid;

pub const CAPACITY: usize = 32;
pub const URL_LIMIT: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub sequence: u64,
    pub page: String,
    pub document: String,
    pub url: String,
    pub url_truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub received: u64,
    pub dropped: u64,
    pub failure: Option<String>,
    pub entries: Vec<Entry>,
}
impl History {
    pub fn valid(&self) -> bool {
        self.entries.len() <= CAPACITY
            && self.received.checked_sub(self.dropped) == Some(self.entries.len() as u64)
            && self.failure.as_ref().is_none_or(|s| s.len() <= URL_LIMIT)
            && self.entries.iter().enumerate().all(|(i,e)|
                self.dropped.checked_add(i as u64 + 1) == Some(e.sequence)
                && canonical_uuid(&e.page) && canonical_uuid(&e.document) && e.url.len() <= URL_LIMIT)
    }
    /// Dropped entries remain explicit; overlapping original evidence cannot be rewritten.
    pub fn follows(&self, old: &Self) -> bool {
        self.valid() && old.valid() && self.received >= old.received && self.dropped >= old.dropped
            && old.failure.as_ref().is_none_or(|f| self.failure.as_ref() == Some(f))
            && old.entries.iter().filter(|e| e.sequence > self.dropped)
                .all(|e| self.entries.iter().any(|n| n == e))
    }
}

#[cfg(test)]
#[path = "ios_navigation_tests.rs"]
mod tests;
