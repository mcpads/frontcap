use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid managed identifier {0:?}: use lowercase ASCII letters and digits joined by single hyphens")]
pub struct InvalidId(pub String);

pub fn validate(s: &str) -> Result<(), InvalidId> {
    let ok = !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('-')
        && !s.ends_with('-')
        && !s.contains("--")
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok { Ok(()) } else { Err(InvalidId(s.to_string())) }
}

macro_rules! managed_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Result<Self, InvalidId> {
                let s = s.into();
                validate(&s)?;
                Ok(Self(s))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        #[cfg(feature = "schema")]
        impl schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({ "type": "string", "pattern": "^[a-z0-9]+(-[a-z0-9]+)*$", "maxLength": 128 })
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidId;
            fn try_from(s: String) -> Result<Self, InvalidId> {
                Self::new(s)
            }
        }

        impl From<$name> for String {
            fn from(v: $name) -> String {
                v.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

managed_id!(
    /// Identity of one generation.
    LaunchId
);
managed_id!(
    /// Identity of one evidence bundle.
    RunId
);
managed_id!(
    /// Client-chosen identity of one state-changing request, scoped to a generation.
    RequestId
);
managed_id!(
    /// Identity of one network rule.
    RuleId
);
managed_id!(
    /// Name of one registered watch.
    WatchName
);
managed_id!(
    /// Content-derived identity of one saved starting point (`b-` and the leading digits of its digest).
    SavedId
);

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 16 lowercase hex digits derived from the wall clock, the process id, and a process-wide counter.
/// Uniqueness, not secrecy, is the requirement.
fn unique_hex() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut h = Sha256::new();
    h.update(nanos.to_le_bytes());
    h.update(std::process::id().to_le_bytes());
    h.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect()
}

impl LaunchId {
    pub fn generate() -> LaunchId {
        LaunchId(format!("g-{}", unique_hex()))
    }
}

impl RequestId {
    /// Ids the control server makes for itself start with this; agents' ids may not.
    pub const SERVER_PREFIX: &'static str = "mcp-";

    /// A request id for a call that carried none.
    pub fn generate() -> RequestId {
        RequestId(format!("{}{}", Self::SERVER_PREFIX, unique_hex()))
    }

    /// The id of one part of a composite call: `mcp-<role>-<id>`.
    pub fn derived(&self, role: &str) -> RequestId {
        let base = self.0.strip_prefix(Self::SERVER_PREFIX).unwrap_or(&self.0);
        let mut s = format!("{}{role}-{base}", Self::SERVER_PREFIX);
        s.truncate(128);
        RequestId::new(s.trim_end_matches('-').to_string()).expect("built from valid parts")
    }
}

#[cfg(test)]
#[path = "ids_tests.rs"]
mod tests;
