//! Caller-visible cold entry phases. OS acceptance remains separate from destination evidence.
use super::{
    external_entry::Accepted,
    flutter::Owner,
    flutter_lifecycle::{self, Process, Replacement},
    request::InvalidRequest,
};
use crate::ids::RequestId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pending {
    pub request_id: RequestId,
    pub previous: Owner,
    pub acceptance: Accepted,
}
impl Pending {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        let request = &self.acceptance.request;
        if flutter_lifecycle::ios_entry(&self.previous, Some(&request.target), &request.url)?
            != *request
        {
            return Err(InvalidRequest(
                "pending Flutter entry differs from previous owner",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum Waiting {
    Process {},
    Endpoint { process: Process },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Progress {
    Waiting {
        entry: Pending,
        readiness: Waiting,
    },
    Ready {
        entry: Pending,
        replacement: Box<Replacement>,
    },
}
impl Progress {
    pub fn entry(&self) -> &Pending {
        match self {
            Self::Waiting { entry, .. } | Self::Ready { entry, .. } => entry,
        }
    }
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.entry().validate()?;
        match self {
            Self::Waiting {
                readiness: Waiting::Process {},
                ..
            } => Ok(()),
            Self::Waiting {
                readiness: Waiting::Endpoint { process },
                ..
            } => process.validate(),
            Self::Ready { entry, replacement } => {
                replacement.validate()?;
                if replacement.previous() != &entry.previous
                    || !matches!(replacement.as_ref(), Replacement::Ios(r) if r.entry.as_ref() == Some(&entry.acceptance))
                {
                    return Err(InvalidRequest(
                        "Flutter entry readiness differs from original acceptance",
                    ));
                }
                Ok(())
            }
        }
    }
}
