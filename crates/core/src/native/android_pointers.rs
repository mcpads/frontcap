//! Bounded passive page-delivery observations, distinct from native injection receipts.
use serde::{Deserialize, Serialize};
use std::io;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub sequence: u64,
    pub kind: String,
    pub id: i64,
    pub trusted: bool,
    pub x: f64,
    pub y: f64,
    #[serde(rename = "pageMs")]
    pub page_ms: f64,
    pub active: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub document: String,
    pub received: u64,
    pub dropped: u64,
    pub active: u64,
    #[serde(rename = "maximumActive")]
    pub maximum_active: u64,
    pub events: Vec<Event>,
}
impl Snapshot {
    pub fn validate(&self) -> io::Result<()> {
        if !crate::native::request::canonical_uuid(&self.document)
            || self.received > 9_007_199_254_740_991
            || self.dropped > self.received
            || self.events.len() > 8
            || self.received - self.dropped != self.events.len() as u64
            || self.active > self.maximum_active
            || self.maximum_active > self.received
            || self.events.iter().enumerate().any(|(index, e)| {
                e.sequence != self.dropped + index as u64 + 1
                    || !matches!(
                        e.kind.as_str(),
                        "pointerdown" | "pointermove" | "pointerup" | "pointercancel"
                    )
                    || !e.x.is_finite()
                    || !e.y.is_finite()
                    || !e.page_ms.is_finite()
                    || e.page_ms < 0.0
                    || e.active > self.maximum_active
            })
            || self.events.last().is_some_and(|e| e.active != self.active)
        {
            return Err(io::Error::other("invalid bounded pointer observation"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "android_pointers_tests.rs"]
mod tests;
