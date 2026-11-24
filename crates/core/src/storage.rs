//! Explicit persistent storage partition selection.
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoragePartition {
    /// Canonical schemeful site (scheme plus registrable domain, without a path or port).
    pub top_level_site: String,
    /// Omitted infers cross-site ancestry from the origin and top-level site.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub has_cross_site_ancestor: Option<bool>,
}

/// Exact saved policy, installed before cache and IndexedDB content or application scripts.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialBucket {
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<StoragePartition>,
    /// Absent selects the default bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Canonical signed decimal Windows-epoch microseconds; absent means no expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiration: Option<String>,
    /// Canonical nonnegative signed-64-bit decimal bytes; zero selects the browser default.
    pub quota: String,
    pub persistent: bool,
    pub durability: BucketDurability,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BucketDurability {
    Strict,
    Relaxed,
}
