//! Lossless DOM storage recipe data. Session owners are logical page contexts,
//! never browser-process namespace IDs. Native bytes are not decoded as JSON strings.
use crate::storage::StoragePartition;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomStorageEncoding {
    /// Chromium local storage format-tagged strings; session UTF-8 keys and
    /// UTF-16 values. UTF-16 code units use little-endian byte order.
    ChromiumLittleEndian,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomStorageEntry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomStorageArea {
    pub origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<StoragePartition>,
    pub entries: Vec<DomStorageEntry>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStorageState {
    /// Generation context, such as `page` or an enrolled popup's context name.
    pub context: String,
    pub areas: Vec<DomStorageArea>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DomStorageState {
    pub encoding: DomStorageEncoding,
    /// Shared once per browser storage partition, not repeated per window.
    pub local: Vec<DomStorageArea>,
    /// Separate even when multiple contexts use the same origin and keys.
    pub sessions: Vec<SessionStorageState>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DomStorageError(pub String);

impl DomStorageState {
    /// Structural validation only. Before any write, the adapter must resolve
    /// every scope through the browser, reject canonical aliases, verify encoding
    /// support and map every session context to a construction-owned target.
    pub fn validate(&self) -> Result<(), DomStorageError> {
        validate_areas(&self.local, "local storage")?;
        let mut contexts = BTreeSet::new();
        for session in &self.sessions {
            if session.context.is_empty() || !contexts.insert(&session.context) {
                return Err(DomStorageError("session storage needs unique nonempty context names".into()));
            }
            validate_areas(&session.areas, &format!("session storage context {}", session.context))?;
        }
        Ok(())
    }
}

fn validate_areas(areas: &[DomStorageArea], owner: &str) -> Result<(), DomStorageError> {
    let mut scopes = BTreeSet::new();
    for area in areas {
        if area.origin.is_empty() || area.partition.as_ref().is_some_and(|p| p.top_level_site.is_empty()) {
            return Err(DomStorageError(format!("{owner} has an empty origin or top-level site")));
        }
        if !scopes.insert((&area.origin, &area.partition)) {
            return Err(DomStorageError(format!("{owner} repeats a storage scope")));
        }
        let mut keys = BTreeSet::new();
        if area.entries.iter().any(|entry| !keys.insert(&entry.key)) {
            return Err(DomStorageError(format!("{owner} repeats a raw storage key")));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "dom_storage_tests.rs"]
mod tests;
