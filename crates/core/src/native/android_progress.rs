//! Bounded retained Android producer evidence; no live sampling or input authority.
use super::{android_frames, android_timers, android_webview, contacts, webview_progress};
use crate::ids::RequestId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frames {
    pub document: String,
    pub completed: Option<android_frames::Request>,
    pub pending: Option<android_frames::Request>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Input {
    pub id: RequestId,
    pub observation: RequestId,
    pub operation: contacts::Operation,
    pub native_request_id: u64,
    pub reply: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contacts {
    pub owner: android_webview::ContactDriver,
    pub storage_uncertain: bool,
    pub revision: u64,
    /// Unallocated native slots; a pending request reserves one. This is not admission authority.
    pub remaining_requests: u64,
    pub last_completed: Option<Input>,
    pub pending: Option<Input>,
}

/// Latest admitted transition and latest verified completion, not a navigation event history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub id: RequestId,
    pub operation: android_webview::WebViewLifecycle,
    pub owner: android_webview::Debugger,
    pub receipt: Option<android_webview::WebViewLifecycleReceipt>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Lifecycle {
    pub uncertain: bool,
    pub last_completed: Option<Transition>,
    pub pending: Option<Transition>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    pub owner: android_webview::Debugger,
    pub page_id: String,
    pub basis: webview_progress::Basis,
    pub connection_closed: bool,
    pub frames_need_reconciliation: bool,
    pub timers_need_pause: bool,
    /// Passive connection events, distinct from the controlled document and frame frontier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_navigation: Option<super::android_navigation::History>,
    pub frames: Option<Frames>,
    pub timers: Option<android_timers::State>,
    pub contacts: Option<Contacts>,
    #[serde(default)]
    pub lifecycle: Option<Lifecycle>,
    pub service: webview_progress::Service,
    pub limitations: Vec<String>,
}
