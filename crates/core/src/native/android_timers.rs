//! Shared bounded Android timer actions and completion evidence.
use serde::{Deserialize, Serialize};
use std::io;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Pause,
    Advance { milliseconds: u32 },
}
impl Action {
    pub fn validate(&self) -> io::Result<()> {
        if let Self::Advance { milliseconds } = self
            && (*milliseconds == 0 || *milliseconds > 60_000)
        {
            return Err(io::Error::other(
                "timer budget must be 1..60000 milliseconds",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub document: String,
    pub action: Action,
    pub ticks_base: f64,
    pub budget_expired: bool,
}

/// Retained policy evidence, not a count of pending app timers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum State {
    Pending {
        document: String,
        request: crate::ids::RequestId,
        action: Action,
    },
    Paused {
        completion: Completion,
    },
}

/// Main-realm function timer registrations. Coverage gaps never imply no pending app work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Activity {
    pub document: String,
    pub sampled_ms: Option<f64>,
    pub methods_observed: bool,
    pub failure: Option<String>,
    pub registered: u64,
    pub invoked: u64,
    pub canceled: u64,
    pub executing: u64,
    pub untracked_registrations: u64,
    pub uncertain_cancellations: u64,
    pub tracked: usize,
    pub omitted: usize,
    pub timers: Vec<Timer>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Timer {
    pub id: u32,
    pub registration: u64,
    pub kind: TimerKind,
    pub registered_ms: f64,
    /// Numeric caller argument only; not an effective deadline or normalized engine delay.
    pub requested_delay_ms: Option<f64>,
    pub calls: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerKind {
    Timeout,
    Interval,
}
impl Activity {
    pub fn valid(&self) -> bool {
        let mut ids = std::collections::HashSet::new();
        let mut previous = 0;
        super::request::canonical_uuid(&self.document)
            && (self.sampled_ms.is_some() || self.failure.is_some())
            && self.sampled_ms.is_none_or(|t| t.is_finite() && t >= 0.0)
            && self.failure.as_ref().is_none_or(|s| s.len() <= 256)
            && [
                self.registered,
                self.invoked,
                self.canceled,
                self.executing,
                self.untracked_registrations,
                self.uncertain_cancellations,
            ]
            .into_iter()
            .all(|n| n <= 9_007_199_254_740_991)
            && self.canceled <= self.registered
            && self.untracked_registrations <= self.registered
            && self.executing <= self.invoked
            && self.tracked <= 4096
            && self.tracked as u64 <= self.registered
            && self.timers.len() <= 32
            && self.omitted.checked_add(self.timers.len()) == Some(self.tracked)
            && self.timers.iter().all(|timer| {
                let ordered = timer.registration > previous;
                previous = timer.registration;
                ordered
                    && timer.registration <= self.registered
                    && timer.id > 0
                    && timer.id <= i32::MAX as u32
                    && ids.insert(timer.id)
                    && timer.registered_ms.is_finite()
                    && timer.registered_ms >= 0.0
                    && timer.requested_delay_ms.is_none_or(f64::is_finite)
                    && timer.calls <= self.invoked
                    && (timer.kind != TimerKind::Timeout || timer.calls == 0)
            })
    }
}

#[cfg(test)]
#[path = "android_timer_activity_tests.rs"]
mod activity_tests;
