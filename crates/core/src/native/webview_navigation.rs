//! Bounded document-local navigation and page focus, not a cross-document browser history.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Position {
    pub href: String,
    pub href_truncated: bool,
    pub focused: bool,
}
impl Position {
    fn valid(&self) -> bool {
        self.href.encode_utf16().count() <= 2048
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Navigation {
    pub document: String,
    pub current: Option<Position>,
    /// False if either History method could not be observed or was subsequently replaced.
    pub history_observed: bool,
    /// Sticky observation failure; known counts do not cover events after this failure.
    pub failure: Option<String>,
    pub received: u64,
    pub dropped: u64,
    pub events: Vec<Event>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub sequence: u64,
    pub elapsed_ms: f64,
    #[serde(flatten)]
    pub position: Position,
    pub kind: Kind,
    pub trusted: Option<bool>,
    pub persisted: Option<bool>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    PushState,
    ReplaceState,
    Popstate,
    Hashchange,
    Pageshow,
    Pagehide,
    Focus,
    Blur,
}
impl Navigation {
    pub fn valid(&self) -> bool {
        super::request::canonical_uuid(&self.document)
            && (self.current.is_some() || self.failure.is_some())
            && self.current.as_ref().is_none_or(Position::valid)
            && self
                .failure
                .as_ref()
                .is_none_or(|s| s.encode_utf16().count() <= 256)
            && self.events.len() <= 8
            && self.dropped.checked_add(self.events.len() as u64) == Some(self.received)
            && self.events.iter().enumerate().all(|(i, e)| {
                self.dropped.checked_add(i as u64 + 1) == Some(e.sequence)
                    && e.elapsed_ms.is_finite()
                    && e.elapsed_ms >= 0.0
                    && e.position.valid()
                    && e.trusted.is_none() == matches!(e.kind, Kind::PushState | Kind::ReplaceState)
                    && e.persisted.is_some() == matches!(e.kind, Kind::Pageshow | Kind::Pagehide)
            })
    }
}

#[cfg(test)]
#[path = "webview_navigation_tests.rs"]
mod tests;
