//! Bounded HTML media observations. Playback requests are not proof of audible output.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct History {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_count: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_limit_reached: Option<bool>,
    pub available: bool,
    pub controlled: bool,
    pub muted_output: bool,
    pub host_ms: f64,
    pub received: u64,
    pub dropped: u64,
    pub omitted: u64,
    pub faults: u64,
    pub players: Vec<Player>,
    pub events: Vec<Event>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<Flow>,
}
/// Cumulative native event counts survive eviction from the detailed event tail.
/// Repeated `playing` events include resumes; they are not distinct playback counts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Flow {
    pub unattributed: u64,
    pub sources: Vec<SourceFlow>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceFlow {
    pub source_id: u64,
    pub events: std::collections::BTreeMap<Kind, EventCount>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EventCount {
    pub count: u64,
    pub last_sequence: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Player {
    pub id: u64,
    #[serde(flatten)]
    pub state: Playback,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Playback {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_source_id: Option<u64>,
    pub current_time: Option<f64>,
    pub duration: Option<f64>,
    pub paused: bool,
    pub ended: bool,
    pub seeking: bool,
    pub r#loop: bool,
    pub ready_state: u8,
    pub network_state: u8,
    pub muted: bool,
    pub volume: Option<f64>,
    pub playback_rate: Option<f64>,
    pub error: Option<u8>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Event {
    pub sequence: u64,
    pub id: u64,
    pub kind: Kind,
    pub host_ms: f64,
    pub page_ms: f64,
    pub state: Playback,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    PlayCall,
    PauseCall,
    Playing,
    Pause,
    Ended,
    Error,
    Loadedmetadata,
    Seeking,
    Seeked,
    Ratechange,
    Durationchange,
    Emptied,
    Waiting,
}
impl Kind {
    fn is_flow(self) -> bool {
        matches!(
            self,
            Self::PlayCall
                | Self::PauseCall
                | Self::Playing
                | Self::Pause
                | Self::Ended
                | Self::Error
        )
    }
}
impl Event {
    fn flow_source(&self) -> Option<u64> {
        if self.kind == Kind::PlayCall {
            self.state.requested_source_id.or(self.state.source_id)
        } else {
            self.state.source_id
        }
    }
}
impl Flow {
    fn valid(&self, history: &History) -> bool {
        let Some(source_count) = history.source_count else {
            return false;
        };
        let mut sources = std::collections::HashSet::new();
        let mut sequences = std::collections::HashSet::new();
        let mut total = self.unattributed;
        if self.sources.len() > 64 || total > history.received {
            return false;
        }
        for source in &self.sources {
            if source.source_id == 0
                || source.source_id > source_count
                || !sources.insert(source.source_id)
                || source.events.is_empty()
            {
                return false;
            }
            for (kind, event) in &source.events {
                if !kind.is_flow()
                    || event.count == 0
                    || event.count > event.last_sequence
                    || event.last_sequence > history.received
                    || !sequences.insert(event.last_sequence)
                {
                    return false;
                }
                let retained = history
                    .events
                    .iter()
                    .filter(|e| e.kind == *kind && e.flow_source() == Some(source.source_id))
                    .count() as u64;
                if event.count < retained
                    || (event.last_sequence > history.dropped
                        && !history.events.iter().any(|e| {
                            e.sequence == event.last_sequence
                                && e.kind == *kind
                                && e.flow_source() == Some(source.source_id)
                        }))
                {
                    return false;
                }
                let Some(next) = total.checked_add(event.count) else {
                    return false;
                };
                total = next;
            }
        }
        if total > history.received
            || self.unattributed
                < history
                    .events
                    .iter()
                    .filter(|e| e.kind.is_flow() && e.flow_source().is_none())
                    .count() as u64
        {
            return false;
        }
        // A summary must not contradict an event still available in the detailed tail.
        history
            .events
            .iter()
            .filter(|e| e.kind.is_flow())
            .all(|e| match e.flow_source() {
                None => self.unattributed > 0,
                Some(id) => self
                    .sources
                    .iter()
                    .find(|s| s.source_id == id)
                    .and_then(|s| s.events.get(&e.kind))
                    .is_some_and(|count| count.last_sequence >= e.sequence),
            })
    }
}
impl Playback {
    fn valid(&self, sources: Option<u64>) -> bool {
        [self.source_id, self.requested_source_id]
            .into_iter()
            .all(|id| id.is_none_or(|id| sources.is_some_and(|count| id > 0 && id <= count)))
            && self.current_time.is_none_or(f64::is_finite)
            && self.duration.is_none_or(|v| v.is_finite() && v >= 0.0)
            && self
                .volume
                .is_none_or(|v| v.is_finite() && (0.0..=1.0).contains(&v))
            && self.playback_rate.is_none_or(f64::is_finite)
            && self.ready_state <= 4
            && self.network_state <= 3
            && self.error.is_none_or(|code| (1..=4).contains(&code))
    }
}
impl History {
    pub(crate) fn valid(&self) -> bool {
        let modern = self
            .document
            .as_ref()
            .is_some_and(|d| super::request::canonical_uuid(d))
            && self.source_count.is_some_and(|n| n <= 64)
            && self.source_limit_reached.is_some();
        let legacy = self.document.is_none()
            && self.source_count.is_none()
            && self.source_limit_reached.is_none();
        let mut ids = std::collections::HashSet::new();
        let time = |v: f64| v.is_finite() && v >= 0.0;
        (modern || legacy)
            && self.flow.as_ref().is_none_or(|flow| flow.valid(self))
            && !self.controlled
            && time(self.host_ms)
            && self.players.len() <= 8
            && self.events.len() <= 8
            && (self.available || (self.players.is_empty() && self.events.is_empty()))
            && self.received <= 9_007_199_254_740_991
            && self.received.checked_sub(self.dropped) == Some(self.events.len() as u64)
            && self.players.iter().all(|p| {
                p.id > 0 && p.id <= 8 && ids.insert(p.id) && p.state.valid(self.source_count)
            })
            && self.events.iter().enumerate().all(|(i, e)| {
                e.sequence == self.dropped + i as u64 + 1
                    && ids.contains(&e.id)
                    && time(e.host_ms)
                    && e.host_ms <= self.host_ms
                    && time(e.page_ms)
                    && e.state.valid(self.source_count)
            })
    }
}

#[cfg(test)]
#[path = "webview_media_tests.rs"]
mod tests;
