//! Shared requests and receipts for the Android document-local frame producer.
use serde::{Deserialize, Serialize};
use std::io;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub document: String,
    pub id: u64,
    pub timestamp: f64,
}
impl Request {
    pub fn validate(&self) -> io::Result<()> {
        if !crate::native::request::canonical_uuid(&self.document)
            || self.id == 0
            || self.id > 9_007_199_254_740_991
            || !self.timestamp.is_finite()
            || self.timestamp < 0.0
        {
            return Err(io::Error::other("invalid frame producer request"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Pending,
    Completed,
    Unresolved,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub request: Request,
    pub state: State,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub document: String,
    #[serde(rename = "legacyUsed")]
    pub legacy_used: bool,
    pub last: Option<Outcome>,
}
impl Receipt {
    pub fn validate(&self) -> io::Result<()> {
        if !crate::native::request::canonical_uuid(&self.document) {
            return Err(io::Error::other("invalid frame document"));
        }
        if let Some(last) = &self.last {
            last.request.validate()?;
            if last.request.document != self.document
                || match last.state {
                    State::Pending => last.result.is_some() || last.error.is_some(),
                    State::Completed => {
                        last.result
                            .as_ref()
                            .and_then(|result| result["timestamp"].as_f64())
                            != Some(last.request.timestamp)
                            || last.error.is_some()
                    }
                    State::Unresolved => last.error.is_none(),
                }
            {
                return Err(io::Error::other("inconsistent frame producer receipt"));
            }
        }
        Ok(())
    }
}

/// Current document-local callback queue, distinct from the last dispatched frame request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activity {
    pub document: String,
    pub pending: u64,
    pub dispatching: bool,
    #[serde(rename = "lastTimestamp")]
    pub last_timestamp: Option<f64>,
    pub delivered: u64,
}
impl Activity {
    pub fn valid(&self) -> bool {
        crate::native::request::canonical_uuid(&self.document)
            && self.pending <= 9_007_199_254_740_991
            && self.delivered <= 9_007_199_254_740_991
            && self.last_timestamp.is_none_or(|value| value.is_finite() && value >= 0.0)
            && (!self.dispatching || self.last_timestamp.is_some())
    }
}

#[cfg(test)]
#[path = "android_frames_tests.rs"]
mod activity_tests;
