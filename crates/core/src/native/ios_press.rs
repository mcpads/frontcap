//! Source touch ordering and the exact clock delivery inside one UIKit press.
use super::{
    ios::{ClockIdentity, ClockStatus, validate_clock_effect},
    ios_targets::valid_reference,
    request::{ClockOperation, ClockRequest, InvalidRequest},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputRole {
    None,
    TextField,
    TextView,
}

#[cfg(test)]
#[path = "ios_press_tests.rs"]
mod tests;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Touch {
    pub phase: u8,
    pub timestamp: serde_json::Number,
    pub x: serde_json::Number,
    pub y: serde_json::Number,
    pub view: String,
    pub hit: String,
    pub identifier: String,
    pub label: String,
    pub input_identifier: String,
    pub input_label: String,
    pub input_role: InputRole,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Boundary {
    pub generation: String,
    pub serial: u64,
    pub now_ns: u64,
    pub touches: Vec<Touch>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockReceipt {
    pub request: ClockRequest,
    pub generation: String,
    pub pid: u32,
    pub port: u16,
    pub last_id: u64,
    pub status: ClockStatus,
    pub before_ns: u64,
    pub now_ns: u64,
    pub applied: bool,
    pub advance_result: Option<u32>,
    pub drain_result: Option<u32>,
    pub replayed: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub input: Delivery,
    pub reference: String,
    pub observation: String,
    pub before: Boundary,
    pub began: Boundary,
    pub ended: Boundary,
    pub clock_request: ClockRequest,
    pub clock_reply: ClockReceipt,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    #[serde(rename = "xctest_timed_press")]
    TimedPress,
}
impl Evidence {
    pub fn validate(
        &self,
        app: &ClockIdentity,
        reference: &str,
        observation: &str,
        request: &ClockRequest,
    ) -> Result<(), InvalidRequest> {
        app.validate_request(request)?;
        let clock = &self.clock_reply;
        validate_clock_effect(
            request,
            clock.status,
            clock.before_ns,
            clock.now_ns,
            clock.applied,
            clock.advance_result,
            clock.drain_result,
        )?;
        if request.operation != ClockOperation::Advance
            || !valid_reference(reference, observation)
            || self.reference != reference
            || self.observation != observation
            || &self.clock_request != request
            || &clock.request != request
            || clock.generation != app.generation
            || clock.pid != app.pid
            || clock.port == 0
            || clock.last_id != request.request_id
            || clock.replayed
            || clock.status != ClockStatus::Ok
            || self.before.serial.checked_add(1) != Some(self.began.serial)
            || self.ended.serial <= self.began.serial
            || self.before.now_ns > self.began.now_ns
            || self.began.now_ns != clock.before_ns
            || self.ended.now_ns != clock.now_ns
            || self.began.touches.len() != 1
            || self.ended.touches.len() != 1
        {
            return Err(InvalidRequest(
                "UIKit press does not bind clock delivery and touch ordering",
            ));
        }
        for boundary in [&self.before, &self.began, &self.ended] {
            if boundary.generation != app.generation
                || boundary.now_ns > i64::MAX as u64
                || boundary.touches.len() > 16
            {
                return Err(InvalidRequest("invalid UIKit input boundary"));
            }
            for touch in &boundary.touches {
                if touch.phase > 4
                    || [
                        &touch.view,
                        &touch.hit,
                        &touch.identifier,
                        &touch.label,
                        &touch.input_identifier,
                        &touch.input_label,
                    ]
                    .iter()
                    .any(|s| s.encode_utf16().count() > 4096)
                    || [&touch.timestamp, &touch.x, &touch.y]
                        .iter()
                        .any(|n| !n.as_f64().is_some_and(f64::is_finite))
                {
                    return Err(InvalidRequest("invalid UIKit source touch"));
                }
            }
        }
        let begin = &self.began.touches[0];
        let end = &self.ended.touches[0];
        if begin.phase != 0
            || end.phase != 3
            || begin.input_role == InputRole::None
            || begin.input_role != end.input_role
            || begin.input_identifier != end.input_identifier
            || begin.input_label != end.input_label
            || (begin.input_identifier.is_empty() && begin.input_label.is_empty())
        {
            return Err(InvalidRequest("UIKit press ended on another input target"));
        }
        Ok(())
    }
}
