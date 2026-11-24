//! Native runner lifecycle evidence, separate from page visibility and resume messages.
use super::{ios_ui::UiIdentity, request::InvalidRequest, webview};
use crate::process::ProcessIdentity;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub app: webview::Owner,
    pub runner: UiIdentity,
    pub runner_process: ProcessIdentity,
    pub port: u16,
    pub products_sha256: String,
}
impl Owner {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.app.validate()?;
        self.runner.validate()?;
        if self.app.simulator != self.runner.simulator
            || self.runner_process.pid != self.runner.pid
            || self.runner_process.started.is_empty()
            || self.runner.pid == self.app.process.pid
            || self.port == 0
            || self.products_sha256.len() != 64
            || !self
                .products_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(InvalidRequest("invalid lifecycle owner"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operation {
    /// The platform runner's launch operation activates the existing consumer; it must not launch it.
    Launch,
    Background,
    Foreground,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Arguments {}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub generation: String,
    pub request_id: u64,
    pub operation: Operation,
    pub arguments: Arguments,
}
impl Request {
    pub fn validate(&self, owner: &Owner) -> Result<(), InvalidRequest> {
        owner.validate()?;
        if self.generation != owner.runner.generation
            || self.request_id == 0
            || self.request_id > i64::MAX as u64
        {
            return Err(InvalidRequest("lifecycle request identity differs"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    Rejected,
    Executing,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultState {
    pub operation: Operation,
    pub app_state: u8,
    pub frame_points: [f64; 4],
    pub input: String,
    pub touches: Option<()>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub generation: String,
    pub pid: u32,
    pub port: u16,
    pub request: Request,
    pub last_id: u64,
    pub replayed: bool,
    pub status: Status,
    pub result: Option<ResultState>,
    pub error: Option<String>,
}
impl Reply {
    /// Executing evidence is retained separately; it cannot complete a durable effect.
    pub fn validate_completion(
        &self,
        owner: &Owner,
        request: &Request,
    ) -> Result<(), InvalidRequest> {
        request.validate(owner)?;
        if self.generation != owner.runner.generation
            || self.pid != owner.runner.pid
            || self.port != owner.port
            || &self.request != request
        {
            return Err(InvalidRequest("lifecycle reply identity differs"));
        }
        let valid = match self.status {
            Status::Ok => {
                self.last_id == request.request_id
                    && self.error.is_none()
                    && self.result.as_ref().is_some_and(|r| {
                        r.operation == request.operation
                            && r.input == "none"
                            && r.touches.is_none()
                            && r.frame_points.iter().all(|v| v.is_finite())
                            && match request.operation {
                                Operation::Background => {
                                    matches!(r.app_state, 2 | 3) && r.frame_points == [0.0; 4]
                                }
                                Operation::Launch | Operation::Foreground => {
                                    r.app_state == 4
                                        && r.frame_points[2] > 0.0
                                        && r.frame_points[3] > 0.0
                                }
                            }
                    })
            }
            Status::Rejected => {
                self.last_id == request.request_id - 1
                    && self.result.is_none()
                    && self.error.as_ref().is_some_and(|e| !e.is_empty())
            }
            Status::Executing => false,
        };
        if !valid {
            return Err(InvalidRequest(
                "incomplete or inconsistent lifecycle result",
            ));
        }
        Ok(())
    }
}
