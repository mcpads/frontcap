//! Native acquisition identity and retirement evidence.
use super::request::Identity;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub identity: Identity,
    pub controller_id: String,
    pub source_sha256: String,
    pub apk_sha256: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Retirement {
    pub passed: bool,
    pub errors: Vec<String>,
    pub signal_error: Option<String>,
}
