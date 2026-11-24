//! Typed direct state changes, separate from native user input and arbitrary evaluation.
use crate::{ids::RequestId, observation::ObservationBoundary, predicate::Locator};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// Cookie installed and verified before the first navigation request.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialCookie {
    pub origin: String,
    pub name: String,
    pub value: String,
    /// Absolute Unix milliseconds on the controlled clock; omit for a session cookie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<u64>,
    /// Omit for host-only; a domain scope must begin with a dot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    pub same_site: CookieSameSite,
    #[serde(default)]
    pub priority: CookiePriority,
    /// Native source metadata; omitted values derive from origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<CookieSource>,
    /// Chronology relative to the initial boundary; exclusive with expires_at_ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<CookieTiming>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition: Option<CookiePartition>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CookieSameSite { Strict, Lax, None, Unspecified }

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CookiePriority { Low, #[default] Medium, High }

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CookieTiming {
    pub creation_age_us: u64,
    pub last_access_age_us: u64,
    pub last_update_age_us: u64,
    /// Omit for a session cookie; persistent cookies retain their remaining lifetime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining_lifetime_us: Option<u64>,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CookieSource {
    pub scheme: CookieSourceScheme,
    /// -1 denotes an unspecified port; otherwise 1 through 65535.
    pub port: i64,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CookieSourceScheme { Unset, NonSecure, Secure }

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CookiePartition {
    /// Canonical schemeful site, not a full document URL.
    pub top_level_site: String,
    pub has_cross_site_ancestor: bool,
}

/// Storage prepared in the first committed top document before any application script can run.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitialStorage {
    pub origin: String,
    #[serde(default)]
    pub local: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub session: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitializationRecord {
    pub storage: InitialStorage,
    pub storage_key: String,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageArea { Local, Session }

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// As `state storage_scope` returns it for the target's document.
pub struct StorageScope {
    pub origin: String,
    /// Engine-issued serialized storage key, including partition identity.
    pub storage_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedStorage {
    pub boundary: ObservationBoundary,
    pub r#ref: String,
    pub scope: StorageScope,
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum StateField {
    /// Storage uses the target element's document as its context anchor; scope is explicitly checked.
    Storage { area: StorageArea, scope: StorageScope, key: String },
    /// Read only: the keys of local or session storage in the explicitly checked scope.
    StorageKeys { area: StorageArea, scope: StorageScope },
    /// Read only: the cookies the browser would send to the target element's document URL,
    /// HttpOnly ones included.
    Cookies,
    /// One cookie of the target document's origin by `name`, `path` (`/` by default), `domain` (a
    /// leading-dot domain cookie; host-only when omitted) and `partition`: `{value, expires_at_ms?, secure,
    /// http_only, same_site}`, or null when absent. A write sets it, read back the same way, or deletes it
    /// with null; `expires_at_ms` is absolute milliseconds on the controlled clock, after now and within
    /// 400 days, and omitted (or null) for a session cookie.
    Cookie {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        domain: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partition: Option<CookiePartition>,
    },
    /// A state provider's JSON value in the target's document (`frontcapState.register`): read by its
    /// `read()`, written by its `write(value)`, the app's own code; what the app loads when it starts is
    /// seeded as a recipe's initial storage instead.
    App { provider: String },
    /// Read only: the state providers the app registered in the target element's document.
    AppProviders,
    /// Read only: IndexedDB of the explicitly checked scope (a named `bucket`, or the default), read
    /// natively without running the page's tasks: its databases; a `database`'s stores with their record
    /// counts; a `store`'s first 100 records `{key, value}` and its total; or one record's value by `key`.
    IndexedDb {
        scope: StorageScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bucket: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        database: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        store: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<Value>,
    },
    /// One JSON record in an existing store. Read/write values are
    /// {"state":"present","value":...} or {"state":"absent"}; null remains a stored value.
    IndexedDbRecord {
        scope: StorageScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bucket: Option<String>,
        database: String,
        store: String,
        key: Value,
    },
    /// A GET cache entry matched natively, including Vary. Values are CacheRecord;
    /// writes replace the URL with a synthetic response regardless of Vary;
    /// absence uses Vary matching and deletes without creating a cache.
    CacheEntry {
        scope: StorageScope,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bucket: Option<String>,
        cache: String,
        request_url: String,
        #[serde(default)]
        request_headers: BTreeMap<String, String>,
    },
    Value,
    Checked,
    Text,
    Attribute { name: String },
    Style { name: String },
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum IndexedDbRecord {
    Present { value: Value },
    Absent,
}

impl StateField {
    /// Whether `state apply` can set this field; the others are read only.
    pub fn writable(&self) -> bool {
        !matches!(self, StateField::StorageKeys { .. } | StateField::Cookies | StateField::AppProviders | StateField::IndexedDb { .. })
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedValue { pub value: Value }

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateChange {
    pub target: Locator,
    pub field: StateField,
    pub value: Value,
    pub expected: Option<ExpectedValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateValue {
    pub boundary: ObservationBoundary,
    pub r#ref: String,
    pub field: StateField,
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeReceipt {
    pub index: usize,
    pub attempted: bool,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub verified: bool,
    pub error: Option<String>,
    /// An app provider's write returned a promise that settled once the page's work queued for this
    /// instant ran, which other work of the instant ran with; virtual time did not move.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub served_instant: bool,
    /// Virtual time the write's promise was given to settle, from the call's progress grant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progressed_us: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedState {
    pub boundary: ObservationBoundary,
    pub completed: bool,
    pub presentation: crate::observation::PresentationRelation,
    /// Ordered attempted prefix. An errored setter may already have caused effects; no rollback.
    pub receipts: Vec<ChangeReceipt>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterventionRecord {
    pub request_id: RequestId,
    pub changes: Vec<StateChange>,
    pub receipts: Vec<ChangeReceipt>,
    pub completed: bool,
}
