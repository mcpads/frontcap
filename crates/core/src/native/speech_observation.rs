//! Passive, process-wide iOS speech evidence. Requests never prove native completion.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct History {
    pub process: String,
    pub pid: u32,
    pub api_observed: bool,
    pub delegate_observed: bool,
    pub received: u64,
    pub dropped: u64,
    pub truncated: bool,
    pub faults: u64,
    pub utterance_limit_reached: bool,
    pub events: Vec<Event>,
    pub utterances: Vec<Utterance>,
    pub synthesizers: Vec<Synthesizer>,
    pub omitted_synthesizers: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Synthesizer {
    pub id: u64,
    pub speaking: bool,
    pub paused: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Event {
    pub sequence: u64,
    pub synthesizer: u64,
    pub utterance: Option<u64>,
    pub kind: Kind,
    pub speaking: bool,
    pub paused: bool,
    pub accepted: Option<bool>,
    pub implemented_callbacks: Option<Callbacks>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Utterance {
    pub id: u64,
    pub requests: u64,
    pub starts: u64,
    pub finishes: u64,
    pub cancellations: u64,
    pub failures: u64,
    pub last_sequence: u64,
    pub last_kind: Kind,
    pub implemented_callbacks: Option<Callbacks>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Callbacks {
    pub start: bool,
    pub finish: bool,
    pub cancel: bool,
    pub pause: bool,
    pub resume: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Request,
    Returned,
    Failed,
    StopRequest,
    StopReturned,
    PauseRequest,
    PauseReturned,
    ResumeRequest,
    ResumeReturned,
    Started,
    Finished,
    Cancelled,
    Paused,
    Resumed,
}
impl Kind {
    fn delegate(self) -> bool {
        matches!(
            self,
            Self::Started | Self::Finished | Self::Cancelled | Self::Paused | Self::Resumed
        )
    }
    fn accepted(self) -> bool {
        matches!(
            self,
            Self::StopReturned | Self::PauseReturned | Self::ResumeReturned
        )
    }
    fn utterance(self) -> bool {
        matches!(self, Self::Request | Self::Returned | Self::Failed) || self.delegate()
    }
}
impl History {
    pub(crate) fn valid(&self) -> bool {
        if !super::request::canonical_uuid(&self.process)
            || self.pid == 0
            || self.pid > i32::MAX as u32
            || self.received > 512
            || self.events.len() > 32
            || self.utterances.len() > 64
            || self.received.checked_sub(self.dropped) != Some(self.events.len() as u64)
        {
            return false;
        }
        let mut synthesizers = std::collections::HashSet::new();
        if self.synthesizers.len() > 8
            || self.omitted_synthesizers > 512
            || !self
                .synthesizers
                .iter()
                .all(|s| s.id > 0 && s.id <= 512 && synthesizers.insert(s.id))
        {
            return false;
        }
        let mut ids = std::collections::HashSet::new();
        let mut last_sequences = std::collections::HashSet::new();
        for u in &self.utterances {
            let sum = u
                .requests
                .checked_add(u.starts)
                .and_then(|n| n.checked_add(u.finishes))
                .and_then(|n| n.checked_add(u.cancellations))
                .and_then(|n| n.checked_add(u.failures));
            if u.id == 0
                || u.id > 64
                || !ids.insert(u.id)
                || !u.last_kind.utterance()
                || !last_sequences.insert(u.last_sequence)
                || ((u.requests > 0 || u.failures > 0) && !self.api_observed)
                || ((u.starts > 0 || u.finishes > 0 || u.cancellations > 0)
                    && !self.delegate_observed)
                || u.last_sequence == 0
                || u.last_sequence > self.received
                || sum.is_none_or(|n| n > u.last_sequence)
                || (u.requests > 0 && u.implemented_callbacks.is_none())
            {
                return false;
            }
            for (kind, count) in [
                (Kind::Request, u.requests),
                (Kind::Started, u.starts),
                (Kind::Finished, u.finishes),
                (Kind::Cancelled, u.cancellations),
                (Kind::Failed, u.failures),
            ] {
                if count
                    < self
                        .events
                        .iter()
                        .filter(|e| e.utterance == Some(u.id) && e.kind == kind)
                        .count() as u64
                {
                    return false;
                }
            }
            if u.last_sequence > self.dropped
                && !self.events.iter().any(|e| {
                    e.sequence == u.last_sequence
                        && e.utterance == Some(u.id)
                        && e.kind == u.last_kind
                })
            {
                return false;
            }
        }
        self.events.iter().enumerate().all(|(i, e)| {
            e.sequence == self.dropped + i as u64 + 1
                && e.synthesizer > 0
                && e.synthesizer <= 512
                && (if e.kind.delegate() {
                    self.delegate_observed
                } else {
                    self.api_observed
                })
                && e.accepted.is_some() == e.kind.accepted()
                && e.implemented_callbacks.is_some() == (e.kind == Kind::Request)
                && (if e.kind.utterance() {
                    e.utterance.is_none_or(|id| ids.contains(&id))
                } else {
                    e.utterance.is_none()
                })
                && e.utterance.is_none_or(|id| {
                    self.utterances
                        .iter()
                        .any(|u| u.id == id && u.last_sequence >= e.sequence)
                })
        })
    }
}

#[cfg(test)]
#[path = "speech_observation_tests.rs"]
mod tests;
