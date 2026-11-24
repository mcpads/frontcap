//! Same-install UIKit replacement evidence; saved records never grant live ownership.
use super::{
    ios_request::Owner,
    ios_ui::{UiCompletion, UiOutcome},
    request::InvalidRequest,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Installation {
    pub bundle: String,
    pub app: PathBuf,
    pub data: PathBuf,
    pub data_device: u64,
    pub data_inode: u64,
    pub app_sha256: String,
}
impl Installation {
    pub fn validate(&self, owner: &Owner) -> Result<(), InvalidRequest> {
        super::external_entry::Target::Ios {
            simulator: owner.app.simulator.to_ascii_lowercase(),
            bundle: self.bundle.clone(),
        }
        .validate()?;
        if !self.app.is_absolute()
            || !self.data.is_absolute()
            || self.app == self.data
            || self.app_sha256.len() != 64
            || !self
                .app_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(InvalidRequest("invalid UIKit installation evidence"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Replacement {
    pub previous: Owner,
    pub owner: Owner,
    pub installation: Installation,
    pub completion: UiCompletion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<super::external_entry::Accepted>,
}
impl Replacement {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.previous.validate()?;
        self.owner.validate()?;
        self.installation.validate(&self.owner)?;
        let expected = restart_entry(
            &self.previous,
            &self.installation.bundle,
            match &self.completion.request.operation {
                super::ios_ui::UiOperation::Restart { url, .. } => url.as_deref(),
                _ => None,
            },
        )?;
        if self.entry != expected.map(super::external_entry::Accepted::new) {
            return Err(InvalidRequest(
                "UIKit replacement entry differs from original request",
            ));
        }
        if self.previous.runner != self.owner.runner
            || self.previous.source_sha256 != self.owner.source_sha256
            || self.previous.controller_id != self.owner.controller_id
        {
            return Err(InvalidRequest(
                "UIKit replacement changed its runner or source",
            ));
        }
        self.completion.validate(
            &self.previous.runner,
            &self.completion.request,
            Some(&self.previous.app),
        )?;
        if !matches!(&self.completion.outcome,UiOutcome::Restarted { app,.. } if app==&self.owner.app)
        {
            return Err(InvalidRequest(
                "UIKit replacement differs from its original receipt",
            ));
        }
        Ok(())
    }
}

/// Bind a URL launch to the original controlled app and installed bundle.
pub fn restart_entry(
    owner: &Owner,
    bundle: &str,
    url: Option<&str>,
) -> Result<Option<super::external_entry::Request>, InvalidRequest> {
    let Some(url) = url else {
        return Ok(None);
    };
    owner.validate()?;
    let request = super::external_entry::Request {
        target: super::external_entry::Target::Ios {
            simulator: owner.app.simulator.to_ascii_lowercase(),
            bundle: bundle.into(),
        },
        generation: owner.app.generation.clone(),
        url: url.into(),
    };
    request.validate()?;
    Ok(Some(request))
}

/// XCTest host state only. A background state does not establish page disposal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateChange {
    pub requested: super::app_state::State,
    pub before_state: u8,
    pub app_state: u8,
    pub now_ns: Option<u64>,
}
impl StateChange {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        use super::app_state::State;
        let valid = match self.requested {
            State::Background => self.before_state == 4 && matches!(self.app_state, 2 | 3) && self.now_ns.is_none(),
            State::Foreground => matches!(self.before_state, 2 | 3) && self.app_state == 4
                && self.now_ns.is_some_and(|n| n <= i64::MAX as u64),
        };
        if valid { Ok(()) } else { Err(InvalidRequest("unconfirmed UIKit host state transition")) }
    }
}
/// Public same-process transition: native state plus independently retained host process samples.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transition {
    pub owner: Owner,
    pub process_before: crate::process::ProcessIdentity,
    pub process_after: crate::process::ProcessIdentity,
    pub completion: UiCompletion,
}
impl Transition {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.owner.validate()?;
        if self.process_before != self.process_after || self.process_before.pid != self.owner.app.pid
            || self.process_before.started.is_empty() {
            return Err(InvalidRequest("UIKit transition replaced or lost its app process"));
        }
        self.completion.validate(&self.owner.runner, &self.completion.request, Some(&self.owner.app))?;
        if !matches!(self.completion.outcome, UiOutcome::AppStateChanged { .. }) {
            return Err(InvalidRequest("missing UIKit host transition receipt"));
        }
        Ok(())
    }
}
