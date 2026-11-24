//! The interception binding journal (`interceptions.jsonl` in the bundle): control metadata that ties a
//! native paused request to the logical request the trace names, written before the platform
//! releases the request's hold on virtual time or answers it. A relaunched recorder reads it to adopt the
//! requests the engine retained instead of deciding them again.
use crate::ChromiumError;
use frontcap_core::event::ResponseSource;
use frontcap_core::ids::RuleId;
use frontcap_core::lines::{Appender, open_append, read_complete};
use frontcap_core::rules::{FailReason, MockSpec};
use std::fs::File;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BindingEntry {
    /// Frozen before Fetch collection/selection; survives loss of the live owner. `document` is
    /// where the lineage's creator document stood then: the worker's place among same-instant issuers.
    WorkerRequestOwner {
        network_id: String, lineage: WorkerLineage, issued_us: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        document: Option<DocumentPlace>,
    },
    Bound(Box<Binding>),
    /// A response captured before releasing a synchronous recording to the renderer.
    RecordedResponse(Box<RecordedResponse>),
    StreamClosed(Box<StreamClosure>),
    /// The owning frame disappeared before its stream could be admitted.
    StreamAbandoned { target_id: String, handshake: Box<StreamHandshake> },
    /// Selected before binding the wrapper or connecting to an upstream server.
    StreamSelected(Box<StreamSelection>),
    /// An external connection may have started; recovery must not connect it again.
    StreamConnecting(Box<StreamConnection>),
    /// A replay session (or exhaustion) selected before any page delivery.
    StreamReplay(Box<StreamReplay>),
    /// An opening about to be delivered. The living wrapper's sequence proves whether it ran.
    StreamOpening {
        target_id: String, ordinal: String, owner: String,
        document: u64, page_stream: u64, sequence: u64, at_us: u64,
        requested_us: u64, request: Box<frontcap_core::event::RequestRecord>,
        answered_us: u64, response: frontcap_core::event::ResponseRecord,
    },
    /// A stream a `hold` rule keeps for the agent, before its request's record is written.
    StreamHeld {
        target_id: String, ordinal: String, owner: String, document: u64, page_stream: u64, websocket: bool,
        requested_us: u64, request: Box<frontcap_core::event::RequestRecord>,
    },
    /// An event the agent sends on a held stream, about to be delivered: a relaunched recorder delivers
    /// it when the page's wrapper did not receive it. `request` is the agent's request that sent it.
    StreamSent {
        target_id: String, ordinal: String, seq: u64, event: frontcap_core::event::StreamEventKind,
        data: Option<String>, sse_event: Option<String>, sse_id: Option<String>, code: Option<u16>, request: String,
    },
    /// Explicit progress resumes the rule chosen before an interception stop.
    InterceptionReleased { target_id: String, fetch_id: String },
    /// A resolution about to be sent for a bound request, with the exact payload.
    Dispatched { ordinal: String, fetch_id: String, resolution: Resolution },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StreamReplay {
    pub target_id: String,
    pub fetch_id: String,
    pub rules_revision: u64,
    pub rule: RuleId,
    pub key: frontcap_core::cassette::Key,
    pub session: Option<u64>,
    pub base_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StreamConnection {
    pub target_id: String,
    pub fetch_id: String,
    pub record: Option<(String, frontcap_core::cassette::Key, u64)>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StreamSelection {
    pub target_id: String,
    pub fetch_id: String,
    pub ordinal: String,
    pub rules_revision: u64,
    pub rule: RuleId,
    pub owner: String,
    pub page_stream: u64,
    pub issued_us: u64,
}

/// The selected outcome of a stream anchor, retained independently of trace publication. An anchor
/// decided from the page's announcement before any interception has no native pause (`fetch_id`); one the
/// browser still reports is known by its Network-domain id.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StreamHandshake {
    pub fetch_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_id: Option<String>,
    pub requested_us: u64,
    pub request: frontcap_core::event::RequestRecord,
    pub answered_us: u64,
    pub response: frontcap_core::event::ResponseRecord,
}

impl StreamHandshake {
    /// Whether `paused` is this stream's anchor: its native pause, or the page's request.
    pub fn anchors(&self, paused: &cdp::protocol::fetch::RequestPausedEvent) -> bool {
        self.fetch_id.as_deref() == Some(paused.request_id.0.as_str())
            || self.network_id.as_deref().is_some_and(|id| paused.network_id.as_ref().is_some_and(|n| n.0 == id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct StreamClosure {
    pub target_id: String,
    pub handshake: StreamHandshake,
    pub closed_us: u64,
    pub receipt: Option<frontcap_core::event::StreamReceipt>,
}

/// A dedicated worker's constructor path within one native creator document.
/// The document token is an equality/correlation key, never a sorting key across
/// documents or replays: the document's `DocumentPlace` orders it among them.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkerLineage {
    pub creator_document: String,
    pub constructors: Vec<u64>,
}

/// A document's logical place, which orders its workers among same-instant issuers: its window (0
/// the launch page, then windows in the order they opened) and its frame's child indices from that
/// window's top frame. `Retired` once it was no frame's current document.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DocumentPlace {
    Current { window: u64, frames: Vec<u32> },
    Retired,
}

/// Full request key frozen before a recovering recorder releases external I/O.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordingPlan {
    pub cassette: String,
    pub key: frontcap_core::cassette::Key,
}

/// An idempotent cassette append: the original prefix and one complete response line.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordedResponse {
    pub ordinal: String,
    pub cassette: String,
    pub offset: usize,
    pub prefix_sha256: String,
    pub line: frontcap_core::cassette::Line,
}

impl RecordedResponse {
    pub fn publish(&self, bundle: &Path) -> Result<(), ChromiumError> {
        let path = bundle.join("cassettes").join(format!("{}.jsonl", self.cassette));
        let mut bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e.into()),
        };
        let line = self.line.to_line() + "\n";
        if bytes.len() < self.offset || frontcap_core::canonical::sha256_bytes_hex(&bytes[..self.offset]) != self.prefix_sha256 {
            return Err(ChromiumError::Shape(format!("{}: recorded cassette prefix changed", self.ordinal)));
        }
        if bytes.len() > self.offset {
            if !bytes[self.offset..].starts_with(line.as_bytes()) {
                return Err(ChromiumError::Shape(format!("{}: recorded cassette append conflicts", self.ordinal)));
            }
            return Ok(());
        }
        bytes.extend_from_slice(line.as_bytes());
        std::fs::create_dir_all(path.parent().expect("cassette directory"))?;
        frontcap_core::state::save_atomic_bytes(&path, &bytes)?;
        Ok(())
    }
}

