//! Android WebView ownership and observation data shared with control protocols.
//! These records do not authorize adoption or replace runtime ownership verification.
use crate::{ids::RequestId, process::ProcessIdentity};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Native app identity; neither a WebView nor a consumer-side test service is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct AppProcess {
    pub emulator: ProcessIdentity,
    pub package: String,
    pub activity: String,
    pub serial: String,
    pub pid: u32,
    pub process_start_ticks: u64,
    pub apk_sha256: String,
    pub source_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Debugger {
    pub emulator: ProcessIdentity,
    pub package: String,
    pub activity: String,
    pub serial: String,
    pub pid: u32,
    pub process_start_ticks: u64,
    pub socket: String,
    pub apk_sha256: String,
    pub source_sha256: String,
}
impl Debugger {
    pub fn process(&self) -> AppProcess {
        AppProcess {
            emulator: self.emulator.clone(), package: self.package.clone(),
            activity: self.activity.clone(), serial: self.serial.clone(), pid: self.pid,
            process_start_ticks: self.process_start_ticks, apk_sha256: self.apk_sha256.clone(),
            source_sha256: self.source_sha256.clone(),
        }
    }
}

/// Preserve original WebView producer records while also admitting a native-only host.
/// Both alternatives reject unknown fields; only WebView records have a debugger socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ContactConsumer {
    WebView(Debugger),
    Host(AppProcess),
}
impl ContactConsumer {
    pub fn process(&self) -> AppProcess {
        match self {
            Self::WebView(debugger) => debugger.process(),
            Self::Host(process) => process.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub random_seed: Option<u32>,
    pub script_identifier: String,
    pub url: String,
    pub attachment_sha256: String,
}
/// Main-realm diagnostics from the declared non-cryptographic random initializer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartingRandomness {
    pub document: String,
    pub seed: u32,
    pub math_calls: u64,
    pub crypto_byte_calls: u64,
    pub crypto_uuid_calls: u64,
    pub observation_errors: u64,
    pub uncontrolled_calls: u64,
}
impl StartingRandomness {
    pub(crate) fn valid(&self) -> bool {
        let calls = self.math_calls.checked_add(self.crypto_byte_calls)
            .and_then(|n| n.checked_add(self.crypto_uuid_calls));
        calls.is_some_and(|n| n <= 9_007_199_254_740_991 && self.observation_errors <= n)
            && self.uncontrolled_calls <= 9_007_199_254_740_991
    }
}

/// Publication by a source-qualified Flutter loader; not whole-app readiness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CanvasKitPublication {
    pub document: String,
    pub published: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starting_randomness: Option<Box<StartingRandomness>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canvas_kit: Option<Box<CanvasKitPublication>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<Box<super::webview_media::History>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_console: Option<Box<super::android_console::History>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_events: Option<Box<super::webview_observation::DocumentEvents>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timer_activity: Option<Box<super::android_timers::Activity>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_activity: Option<Box<super::android_frames::Activity>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigation: Option<super::webview_navigation::Navigation>,
    pub href: String,
    pub ready_state: String,
    pub visibility: String,
    pub focused: bool,
    pub performance_ms: f64,
    pub date_ms: f64,
}

