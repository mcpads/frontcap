//! Committed UTF-16 edits in a fresh, focused Android source View. No IME composition claim.
use super::request::{InvalidRequest, canonical_uuid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub generation: String,
    pub controller_id: String,
    pub request_id: u64,
    pub observation: String,
    pub target: String,
    pub now_ns: u64,
    pub start: u32,
    pub end: u32,
    pub text: String,
}
impl Request {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if [
            &self.generation,
            &self.controller_id,
            &self.observation,
            &self.target,
        ]
        .iter()
        .any(|s| !canonical_uuid(s))
            || self.request_id == 0
            || self.request_id > i64::MAX as u64
            || self.now_ns > i64::MAX as u64
            || self.text.encode_utf16().count() > 16384
        {
            return Err(InvalidRequest("invalid Android edit request"));
        }
        Ok(())
    }
    /// Derive the requested text independently of the source acknowledgement, preserving both sides.
    pub fn apply_to(&self, before: &str) -> Result<String, InvalidRequest> {
        self.validate()?;
        let units: Vec<_> = before.encode_utf16().collect();
        let (start, end) = (self.start as usize, self.end as usize);
        if units.len() > 16384 || start > end || end > units.len() {
            return Err(InvalidRequest("Android edit range exceeds text"));
        }
        let prefix = String::from_utf16(&units[..start])
            .map_err(|_| InvalidRequest("Android edit splits a surrogate pair"))?;
        let suffix = String::from_utf16(&units[end..])
            .map_err(|_| InvalidRequest("Android edit splits a surrogate pair"))?;
        let result = prefix + &self.text + &suffix;
        if result.encode_utf16().count() > 16384 {
            return Err(InvalidRequest("Android edit result exceeds text budget"));
        }
        Ok(result)
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Completed,
    Rejected,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    StaleObservation,
    ChangedViewState,
    UnsupportedTarget,
    InvalidRange,
    SplitSurrogate,
    TextBudgetExceeded,
    FieldLimitExceeded,
    InvalidUnicode,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub request: Request,
    pub generation: String,
    pub pid: u32,
    pub now_ns: u64,
    pub last_id: u64,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
}
impl Receipt {
    pub fn validate(
        &self,
        request: &Request,
        generation: &str,
        pid: u32,
    ) -> Result<(), InvalidRequest> {
        request.validate()?;
        if self.request != *request
            || self.generation != generation
            || request.generation != generation
            || self.pid != pid
            || pid == 0
            || pid > i32::MAX as u32
            || self.last_id != request.request_id
            || self.now_ns > i64::MAX as u64
        {
            return Err(InvalidRequest(
                "Android edit receipt differs from bound request/owner",
            ));
        }
        match self.state {
            State::Completed => {
                let before = self
                    .before
                    .as_deref()
                    .ok_or(InvalidRequest("missing edit input"))?;
                if self.reason.is_some()
                    || self.now_ns != request.now_ns
                    || self.after.as_deref() != Some(request.apply_to(before)?.as_str())
                {
                    return Err(InvalidRequest(
                        "Android edit outcome differs from requested replacement",
                    ));
                }
            }
            State::Pending | State::Rejected => {
                if self.before.is_some()
                    || self.after.is_some()
                    || (self.state == State::Rejected) != self.reason.is_some()
                    || (self.state == State::Pending && self.now_ns != request.now_ns)
                {
                    return Err(InvalidRequest("contradictory Android edit status"));
                }
            }
        }
        Ok(())
    }
}


/// Caller observation identity plus the exact source basis, resolved from recorder-owned evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub observation: crate::ids::RequestId,
    pub source_observation: String,
    pub reference: String,
    pub now_ns: u64,
    pub before: String,
    pub start: u32,
    pub end: u32,
    pub text: String,
}
impl Edit {
    pub fn request(&self, generation: &str, controller: &str, number: u64) -> Result<Request, InvalidRequest> {
        let request = Request { generation:generation.into(),controller_id:controller.into(),request_id:number,
            observation:self.source_observation.clone(),target:self.reference.clone(),now_ns:self.now_ns,
            start:self.start,end:self.end,text:self.text.clone() };
        request.apply_to(&self.before)?;
        Ok(request)
    }
    pub fn validate_target(&self, capture: &super::android_capture::Capture) -> Result<(), InvalidRequest> {
        capture.validate()?;
        if self.source_observation != capture.after_request || self.now_ns != capture.now_ns {
            return Err(InvalidRequest("Android edit differs from source observation"));
        }
        let view=capture.catalog.targets.iter().find(|v|v.token==self.reference)
            .ok_or(InvalidRequest("Android edit target missing"))?;
        let result=self.request(&capture.generation,&capture.generation,1)?.apply_to(&self.before)?;
        if view.role != super::android_views::Role::TextField || !view.focused || !view.enabled
            || !view.visible || view.secure || view.transformed || view.text.as_deref()!=Some(&self.before)
            || view.edit_limit_utf16.is_none_or(|limit|result.encode_utf16().count()>limit as usize) {
            return Err(InvalidRequest("Android edit requires a focused editable target within its field limit"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "android_edit_tests.rs"]
mod tests;
