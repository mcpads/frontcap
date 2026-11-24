//! Bounded retained evidence, not a fresh or atomic observation of all app producers.
use super::{contacts, request::ClockRequest, webview, webview_lifecycle};
use crate::ids::RequestId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completed<R, T> {
    pub id: RequestId,
    pub native_request_id: u64,
    pub request: R,
    pub reply: T,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pending<R> {
    pub id: RequestId,
    pub native_request_id: u64,
    pub request: R,
    pub dispatch: Dispatch,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dispatch {
    Prepared,
    Unresolved,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Journal<R, T> {
    /// Publication failure may leave disk ahead of this retained in-memory evidence.
    pub storage_uncertain: bool,
    pub last_completed: Option<Completed<R, T>>,
    pub pending: Option<Pending<R>>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub owner: webview::Owner,
    pub retired: bool,
    pub basis: Basis,
    pub service: Service,
    pub clock: Journal<ClockRequest, webview::Reply>,
    pub contact_endpoint: Option<contacts::Endpoint>,
    pub contacts: Option<Journal<contacts::Operation, contacts::Receipt>>,
    pub requires_cancel: Option<bool>,
    pub lifecycle: Option<Journal<webview_lifecycle::Request, webview_lifecycle::Reply>>,
    /// These producers are outside this inspection, even when every known count is zero.
    pub unobserved: Vec<Unobserved>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    RetainedReceipts,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unobserved {
    ServiceDelivery,
    Media,
    Speech,
    NativePresentation,
    OtherAppWork,
}

/// Sampled separately by the recorder; HTTP transport never proves page consumption.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Service {
    NotInspected,
    Unconfigured,
    Observed {
        generation: crate::ids::LaunchId,
        stopping: bool,
        requests: usize,
        held: usize,
        open: usize,
        closed: usize,
        failed: usize,
        timed_out: usize,
        stopped: usize,
        releases: usize,
        prepared: usize,
        queued_for_http: usize,
        receiver_gone: usize,
    },
    Uncertain {
        reason: String,
    },
}
impl Service {
    pub fn from_record(record: &super::service::Record) -> Self {
        use super::service::{ReleaseState, Transport};
        let transport = |state| {
            record
                .requests
                .values()
                .filter(|r| r.transport == state)
                .count()
        };
        let release = |state| {
            record
                .releases
                .values()
                .filter(|r| r.state == state)
                .count()
        };
        Self::Observed {
            generation: record.generation.clone(),
            stopping: record.stopping,
            requests: record.requests.len(),
            held: record
                .requests
                .values()
                .filter(|r| r.release.is_none() && r.transport == Transport::Open)
                .count(),
            open: transport(Transport::Open),
            closed: transport(Transport::Closed),
            failed: transport(Transport::Failed),
            timed_out: transport(Transport::TimedOut),
            stopped: transport(Transport::Stopped),
            releases: record.releases.len(),
            prepared: release(ReleaseState::Prepared),
            queued_for_http: release(ReleaseState::QueuedForHttp),
            receiver_gone: release(ReleaseState::ReceiverGone),
        }
    }
}

/// Caller-selected historical image, never a current input basis or atomic page observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capture {
    pub request_id: RequestId,
    pub path: std::path::PathBuf,
    pub bytes: u64,
    pub sha256: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub progress: RequestId,
    pub capture: Option<Capture>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueSample {
    pub present: bool,
    pub serialized_bytes: Option<usize>,
    pub value: Option<serde_json::Value>,
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    /// JSON Pointer into the saved progress reply; arrays are compared as bounded whole values.
    pub path: String,
    pub before: ValueSample,
    pub after: ValueSample,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Comparison<O = webview::Owner> {
    pub owner: O,
    pub before: Evidence,
    pub after: Evidence,
    pub changes: Vec<Change>,
    pub omitted_changes: usize,
    pub capture_content_changed: Option<bool>,
    pub limitations: Vec<String>,
}
