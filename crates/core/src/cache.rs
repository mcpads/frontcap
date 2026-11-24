//! Explicit initial contents of a CacheStorage cache.
use std::collections::BTreeMap;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialCache {
    pub origin: String,
    /// Explicit partition; omitted keeps the document key for its own origin, otherwise first-party.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<crate::storage::StoragePartition>,
    /// Omitted selects the default bucket; `initial_buckets` supplies explicit policies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    pub name: String,
    pub entries: Vec<CacheEntry>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheEntry {
    /// Canonical absolute HTTP(S) URL for a synthetic GET request.
    pub request_url: String,
    #[serde(default)]
    pub request_headers: BTreeMap<String, String>,
    pub status: u16,
    #[serde(default)]
    pub status_text: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: String,
    /// Base64 bytes instead of body text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_base64: Option<String>,
    /// Native persisted response attributes; absent constructs a synthetic response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<CacheResponseMetadata>,
}

impl CacheEntry {
    pub fn body_bytes(&self) -> Option<Vec<u8>> {
        match &self.body_base64 {
            Some(value) if self.body.is_empty() => crate::encode::unbase64(value),
            Some(_) => None,
            None => Some(self.body.as_bytes().to_vec()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheInitializationRecord {
    pub cache: InitialCache,
    pub storage_key: String,
}

/// Stored response metadata and bytes. Native metadata restores recorded attributes of fetched responses;
/// omitting it constructs a synthetic default response.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheResponse {
    pub status: u16,
    #[serde(default)]
    pub status_text: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub response_type: CacheResponseType,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body_base64: Option<String>,
    /// Native persisted response attributes; absent constructs a synthetic response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<CacheResponseMetadata>,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheResponseType {
    #[default]
    Default,
    Basic,
    Cors,
    Error,
    OpaqueResponse,
    OpaqueRedirect,
}
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum CacheRecord {
    Present { response: Box<CacheResponse> },
    Absent,
}

/// Browser-persisted response attributes. Decimal strings retain exact 64-bit values.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CacheResponseMetadata {
    pub response_type: CacheResponseType,
    pub urls: Vec<String>,
    pub cors_exposed_header_names: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_method: Option<String>,
    pub response_time: String,
    pub padding: String,
    pub connection_info: i64,
    pub alpn_negotiated_protocol: String,
    pub was_fetched_via_spdy: bool,
    pub request_include_credentials: bool,
}