/// A native pause (`target_id`, `fetch_id`: one pause, since only the request stage is intercepted and a
/// redirect hop gets a new id) bound to its logical request and the decision taken for it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Binding {
    /// Native evidence that the issuing document task waits for this request.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub blocks_main_thread: bool,
    /// Native frame identity for pre-renderer admission within one interrupted task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recording: Option<RecordingPlan>,
    /// The preceding synchronous request when admitted during pre-renderer recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<String>,
    /// Selection evidence not yet published by the recovering recorder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_taint: Option<frontcap_core::event::TaintRecord>,
    /// Installation selected before binding; recovery must not rematch the active set.
    #[serde(default)]
    pub rules_revision: u64,
    pub ordinal: String,
    pub target_id: String,
    pub fetch_id: String,
    pub network_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_lineage: Option<WorkerLineage>,
    pub document: u64,
    pub issued_us: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub issued_during_progress: bool,
    pub rule: RuleId,
    /// The replayed cassette line or stream session, as the trace's request record names it.
    pub entry: Option<u64>,
    /// Bounded request evidence retained before its trace publication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<frontcap_core::event::RequestBody>,
    pub method: String,
    pub url: String,
    /// The `Origin` the request carried: a mocked answer gets CORS headers for it.
    pub origin: Option<String>,
    pub decision: Decision,
    /// The top frame's document request: while it loads, held input waits for the new document.
    #[serde(default)]
    pub document_request: bool,
    /// The agent's navigation request this top-frame document request belongs to, when the platform
    /// issued that navigation. Durable before the request is answered, so a relaunched recorder knows the
    /// browser received the navigation even after its request has been answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Decision {
    /// Frozen before applying the selected rule; explicit progress resumes it.
    Intercept { then: Box<Decision> },
    /// Answered with `spec` at the absolute instant `due_us`.
    Deliver {
        spec: MockSpec,
        source: ResponseSource,
        due_us: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        requires_progress: bool,
    },
    /// Held for the agent.
    Hold,
    Passthrough,
    Fail { reason: FailReason },
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Resolution {
    Fulfill { status: u16, body_sha256: String },
    Fail { reason: FailReason },
    Continue,
}

/// SHA-256 (hex) of the body bytes `fulfill` sends for `spec`, which the engine's receipt digests too.
pub fn body_sha256(spec: &MockSpec) -> String {
    match spec.body_base64.as_deref().and_then(frontcap_core::encode::unbase64) {
        Some(bytes) => frontcap_core::canonical::sha256_bytes_hex(&bytes),
        None => frontcap_core::canonical::sha256_bytes_hex(spec.body.as_bytes()),
    }
}

pub struct BindingJournal {
    path: PathBuf,
    out: Appender<File>,
    recovered: Option<PathBuf>,
}

impl BindingJournal {
    /// Opens for appending; a line cut by a killed writer is moved aside (`recovered_tail`).
    pub fn open(path: &Path) -> Result<BindingJournal, ChromiumError> {
        let opened = open_append(path).map_err(shape)?;
        Ok(BindingJournal { path: path.to_path_buf(), out: Appender::new(opened.file, opened.len), recovered: opened.recovered })
    }

    pub fn recovered_tail(&self) -> Option<&Path> {
        self.recovered.as_deref()
    }

    /// Appends `e`: once this returns, a relaunched recorder sees the entry. The fault model is the
    /// recorder's process ending (the trace is not synced either), so a write suffices; a sync per
    /// intercepted request would only add latency.
    pub fn append(&mut self, e: &BindingEntry) -> Result<(), ChromiumError> {
        let line = serde_json::to_string(e).map_err(|e| ChromiumError::Shape(e.to_string()))?;
        self.out.append_line(&line)?;
        Ok(())
    }

    /// The complete entries of a journal (none when it does not exist); a partial last line is ignored.
    pub fn read(path: &Path) -> Result<Vec<BindingEntry>, ChromiumError> {
        let lines = match read_complete(path) {
            Ok(lines) => lines,
            Err(frontcap_core::lines::LinesError::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(shape(e)),
        };
        lines
            .iter()
            .enumerate()
            .map(|(i, l)| serde_json::from_str(l).map_err(|e| ChromiumError::Shape(format!("line {} of {}: {e}", i + 1, path.display()))))
            .collect()
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn shape(e: frontcap_core::lines::LinesError) -> ChromiumError {
    ChromiumError::Shape(e.to_string())
}

#[cfg(test)]
#[path = "bindings_tests.rs"]
mod tests;
