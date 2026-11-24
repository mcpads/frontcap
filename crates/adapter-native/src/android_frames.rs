//! Host admission for the document-local frame producer. No timer-domain advancement.
use serde::{Deserialize, Serialize};
use std::io;
pub use frontcap_core::native::android_frames::{Request, State, Outcome, Receipt};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Ledger {
    document: String,
    completed: Option<Request>,
    pending: Option<Request>,
}
impl Ledger {
    pub(crate) fn inspect(&self) -> frontcap_core::native::android_progress::Frames {
        frontcap_core::native::android_progress::Frames {
            document: self.document.clone(), completed: self.completed.clone(), pending: self.pending.clone(),
        }
    }
    pub(crate) fn document(&self)->&str { &self.document }
    pub fn idle_document(&self)->io::Result<&str> {
        if self.pending.is_some() {return Err(io::Error::other("frame producer request is pending"));}
        Ok(&self.document)
    }
    pub fn validate(&self) -> io::Result<()> {
        if !frontcap_core::native::request::canonical_uuid(&self.document) {
            return Err(io::Error::other("invalid retained frame document"));
        }
        if let Some(completed) = &self.completed {
            completed.validate()?;
            if completed.document != self.document {
                return Err(io::Error::other("retained completion document differs"));
            }
        }
        if let Some(pending) = &self.pending {
            let mut before = self.clone();
            before.pending = None;
            before.admit(pending)?;
        }
        Ok(())
    }
    pub fn bind(receipt: &Receipt) -> io::Result<Self> {
        receipt.validate()?;
        if receipt.legacy_used || receipt.last.is_some() {
            return Err(io::Error::other(
                "frame producer already used; original owner required",
            ));
        }
        Ok(Self {
            document: receipt.document.clone(),
            completed: None,
            pending: None,
        })
    }
    pub fn admit(&mut self, request: &Request) -> io::Result<()> {
        request.validate()?;
        if self.pending.is_some()
            || request.document != self.document
            || request.id != self.completed.as_ref().map_or(1, |r| r.id + 1)
            || self
                .completed
                .as_ref()
                .is_some_and(|r| request.timestamp < r.timestamp)
        {
            return Err(io::Error::other(
                "frame request violates retained producer frontier",
            ));
        }
        self.pending = Some(request.clone());
        Ok(())
    }
    pub fn observe(&mut self, receipt: &Receipt) -> io::Result<()> {
        receipt.validate()?;
        if receipt.document != self.document || receipt.legacy_used {
            return Err(io::Error::other("frame document or control changed"));
        }
        let expected = self.pending.as_ref().or(self.completed.as_ref());
        if receipt.last.as_ref().map(|last| &last.request) != expected {
            return Err(io::Error::other(
                "frame producer differs from retained request",
            ));
        }
        if let Some(last) = &receipt.last
            && last.state == State::Completed
        {
            self.completed = Some(last.request.clone());
            self.pending = None;
        }
        Ok(())
    }
}
#[cfg(test)]
#[path = "android_frames_tests.rs"]
mod tests;
