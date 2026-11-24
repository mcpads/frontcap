//! External WebView identities and page-clock evidence, distinct from ordinary UIKit receipts.
use super::request::{ClockOperation, ClockRequest, InvalidRequest, canonical_uuid};
use crate::{ids::RequestId, process::ProcessIdentity};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
/// Projected descriptor fields; the source file can also contain its last receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub generation: String,
    pub pid: u32,
    pub port: u16,
}
impl Endpoint {
    pub fn validate(&self, request: &ClockRequest) -> Result<(), InvalidRequest> {
        if !canonical_uuid(&self.generation)
            || self.pid == 0
            || self.pid > i32::MAX as u32
            || self.port == 0
            || request.generation != self.generation
            || request.request_id == 0
            || request.request_id > i64::MAX as u64
            || request.delta_ns > i64::MAX as u64
            || (request.operation == ClockOperation::Read && request.delta_ns != 0)
        {
            return Err(InvalidRequest("invalid WebView endpoint or request"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    NotReady,
    Rejected,
    Unknown,
}

/// Controlled page-clock state and bounded timer/frame evidence; other producers remain separate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer_queue: Option<Box<super::webview_observation::TimerQueue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frames: Option<super::webview_observation::Frames>,
    pub elapsed_ms: f64,
    pub date_ms: f64,
    pub performance_ms: f64,
    pub advancing: bool,
    pub failure: Option<String>,
    pub pending_timers: u64,
    pub cancelled_on_failure: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub before: State,
    pub requested_ms: f64,
    pub after: State,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    #[serde(flatten)]
    pub observation: super::webview_observation::Observation,
    pub generation: String,
    pub pid: u32,
    pub port: u16,
    pub request: ClockRequest,
    pub last_id: u64,
    pub now_ns: u64,
    pub replayed: bool,
    pub status: Status,
    pub applied: Option<bool>,
    pub document: Option<String>,
    pub state: Option<State>,
    pub progress: Option<Progress>,
    pub error: Option<String>,
}
fn nanos(ms: f64) -> Option<u64> {
    // JavaScript's exact integer range bounds meaningful page-clock evidence.
    let ns = ms * 1_000_000.0;
    (ns.is_finite() && (0.0..=9_007_199_254_740_991.0).contains(&ns) && ns.fract() == 0.0)
        .then_some(ns as u64)
}
impl State {
    fn validate(&self) -> bool {
        self.frames
            .as_ref()
            .is_none_or(super::webview_observation::Frames::valid)
            && self.timer_queue.as_ref().is_none_or(|queue| queue.valid(self.pending_timers))
            && nanos(self.elapsed_ms).is_some()
            && self.performance_ms == self.elapsed_ms
            && self.date_ms.is_finite()
            && !self.advancing
            && self.failure.is_none()
    }
}
impl Reply {
    pub fn validate(
        &self,
        endpoint: &Endpoint,
        request: &ClockRequest,
        document: Option<&str>,
    ) -> Result<(), InvalidRequest> {
        endpoint.validate(request)?;
        if !self.observation.valid()
            || self.observation.canvas_kit.as_ref().is_some_and(|c|
                !canonical_uuid(&c.document) || self.document.as_deref() != Some(c.document.as_str()))
            || self.observation.starting_randomness.as_ref().is_some_and(|r|
                !canonical_uuid(&r.document) || self.document.as_deref() != Some(r.document.as_str()))
            || self.observation.native_speech.as_ref().is_some_and(|s| s.pid != self.pid)
            || self.observation.media.as_ref().and_then(|m| m.document.as_ref()).is_some_and(|d| self.document.as_ref() != Some(d))
            || self.observation.console.as_ref().is_some_and(|c| self.document.as_deref() != Some(c.document.as_str()))
            || self.observation.navigation.as_ref().is_some_and(|n|
            self.document.as_deref() != Some(n.document.as_str())) {
            return Err(InvalidRequest("invalid WebView page observation"));
        }
        if self.generation != endpoint.generation
            || self.pid != endpoint.pid
            || self.port != endpoint.port
            || &self.request != request
            || self.last_id > i64::MAX as u64
            || self.now_ns > i64::MAX as u64
        {
            return Err(InvalidRequest("WebView reply identity differs"));
        }
        if self.status != Status::Ok {
            // A rejection may precede admission, or consume a request in the WebView handler.
            // None of these outcomes authorizes a fresh mutation or establishes readiness.
            if self.status != Status::Rejected && self.last_id != request.request_id {
                return Err(InvalidRequest("WebView outcome cursor differs"));
            }
            if self.applied == Some(true)
                || (self.status == Status::Unknown && self.applied.is_some())
            {
                return Err(InvalidRequest("inconsistent WebView incomplete outcome"));
            }
            return Ok(());
        }
        let state = self
            .state
            .as_ref()
            .ok_or(InvalidRequest("missing WebView state"))?;
        let page = self
            .document
            .as_deref()
            .ok_or(InvalidRequest("missing WebView document"))?;
        if !canonical_uuid(page)
            || document.is_some_and(|expected| expected != page)
            || self.last_id != request.request_id
            || !state.validate()
            || nanos(state.elapsed_ms) != Some(self.now_ns)
            || self.error.is_some()
        {
            return Err(InvalidRequest("invalid WebView page outcome"));
        }
        let valid = match request.operation {
            ClockOperation::Read => self.applied == Some(false) && self.progress.is_none(),
            ClockOperation::Advance => {
                self.applied == Some(true)
                    && self.progress.as_ref().is_some_and(|p| {
                        p.before.validate()
                            && p.after == *state
                            && nanos(p.before.elapsed_ms)
                                .and_then(|n| n.checked_add(request.delta_ns))
                                == Some(self.now_ns)
                            && nanos(p.requested_ms) == Some(self.now_ns)
                    })
            }
        };
        if !valid {
            return Err(InvalidRequest("inconsistent WebView time effect"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub simulator: String,
    pub process: ProcessIdentity,
    pub endpoint: Endpoint,
    pub attachment_sha256: String,
}
impl Owner {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.endpoint.validate(&ClockRequest {
            generation: self.endpoint.generation.clone(),
            request_id: 1,
            operation: ClockOperation::Read,
            delta_ns: 0,
        })?;
        if !canonical_uuid(&self.simulator.to_ascii_lowercase())
            || self.process.pid != self.endpoint.pid
            || self.process.started.is_empty()
            || self.attachment_sha256.len() != 64
            || !self
                .attachment_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(InvalidRequest("invalid WebView owner"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Host { request: super::ios_host::Operation },
    Restart { generation: String },
    InspectProgress,
    Background,
    Foreground,
    ReconcileLifecycle { request_id: RequestId },
    Contacts {
        observation: RequestId,
        input: super::contacts::Operation,
    },
    ReconcileContacts {
        request_id: RequestId,
    },
    Clock {
        kind: ClockOperation,
        delta_ns: u64,
    },
    Reconcile {
        request_id: RequestId,
    },
    Capture,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Completion {
    Host { completion: super::ios_host::Completion },
    Restarted { previous: Owner, owner: Owner, page_ready: bool },
    Progress { observation: Box<super::webview_progress::Progress> },
    Lifecycle { reply: super::webview_lifecycle::Reply },
    LifecycleReconciled { reply: super::webview_lifecycle::Reply },
    Contacts {
        receipt: super::contacts::Receipt,
    },
    ContactsReconciled {
        receipt: super::contacts::Receipt,
    },
    Clock {
        reply: Reply,
    },
    Reconciled {
        reply: Reply,
    },
    Captured {
        #[serde(default)]
        input_basis: Option<super::contacts::CaptureInput>,
        path: PathBuf,
        bytes: u64,
        sha256: String,
    },
}

#[cfg(test)]
#[path = "webview_tests.rs"]
mod tests;
