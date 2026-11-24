//! iOS app-clock evidence. XCTest has a separate identity and request sequence.
use super::request::{ClockOperation, ClockRequest, InvalidRequest, canonical_uuid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockIdentity {
    pub simulator: String,
    pub generation: String,
    pub pid: u32,
}
impl ClockIdentity {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if !canonical_uuid(&self.simulator.to_ascii_lowercase())
            || !canonical_uuid(&self.generation)
            || self.pid == 0
            || self.pid > i32::MAX as u32
        {
            return Err(InvalidRequest("invalid iOS app-clock identity"));
        }
        Ok(())
    }
    pub fn validate_request(&self, request: &ClockRequest) -> Result<(), InvalidRequest> {
        self.validate()?;
        if request.generation != self.generation
            || request.request_id == 0
            || request.request_id > i64::MAX as u64
            || request.delta_ns > i64::MAX as u64
            || (request.operation == ClockOperation::Read && request.delta_ns != 0)
        {
            return Err(InvalidRequest(
                "iOS clock request differs from its bound identity or range",
            ));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockStatus {
    Ok,
    Failed,
}

/// Failed advances consume their request number; drain failure may follow an applied advance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockCompletion {
    pub request: ClockRequest,
    pub generation: String,
    pub pid: u32,
    pub last_id: u64,
    pub status: ClockStatus,
    pub before_ns: u64,
    pub now_ns: u64,
    pub applied: bool,
    pub advance_result: Option<u32>,
    pub drain_result: Option<u32>,
    pub receipt_sha256: String,
}
impl ClockCompletion {
    pub fn validate(
        &self,
        identity: &ClockIdentity,
        request: &ClockRequest,
    ) -> Result<(), InvalidRequest> {
        identity.validate_request(request)?;
        validate_clock_effect(
            request,
            self.status,
            self.before_ns,
            self.now_ns,
            self.applied,
            self.advance_result,
            self.drain_result,
        )?;
        if &self.request != request
            || self.generation != identity.generation
            || self.pid != identity.pid
            || self.last_id != request.request_id
            || self.receipt_sha256.len() != 64
            || !self
                .receipt_sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(InvalidRequest(
                "iOS receipt does not resolve the bound clock request",
            ));
        }
        Ok(())
    }
}

/// Validate the mutation independently of transport identity and receipt storage.
pub fn validate_clock_effect(
    request: &ClockRequest,
    status: ClockStatus,
    before_ns: u64,
    now_ns: u64,
    applied: bool,
    advance_result: Option<u32>,
    drain_result: Option<u32>,
) -> Result<(), InvalidRequest> {
    let fields = match request.operation {
        ClockOperation::Read => {
            status == ClockStatus::Ok
                && !applied
                && advance_result.is_none()
                && drain_result.is_none()
        }
        ClockOperation::Advance => match (advance_result, drain_result) {
            (Some(0), Some(drain)) => applied && (status == ClockStatus::Ok) == (drain == 0),
            (Some(advance), None) if advance != 0 => !applied && status == ClockStatus::Failed,
            _ => false,
        },
    };

    if !fields
        || now_ns > i64::MAX as u64
        || before_ns.checked_add(if applied { request.delta_ns } else { 0 }) != Some(now_ns)
    {
        return Err(InvalidRequest("inconsistent iOS clock mutation outcome"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "ios_tests.rs"]
mod tests;
