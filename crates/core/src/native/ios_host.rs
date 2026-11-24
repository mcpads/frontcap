//! Native simulator app identity independent of an optional controlled page.
use super::request::{InvalidRequest, canonical_uuid};
use crate::process::ProcessIdentity;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub simulator: String,
    pub process: ProcessIdentity,
    pub attachment_sha256: String,
}
impl Owner {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if !canonical_uuid(&self.simulator.to_ascii_lowercase())
            || self.process.pid == 0
            || self.process.pid > i32::MAX as u32
            || self.process.started.is_empty()
            || self.attachment_sha256.len() != 64
            || !self
                .attachment_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(InvalidRequest("invalid native iOS host owner"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub owner: Owner,
    pub contacts: Option<super::contacts::Endpoint>,
    #[serde(default)]
    pub pages: bool,
    #[serde(default)]
    pub app_lifecycle: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_ui: Option<super::ios_ui::UiIdentity>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub owner: Owner,
    pub path: std::path::PathBuf,
    pub sha256: String,
    pub bytes: u64,
    pub input: Option<super::contacts::CaptureInput>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Restart { previous: Owner },
    SetAppState { owner: Box<Owner>, state: super::app_state::State },
    BeginEntry { previous: Owner, url: String },
    PollEntry { begin: crate::ids::RequestId },
    DispatchUrl { owner: Box<Owner>, url: String },
    ObserveSystemAlerts,
    TapSystemButton { observation: String, reference: String },
    DiscoverPages,
    AttachPage {
        discovery: crate::ids::RequestId,
        page: String,
    },
    ReleasePage {
        attachment: crate::ids::RequestId,
    },
    RetirePage {
        attachment: crate::ids::RequestId,
    },
    Page {
        attachment: crate::ids::RequestId,
        command: PageOperation,
    },
    Observe {
        input_basis: bool,
    },
    Contacts {
        observation: crate::ids::RequestId,
        input: super::contacts::Operation,
    },
    ReconcileContacts {
        request_id: crate::ids::RequestId,
    },
}
impl Operation {
    pub fn system_operation(&self) -> Option<super::ios_ui::UiOperation> {
        match self {
            Self::ObserveSystemAlerts => Some(super::ios_ui::UiOperation::ObserveSystemAlerts {}),
            Self::TapSystemButton { observation, reference } => Some(super::ios_ui::UiOperation::TapSystemButton { observation: observation.clone(), reference: reference.clone() }),
            _ => None,
        }
    }
    pub fn requires_contacts(&self) -> bool {
        matches!(
            self,
            Self::Contacts { .. }
                | Self::ReconcileContacts { .. }
                | Self::Observe { input_basis: true }
        )
    }
    pub fn requires_pages(&self) -> bool {
        matches!(
            self,
            Self::DiscoverPages
                | Self::AttachPage { .. }
                | Self::ReleasePage { .. }
                | Self::RetirePage { .. }
                | Self::Page { .. }
        )
    }
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if matches!(self, Self::AttachPage {page,..} if !canonical_uuid(page))
            || matches!(self, Self::Page {command:PageOperation::Advance {delta_ns},..} if *delta_ns > 9_007_199_254_740_991)
        {
            return Err(InvalidRequest("invalid iOS page selection or advance"));
        }
        if let Self::SetAppState { owner, .. } = self { owner.validate()?; }
        if let Self::Restart { previous } | Self::BeginEntry { previous, .. } = self { previous.validate()?; }
        if let Self::BeginEntry { url, .. } = self { super::external_entry::validate_url(url)?; }
        if let Self::DispatchUrl { owner, url } = self {
            owner.validate()?;
            super::external_entry::validate_url(url)?;
        }
        if let Self::TapSystemButton { observation, reference } = self
            && !super::ios_targets::valid_reference(reference, observation) {
            return Err(InvalidRequest("invalid system alert button reference"));
        }
        if let Self::Contacts { input, .. } = self {
            input.validate()?;
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Completion {
    Restarted { previous: Box<Owner>, ready: Box<Ready> },
    StateChanged { transition: Box<StateTransition> },
    EntryBegun { entry: Box<Entry> },
    EntryPolled { begin: crate::ids::RequestId, ready: Option<Box<Ready>> },
    UrlAccepted { acceptance: Box<UrlAcceptance> },
    SystemUi { completion: Box<super::ios_ui::UiCompletion>, directory: std::path::PathBuf },
    Pages {
        owner: Box<Owner>,
        inventory: Inventory,
    },
    Attached {
        attachment: Box<Attachment>,
    },
    Released {
        attachment: Box<Attachment>,
    },
    Retired {
        attachment: Box<Attachment>,
    },
    Page {
        attachment: crate::ids::RequestId,
        reply: Box<super::webview::Reply>,
    },
    Observed {
        capture: Capture,
    },
    Contacts {
        receipt: super::contacts::Receipt,
    },
    ContactsReconciled {
        receipt: super::contacts::Receipt,
    },
}

/// Original process samples and independent runner evidence; no clock or page-lifetime claim.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateTransition {
    pub owner: Owner,
    pub before: ProcessIdentity,
    pub after: ProcessIdentity,
    pub completion: super::ios_ui::UiCompletion,
    pub directory: std::path::PathBuf,
}
impl StateTransition {
    pub fn state(&self) -> Result<super::app_state::State, InvalidRequest> {
        self.owner.validate()?;
        let super::ios_ui::UiOutcome::HostAppStateChanged { change } = &self.completion.outcome else {
            return Err(InvalidRequest("missing independent host visibility"));
        };
        change.validate()?;
        if self.before != self.owner.process || self.after != self.before || change.pid != self.before.pid {
            return Err(InvalidRequest("host visibility process changed"));
        }
        Ok(change.requested)
    }
}

/// OS acceptance bound to the original process; app delivery and destination remain unobserved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UrlAcceptance {
    pub owner: Owner,
    pub url: String,
    pub confirmation: super::external_entry::Observation,
    pub destination: super::external_entry::Observation,
}

/// One admitted URL dispatch bound to an original app and its pending replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntryAcceptance {
    pub previous: Owner,
    pub url: String,
    pub confirmation: super::external_entry::Observation,
    pub destination: super::external_entry::Observation,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry { pub begin: crate::ids::RequestId, pub acceptance: EntryAcceptance }
impl Entry {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.acceptance.previous.validate()?;
        super::external_entry::validate_url(&self.acceptance.url)?;
        if self.acceptance.confirmation != super::external_entry::Observation::Unobserved
            || self.acceptance.destination != super::external_entry::Observation::Unobserved {
            return Err(InvalidRequest("entry acceptance cannot assert confirmation or destination"));
        }
        Ok(())
    }
}

/// Platform registry evidence, not authority to choose an implicit current page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    /// Stable native view identity; several document channels can share it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<String>,
    #[serde(default)]
    pub superseded: bool,
    #[serde(default)]
    pub url_truncated: bool,
    pub id: String,
    pub endpoint: super::webview::Endpoint,
    pub alive: bool,
    pub control_stopped: bool,
    pub url: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    /// Main-frame document starts, not a navigation commit or visual-readiness guarantee.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub navigation: Option<super::ios_navigation::History>,
    pub pid: u32,
    pub revision: u64,
    pub capacity_exceeded: bool,
    pub pages: Vec<Page>,
}
impl Inventory {
    pub fn validate(&self, owner: &Owner) -> Result<(), InvalidRequest> {
        owner.validate()?;
        if self.pid != owner.process.pid || self.revision == 0 || self.pages.len() > 64 {
            return Err(InvalidRequest("invalid iOS page inventory"));
        }
        let mut ids = std::collections::HashSet::new();
        let mut generations = std::collections::HashSet::new();
        for page in &self.pages {
            page.endpoint.validate(&super::request::ClockRequest {
                generation: page.endpoint.generation.clone(),
                request_id: 1,
                operation: super::request::ClockOperation::Read,
                delta_ns: 0,
            })?;
            if !canonical_uuid(&page.id)
                || page.view.as_ref().is_some_and(|id| !canonical_uuid(id))
                || page.document.as_ref().is_some_and(|id| !canonical_uuid(id))
                || (page.superseded && (page.view.is_none() || page.document.is_none()))
                || page.endpoint.pid != self.pid
                || !ids.insert(&page.id)
                || !generations.insert(&page.endpoint.generation)
                || page.url.len() > 8192
                || (page.alive && page.control_stopped)
            {
                return Err(InvalidRequest("invalid iOS page identity"));
            }
        }
        if self.navigation.as_ref().is_some_and(|h| !h.valid()
            || h.entries.iter().any(|e| !ids.contains(&e.page))) {
            return Err(InvalidRequest("invalid iOS document history"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub id: crate::ids::RequestId,
    pub owner: super::webview::Owner,
    pub page: Page,
    pub document: String,
}
impl Attachment {
    pub fn validate(&self, owner: &Owner) -> Result<(), InvalidRequest> {
        self.owner.validate()?;
        Inventory {
            navigation: None,
            pid: owner.process.pid,
            revision: 1,
            capacity_exceeded: false,
            pages: vec![self.page.clone()],
        }
        .validate(owner)?;
        if self.owner.simulator != owner.simulator
            || self.owner.process != owner.process
            || self.owner.attachment_sha256 != owner.attachment_sha256
            || self.owner.endpoint != self.page.endpoint
            || !self.page.alive
            || self.page.control_stopped
            || self.page.superseded
            || self.page.document.as_ref().is_some_and(|id| id != &self.document)
            || !canonical_uuid(&self.document)
        {
            return Err(InvalidRequest(
                "iOS page attachment differs from native owner",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum PageOperation {
    Read,
    Advance { delta_ns: u64 },
    Reconcile { request_id: crate::ids::RequestId },
}
impl PageOperation {
    pub fn clock(&self) -> Option<(super::request::ClockOperation, u64)> {
        use super::request::ClockOperation as C;
        match self {
            Self::Read => Some((C::Read, 0)),
            Self::Advance { delta_ns } => Some((C::Advance, *delta_ns)),
            Self::Reconcile { .. } => None,
        }
    }
}