impl Snapshot {
    pub fn validate(&self, document: Option<&str>) -> std::io::Result<()> {
        if self.starting_randomness.as_ref().is_some_and(|r| !r.valid()) {
            return Err(std::io::Error::other("invalid starting randomness diagnostics"));
        }
        let documents = [document,
            self.canvas_kit.as_ref().map(|c| c.document.as_str()),
            self.starting_randomness.as_ref().map(|r| r.document.as_str()),
            self.media.as_ref().and_then(|m| m.document.as_deref()),
            self.frame_activity.as_ref().map(|f| f.document.as_str()),
            self.page_events.as_ref().map(|e| e.document.as_str()),
            self.navigation.as_ref().map(|n| n.document.as_str()),
            self.timer_activity.as_ref().map(|t| t.document.as_str())];
        for observed in [self.canvas_kit.as_ref().map(|c| c.document.as_str()),
            self.starting_randomness.as_ref().map(|r| r.document.as_str())].into_iter().flatten() {
            if !super::request::canonical_uuid(observed)
                || documents.into_iter().flatten().any(|d| d != observed) {
                return Err(std::io::Error::other("invalid or foreign-document initialization observation"));
            }
        }
        if self.media.as_ref().is_some_and(|m| !m.valid() || m.document.is_none()
                || document.is_some_and(|d| m.document.as_deref() != Some(d))
                || self.frame_activity.as_ref().is_some_and(|f| m.document.as_ref() != Some(&f.document))
                || self.page_events.as_ref().is_some_and(|e| m.document.as_ref() != Some(&e.document))
                || self.navigation.as_ref().is_some_and(|n| m.document.as_ref() != Some(&n.document))
                || self.timer_activity.as_ref().is_some_and(|t| m.document.as_ref() != Some(&t.document)))
            || self.frame_activity.as_ref().is_some_and(|f| !f.valid()
                || document.is_some_and(|expected| f.document != expected)
                || self.page_events.as_ref().is_some_and(|e| e.document != f.document)
                || self.navigation.as_ref().is_some_and(|n| n.document != f.document)
                || self.timer_activity.as_ref().is_some_and(|t| t.document != f.document))
            || self.connection_console.as_ref().is_some_and(|c| !c.valid())
            || !self.performance_ms.is_finite() || !self.date_ms.is_finite()
            || self.page_events.as_ref().is_some_and(|e| !e.valid()
                || document.is_some_and(|expected| e.document != expected)
                || self.navigation.as_ref().is_some_and(|n| n.document != e.document)
                || self.timer_activity.as_ref().is_some_and(|t| t.document != e.document))
            || self.navigation.as_ref().is_some_and(|n| !n.valid()
                || document.is_some_and(|expected| n.document != expected))
            || self.timer_activity.as_ref().is_some_and(|t| !t.valid()
                || document.is_some_and(|expected| t.document != expected))
            || self.navigation.as_ref().zip(self.timer_activity.as_ref())
                .is_some_and(|(n,t)| n.document != t.document) {
            return Err(std::io::Error::other("invalid or foreign-document Android page observation"));
        }
        Ok(())
    }
    /// Parsing must finish before attachment; pending subresources remain explicitly observable.
    /// The caller must independently verify and bind the document-local frame producer.
    pub fn entry_document_parsed(&self, url: &str) -> bool {
        matches!(self.ready_state.as_str(), "interactive" | "complete") && self.href == url
    }
    /// A settled initial host navigation permits the caller's explicit entry navigation.
    /// An error document is not content readiness; about:blank may precede a queued host load.
    pub fn initial_navigation_finished(&self) -> bool {
        self.ready_state == "complete"
            && (self.href.starts_with("http://")
                || self.href.starts_with("https://")
                || self.href == "chrome-error://chromewebdata/")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContactDriver {
    /// Producer supports one bounded native DOWN/UP request. Older producers do not.
    #[serde(default)]
    pub short_tap: bool,
    pub generation: String,
    pub lifetime_ms: u64,
    pub request_capacity: u64,
    pub pid: u32,
    pub process_start_ticks: u64,
    pub guest_directory: String,
    pub jar_sha256: String,
    pub consumer: ContactConsumer,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContactCapture {
    pub id: RequestId,
    pub path: PathBuf,
    pub sha256: String,
    pub width: u32,
    pub height: u32,
    pub focus: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WebViewLifecycle {
    Home,
    Activate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebViewLifecycleReceipt {
    pub operation: WebViewLifecycle,
    pub owner: Debugger,
    pub focused_component: String,
}

/// Commands for one prepared document under a still-live Android owner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Host { request: super::android_host::Operation },
    Read,
    Observe {
        #[serde(default)]
        input_basis: bool,
    },
    InspectProgress,
    InspectFrames,
    InspectPointers,
    Frame {
        request: super::android_frames::Request,
    },
    Timers {
        action: super::android_timers::Action,
    },
    Contacts {
        observation: RequestId,
        input: super::contacts::Operation,
    },
    ReconcileContacts {
        request_id: RequestId,
    },
    HomeReturn,
}
impl Operation {
    pub fn validate(&self) -> std::io::Result<()> {
        match self {
            Self::Host { request } => request.validate(),
            Self::Frame { request } => request.validate(),
            Self::Timers { action } => action.validate(),
            Self::Contacts { input, .. } => input.validate().map_err(std::io::Error::other),
            _ => Ok(()),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub owner: Debugger,
    pub page_id: String,
    pub document: String,
    pub prepared: Prepared,
    pub contacts: Option<ContactDriver>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum Completion {
    Host { completion: super::android_host::Completion },
    Progress { observation: Box<super::android_progress::Progress> },
    Read {
        snapshot: Snapshot,
    },
    Observed {
        snapshot: Snapshot,
        screen: PathBuf,
        bytes: u64,
        sha256: String,
        pointers: super::android_pointers::Snapshot,
        basis: Option<ContactCapture>,
    },
    Frames {
        receipt: Option<super::android_frames::Receipt>,
    },
    Pointers {
        snapshot: super::android_pointers::Snapshot,
    },
    Frame {
        receipt: super::android_frames::Receipt,
    },
    Timers {
        completion: super::android_timers::Completion,
    },
    Contacts {
        receipt: serde_json::Value,
    },
    HomeReturned {
        home: Box<WebViewLifecycleReceipt>,
        returned: Box<WebViewLifecycleReceipt>,
        before: Snapshot,
        after: Snapshot,
        receipt: super::android_frames::Receipt,
    },
}

#[cfg(test)]
#[path = "android_webview_tests.rs"]
mod page_event_tests;
