//! Controlled local service wire types. Transport completion does not prove app delivery.
use crate::ids::{LaunchId, RequestId};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, net::SocketAddr};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ResponseSpec {
    pub status: u16,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Inspect {
        generation: LaunchId,
    },
    Release {
        generation: LaunchId,
        id: RequestId,
        request: RequestId,
        response: ResponseSpec,
    },
    ReleaseAsset {
        generation: LaunchId,
        id: RequestId,
        request: RequestId,
        sha256: String,
    },
    Stop {
        generation: LaunchId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseState {
    Prepared,
    QueuedForHttp,
    ReceiverGone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseReceipt {
    pub command: Control,
    pub state: ReleaseState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transport {
    Open,
    Closed,
    Failed,
    TimedOut,
    Stopped,
}

/// Identity of bytes frozen before the service starts accepting requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetIdentity {
    pub sha256: String,
    pub size: u64,
    pub content_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_security_policy: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceRequest {
    pub uri: String,
    pub asset: AssetIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Received {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<ResourceRequest>,
    pub release: Option<RequestId>,
    pub transport: Transport,
}

/// An existing consumer's local HTTP endpoint. It is not a service-generation identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEndpoint {
    pub port: u16,
    pub request_path: String,
}
impl RequestEndpoint {
    pub fn validate(&self) -> Result<(), String> {
        let segments = self
            .request_path
            .strip_prefix('/')
            .and_then(|s| s.strip_suffix('/'));
        if self.port == 0
            || self.request_path.len() > 256
            || !segments.is_some_and(|path| {
                path.split('/').all(|part| {
                    !part.is_empty()
                        && part != "."
                        && part != ".."
                        && part
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
                })
            })
        {
            return Err("service endpoint requires a nonzero loopback port and an absolute request directory".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub generation: LaunchId,
    pub address: SocketAddr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_path: Option<String>,
    pub pid: u32,
    pub stopping: bool,
    pub requests: BTreeMap<RequestId, Received>,
    pub releases: BTreeMap<RequestId, ReleaseReceipt>,
}
impl Record {
    pub fn requests_path(&self) -> String {
        self.request_path
            .clone()
            .unwrap_or_else(|| format!("/{}/requests/", self.generation))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Snapshot { record: Record },
    Released { receipt: ReleaseReceipt },
    Stopping,
    Uncertain { reason: String },
    Rejected { reason: String },
}
