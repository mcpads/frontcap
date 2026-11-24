//! Explicit JSON-valued initial IndexedDB contents in a selected storage bucket.
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialDatabase {
    pub origin: String,
    /// Explicit partition; omitted keeps the document key for its own origin, otherwise first-party.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<crate::storage::StoragePartition>,
    /// Omitted selects the default bucket; `initial_buckets` supplies explicit policies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    pub name: String,
    pub version: u64,
    pub stores: Vec<ObjectStore>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectStore {
    pub name: String,
    #[serde(default)]
    pub key_path: Option<KeyPath>,
    #[serde(default)]
    pub auto_increment: bool,
    #[serde(default)]
    pub indexes: Vec<Index>,
    #[serde(default)]
    pub records: Vec<Record>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum KeyPath {
    String(String),
    Array(Vec<String>),
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    pub name: String,
    pub key_path: KeyPath,
    #[serde(default)]
    pub unique: bool,
    #[serde(default)]
    pub multi_entry: bool,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<Key>,
    pub value: serde_json::Value,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Key {
    Number(serde_json::Number),
    String(String),
    /// Integer milliseconds since the Unix epoch, within the JavaScript Date range.
    Date(i64),
    Array(Vec<Key>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseInitializationRecord {
    pub database: InitialDatabase,
    pub storage_key: String,
}
