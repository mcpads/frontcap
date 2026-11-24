//! Bounded frame-navigation events from one Android CDP connection, not control authority.
use serde::{Deserialize, Serialize};

pub const CAPACITY: usize = 32;
pub const URL_LIMIT: usize = 2048;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub sequence: u64,
    pub frame: String,
    pub parent: Option<String>,
    /// CDP loader identity, distinct from the platform's controlled-document UUID.
    pub loader: String,
    pub url: String,
    pub url_truncated: bool,
    pub restored_from_cache: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub connection: String,
    pub received: u64,
    pub dropped: u64,
    pub closed: bool,
    pub failure: Option<String>,
    pub entries: Vec<Entry>,
}
impl History {
    pub fn valid(&self) -> bool {
        let identity = |s: &str| !s.is_empty() && s.len()<=256;
        crate::ids::validate(&self.connection).is_ok()
            && self.entries.len()<=CAPACITY
            && self.received.checked_sub(self.dropped)==Some(self.entries.len() as u64)
            && self.failure.as_ref().is_none_or(|s|s.len()<=URL_LIMIT)
            && self.entries.iter().enumerate().all(|(i,e)|
                self.dropped.checked_add(i as u64+1)==Some(e.sequence)
                && identity(&e.frame) && identity(&e.loader)
                && e.parent.as_ref().is_none_or(|p|identity(p)) && e.url.len()<=URL_LIMIT)
    }
}
