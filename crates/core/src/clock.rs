use crate::time::{VirtualDuration, VirtualTime};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopBounds {
    pub goal: VirtualTime,
    pub agenda_next: Option<VirtualTime>,
    pub max_segment: Option<VirtualDuration>,
    pub frame_interval: Option<VirtualDuration>,
}

/// The next instant an advancing operation must stop at.
pub fn next_stop(now: VirtualTime, b: &StopBounds) -> VirtualTime {
    if now >= b.goal {
        return now;
    }
    // An agenda event due at or before `now` must be drained before any time passes.
    if let Some(a) = b.agenda_next
        && a <= now
    {
        return now;
    }
    let mut stop = b.goal;
    if let Some(a) = b.agenda_next
        && a > now
    {
        stop = stop.min(a);
    }
    if let Some(seg) = b.max_segment
        && seg.0 > 0
    {
        stop = stop.min(now.saturating_add(seg));
    }
    if let Some(interval) = b.frame_interval
        && interval.0 > 0
    {
        stop = stop.min(frame_tick(frame_slot(now, interval) + 1, interval));
    }
    stop
}

/// The `k`-th frame tick: `k` frame intervals, rounded up to the platform's millisecond frame grid.
/// This cadence is independent of the microsecond resolution of clock grants and remains the same
/// for every slicing of the operations that reach it. An interval under a millisecond counts as one.
pub fn frame_tick(k: u64, interval: VirtualDuration) -> VirtualTime {
    let q = interval.0.max(1000);
    VirtualTime(k.saturating_mul(q).div_ceil(1000) * 1000)
}

/// The number of the last frame tick at or before `now` (0 before the first).
pub fn frame_slot(now: VirtualTime, interval: VirtualDuration) -> u64 {
    let k = now.0 / interval.0.max(1000);
    if k > 0 && frame_tick(k, interval) > now { k - 1 } else { k }
}

/// Tracks whether nothing observable has happened for the idle window.
pub struct Quiescence {
    idle_window: VirtualDuration,
    last_activity: VirtualTime,
    active: BTreeSet<String>,
}

impl Quiescence {
    pub fn new(idle_window: VirtualDuration, now: VirtualTime) -> Quiescence {
        Quiescence { idle_window, last_activity: now, active: BTreeSet::new() }
    }

    pub fn activity(&mut self, t: VirtualTime) {
        self.last_activity = self.last_activity.max(t);
    }

    pub fn begin(&mut self, cause: &str, t: VirtualTime) {
        self.active.insert(cause.to_string());
        self.activity(t);
    }

    pub fn end(&mut self, cause: &str, t: VirtualTime) {
        self.active.remove(cause);
        self.activity(t);
    }

    pub fn is_quiescent(&self, now: VirtualTime) -> bool {
        self.active.is_empty() && now.since(self.last_activity) >= self.idle_window
    }

    /// The start of the current quiet window.
    pub fn last_activity(&self) -> VirtualTime {
        self.last_activity
    }

    pub fn still_active(&self) -> Vec<String> {
        self.active.iter().cloned().collect()
    }
}

/// How `wait_actionable` ended: the instant the target became actionable, or the reason it still was not
/// when the budget ran out.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WaitOutcome {
    Actionable { t: VirtualTime },
    TimedOut { t: VirtualTime, reason: String },
    Cancelled { t: VirtualTime },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SettleOutcome {
    NotRequested,
    /// `next_timer_t`: when the page's earliest pending timer is due, a hint of work quiescence leaves.
    Quiescent {
        t: VirtualTime,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_timer_t: Option<VirtualTime>,
    },
    BudgetExhausted { t: VirtualTime, still_active: Vec<String> },
    Cancelled { t: VirtualTime },
}

#[cfg(test)]
#[path = "clock_tests.rs"]
mod tests;
