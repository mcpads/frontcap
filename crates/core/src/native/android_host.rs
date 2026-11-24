//! Native app operations independent of optional WebView/page control.
use super::android_webview::{AppProcess, ContactCapture, ContactDriver};
use crate::ids::RequestId;
use serde::{Deserialize, Serialize};
use std::{io, path::PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    AttachSpeech { owner: Box<AppProcess>, library: PathBuf, sha256: String },
    ReadSpeech { attachment: RequestId },
    SetAppState { owner: Box<AppProcess>, state: super::app_state::State },
    Restart { previous: Box<AppProcess>, #[serde(default, skip_serializing_if = "Option::is_none")] url: Option<String> },
    /// Package-restricted warm OS entry; never establishes the destination or a new owner.
    DispatchUrl { owner: Box<AppProcess>, url: String },
    DiscoverPages,
    /// Install document-start control and deliberately reload/navigate the selected page URL.
    AttachPage {
        discovery: RequestId, page: String, url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        random_seed: Option<u32>,
    },
    /// Select an already-loaded fresh document after proven release; never navigate or reinstall.
    AttachDocument { release: RequestId, discovery: RequestId, page: String, url: String, document: String },
    /// Relinquish selection without closing its transport or claiming target disposal.
    ReleasePage { attachment: RequestId },
    /// Retire authority only after the selected debugger target is observed absent.
    RetirePage { attachment: RequestId },
    Page { attachment: RequestId, command: PageOperation },
    Observe { #[serde(default)] input_basis: bool },
    Contacts { observation: RequestId, input: super::contacts::Operation },
    ReconcileContacts { request_id: RequestId },
}
impl Operation {
    pub fn validate(&self) -> io::Result<()> {
        match self {
            Self::AttachSpeech { library, sha256, .. } if !library.is_absolute()
                || sha256.len() != 64 || !sha256.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) =>
                Err(io::Error::other("invalid Android speech library path or digest")),
            Self::Restart { url: Some(url), .. } => super::external_entry::validate_url(url).map_err(io::Error::other),
            Self::DispatchUrl { url, .. } => super::external_entry::validate_url(url).map_err(io::Error::other),
            Self::AttachPage { page, url, .. } | Self::AttachDocument { page, url, .. } if page.is_empty() || page.len()>256
                || !page.bytes().all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
                || url.len()>2048 || !(url.starts_with("http://") || url.starts_with("https://"))
                || url.chars().any(char::is_control) => Err(io::Error::other("invalid selected page or HTTP entry URL")),
            Self::AttachDocument { document, .. } if !super::request::canonical_uuid(document) =>
                Err(io::Error::other("invalid selected document identity")),
            Self::Page { command, .. } => command.validate(),
            Self::Contacts { input, .. } => input.validate().map_err(io::Error::other),
            _ => Ok(()),
        }
    }
    pub fn requires_contacts(&self) -> bool {
        matches!(self, Self::Contacts { .. } | Self::ReconcileContacts { .. } | Self::Observe { input_basis: true })
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub owner: AppProcess,
    pub contacts: Option<ContactDriver>,
}

/// Same installed artifact and data container, with fresh process and input authority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub previous: Ready,
    pub ready: Ready,
    pub previous_process: super::android_lifecycle::Process,
    pub process: super::android_lifecycle::Process,
    pub installation: super::android_lifecycle::Installation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<UrlAcceptance>,
}
impl Replacement {
    pub fn validate(&self) -> io::Result<()> {
        let before = &self.previous.owner;
        let after = &self.ready.owner;
        if let Some(entry) = &self.entry {
            super::external_entry::validate_url(&entry.url).map_err(io::Error::other)?;
            if entry.owner != *before { return Err(io::Error::other("Android host cold entry owner differs")); }
        }
        self.previous_process.validate(&before.package).map_err(io::Error::other)?;
        self.process.validate(&after.package).map_err(io::Error::other)?;
        self.installation.validate(&after.package, &after.apk_sha256).map_err(io::Error::other)?;
        if before.emulator != after.emulator || before.package != after.package
            || before.activity != after.activity || before.serial != after.serial
            || before.source_sha256 != after.source_sha256 || before.apk_sha256 != after.apk_sha256
            || self.previous_process.pid != before.pid || self.process.pid != after.pid
            || self.previous_process.start_ticks != before.process_start_ticks
            || self.process.start_ticks != after.process_start_ticks
            || self.previous_process.boot_id != self.process.boot_id
            || (before.pid, before.process_start_ticks) == (after.pid, after.process_start_ticks)
        {
            return Err(io::Error::other("Android host replacement lineage differs"));
        }
        match (&self.previous.contacts, &self.ready.contacts) {
            (None, None) => {}
            (Some(old), Some(new)) if old.consumer.process() == *before
                && matches!(&new.consumer, super::android_webview::ContactConsumer::Host(owner) if owner == after)
                && old.generation != new.generation && old.guest_directory != new.guest_directory
                && (old.pid, old.process_start_ticks) != (new.pid, new.process_start_ticks)
                && old.jar_sha256 == new.jar_sha256 && old.lifetime_ms == new.lifetime_ms
                && old.request_capacity == new.request_capacity => {}
            _ => return Err(io::Error::other("Android replacement contact authority differs")),
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum Completion {
    SpeechAttached { attachment: Box<SpeechAttachment>, history: Box<super::android_speech::History> },
    SpeechRead { attachment: RequestId, history: Box<super::android_speech::History> },
    StateChanged { transition: Box<Transition> },
    Restarted { replacement: Box<Replacement> },
    UrlAccepted { acceptance: Box<UrlAcceptance> },
    Pages { owner: Box<AppProcess>, pages: Vec<Page> },
    Attached { attachment: Box<Attachment> },
    Retired { attachment: Box<Attachment> },
    Released { attachment: Box<Attachment> },
    Page { attachment: RequestId, completion: Box<super::android_webview::Completion> },
    Observed { owner: Box<AppProcess>, screen: PathBuf, bytes: u64, sha256: String, basis: Option<ContactCapture> },
    Contacts { receipt: serde_json::Value },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpeechAttachment {
    pub id: RequestId,
    pub owner: AppProcess,
    pub identity: super::android_speech::Attachment,
    pub directory: String,
    pub library_sha256: String,
}

/// Ordinary host focus/process evidence, independent of any consumer clock or page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transition {
    pub owner: AppProcess,
    pub requested: super::app_state::State,
    pub process_before: super::android_lifecycle::Process,
    pub process_after: super::android_lifecycle::Process,
    pub focused_before: String,
    pub focused_after: String,
}
impl Transition {
    pub fn validate(&self) -> io::Result<()> {
        use super::app_state::State;
        self.process_before.validate(&self.owner.package).map_err(io::Error::other)?;
        self.process_after.validate(&self.owner.package).map_err(io::Error::other)?;
        let component = format!("{}/{}", self.owner.package, self.owner.activity);
        let valid = |value: &str| value.split_once('/').is_some_and(|(package, activity)|
            super::android_capture::valid_app(package, activity));
        let outside = |value: &str| value.split('/').next() != Some(self.owner.package.as_str());
        let focused = match self.requested {
            State::Background => self.focused_before == component && outside(&self.focused_after),
            State::Foreground => outside(&self.focused_before) && self.focused_after == component,
        };
        if self.process_before != self.process_after || self.process_before.pid != self.owner.pid
            || self.process_before.start_ticks != self.owner.process_start_ticks
            || !valid(&self.focused_before) || !valid(&self.focused_after) || !focused {
            return Err(io::Error::other("Android host visibility lacks same-process focus evidence"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UrlAcceptance {
    pub owner: AppProcess,
    pub url: String,
    pub confirmation: super::external_entry::Observation,
    pub destination: super::external_entry::Observation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page { pub id: String, pub url: String, pub title: String }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub id: RequestId,
    pub owner: AppProcess,
    pub page: Page,
    pub prepared: super::android_webview::Prepared,
    pub document: String,
}
/// These operations require a particular live attachment, never an implicit current page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum PageOperation {
    Read,
    InspectProgress,
    InspectFrames,
    InspectDocument,
    InspectPointers,
    Frame { request: super::android_frames::Request },
    Timers { action: super::android_timers::Action },
}
impl PageOperation {
    pub fn validate(&self) -> io::Result<()> {
        match self {
            Self::Frame { request } => request.validate(),
            Self::Timers { action } => action.validate(),
            _ => Ok(()),
        }
    }
}
