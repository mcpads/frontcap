//! Bounded console observations from one selected Android page connection.
use serde::{Deserialize, Serialize};

pub const CAPACITY: usize = 32;
pub const TEXT_LIMIT: usize = 2048;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub sequence: u64,
    pub context: i64,
    /// CDP timestamp, not a controlled page-clock receipt.
    pub timestamp_ms: f64,
    pub level: String,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    /// A connection may observe several documents; context IDs retain that distinction.
    pub connection: String,
    pub received: u64,
    pub dropped: u64,
    pub closed: bool,
    pub failure: Option<String>,
    pub entries: Vec<Entry>,
}
impl History {
    pub fn valid(&self) -> bool {
        crate::ids::validate(&self.connection).is_ok()
            && self.entries.len() <= CAPACITY
            && self.received.checked_sub(self.dropped) == Some(self.entries.len() as u64)
            && self.failure.as_ref().is_none_or(|s| s.len() <= TEXT_LIMIT)
            && self.entries.iter().enumerate().all(|(i,e)|
                e.sequence == self.dropped + i as u64 + 1
                && e.timestamp_ms.is_finite() && e.context > 0
                && !e.level.is_empty() && e.level.len() <= 64 && e.text.len() <= TEXT_LIMIT)
    }
}
