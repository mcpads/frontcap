//! Bounded page observations; absence and dropped events never establish whole-app idle.
use serde::{Deserialize, Serialize};

/// Bounded readback from the controlled iOS page scheduler, not all app producers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimerQueue {
    pub timers: Vec<ScheduledTimer>,
    pub omitted: u64,
    pub failure: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduledTimer {
    pub id: u64,
    pub kind: TimerKind,
    /// Scheduled elapsed page time; may already be due. Not host time or an idle guarantee.
    pub due_ms: f64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimerKind { Timeout, Interval }
impl TimerQueue {
    pub(crate) fn valid(&self, pending: u64) -> bool {
        let mut ids = std::collections::HashSet::new();
        self.timers.len() <= 32
            && self.omitted.checked_add(self.timers.len() as u64) == Some(pending)
            && self.failure.as_ref().is_none_or(|s| s.len() <= 256 && self.timers.is_empty())
            && self.timers.iter().all(|timer| timer.id > 0
                && timer.id <= 9_007_199_254_740_991 && ids.insert(timer.id) && timer.due_ms.is_finite())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Frames {
    pub pending: u64,
    pub dispatching: bool,
    pub last_timestamp: Option<f64>,
    pub delivered: u64,
}
impl Frames {
    pub(crate) fn valid(&self) -> bool {
        self.last_timestamp.is_none_or(time)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Visibility {
    Visible,
    Hidden,
    Prerender,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Observation {
    /// Source-qualified library publication; not application readiness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas_kit: Option<Box<super::android_webview::CanvasKitPublication>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starting_randomness: Option<Box<super::android_webview::StartingRandomness>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_speech: Option<super::speech_observation::History>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<super::webview_media::History>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console: Option<super::webview_console::History>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigation: Option<super::webview_navigation::Navigation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<Visibility>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<PageEvents>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointers: Option<Pointers>,
}
impl Observation {
    pub(crate) fn valid(&self) -> bool {
        self.starting_randomness.as_ref().is_none_or(|r| r.valid())
            && self.native_speech.as_ref().is_none_or(super::speech_observation::History::valid)
            && self.media.as_ref().is_none_or(super::webview_media::History::valid)
            && self.console.as_ref().is_none_or(super::webview_console::History::valid)
            && self.navigation.as_ref().is_none_or(super::webview_navigation::Navigation::valid)
            && self.events.as_ref().is_none_or(PageEvents::valid)
            && self.pointers.as_ref().is_none_or(Pointers::valid)
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PageEvents {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    pub received: u64,
    pub dropped: u64,
    pub events: Vec<PageEvent>,
}
/// Page-event evidence bound to a selected Android document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentEvents {
    pub document: String,
    #[serde(flatten)]
    pub history: PageEvents,
}
impl DocumentEvents {
    pub(crate) fn valid(&self) -> bool {
        super::request::canonical_uuid(&self.document) && self.history.valid()
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageEvent {
    pub sequence: u64,
    pub elapsed_ms: f64,
    #[serde(flatten)]
    pub event: PageEventKind,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PageEventKind {
    Visibility {
        state: Visibility,
    },
    Message {
        origin: String,
        #[serde(rename = "originTruncated")]
        origin_truncated: bool,
        #[serde(rename = "dataType")]
        data_type: String,
        data: Option<String>,
        truncated: bool,
    },
}
fn time(value: f64) -> bool {
    value.is_finite() && value >= 0.0
}
fn tail(received: u64, dropped: u64, sequences: impl Iterator<Item = u64>) -> bool {
    let sequences: Vec<_> = sequences.collect();
    sequences.len() <= 8
        && dropped.checked_add(sequences.len() as u64) == Some(received)
        && sequences
            .iter()
            .enumerate()
            .all(|(index, sequence)| dropped.checked_add(index as u64 + 1) == Some(*sequence))
}
impl PageEvents {
    pub(crate) fn valid(&self) -> bool {
        self.failure.as_ref().is_none_or(|s| s.encode_utf16().count() <= 256) && tail(
            self.received,
            self.dropped,
            self.events.iter().map(|e| e.sequence),
        ) && self.events.iter().all(|e| {
            time(e.elapsed_ms)
                && match &e.event {
                    PageEventKind::Visibility { .. } => true,
                    PageEventKind::Message {
                        origin,
                        data_type,
                        data,
                        truncated,
                        ..
                    } => {
                        origin.encode_utf16().count() <= 32
                            && matches!(
                                data_type.as_str(),
                                "string"
                                    | "object"
                                    | "number"
                                    | "boolean"
                                    | "undefined"
                                    | "bigint"
                                    | "symbol"
                                    | "function"
                            )
                            && (data_type == "string") == data.is_some()
                            && data.as_ref().is_none_or(|s| s.encode_utf16().count() <= 64)
                            && (!truncated || data.is_some())
                    }
                }
        })
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pointers {
    pub received: u64,
    pub dropped: u64,
    pub active: u64,
    pub maximum_active: u64,
    pub events: Vec<PointerEvent>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PointerKind {
    PointerDown,
    PointerMove,
    PointerUp,
    PointerCancel,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PointerEvent {
    pub sequence: u64,
    pub kind: PointerKind,
    pub id: i64,
    pub trusted: bool,
    pub x: f64,
    pub y: f64,
    pub page_ms: f64,
    pub active: u64,
}
impl Pointers {
    fn valid(&self) -> bool {
        self.active <= self.maximum_active
            && tail(
                self.received,
                self.dropped,
                self.events.iter().map(|e| e.sequence),
            )
            && self.events.iter().all(|e| {
                e.x.is_finite()
                    && e.y.is_finite()
                    && time(e.page_ms)
                    && e.active <= self.maximum_active
            })
            && self.events.last().is_none_or(|e| e.active == self.active)
    }
}

#[cfg(test)]
#[path = "webview_observation_tests.rs"]
mod tests;
