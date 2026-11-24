//! Source-owned UIKit first-responder state, distinct from XCTest focus attributes.
use super::request::{InvalidRequest, canonical_uuid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextRole {
    TextField,
    TextView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedReason {
    InputType,
    SecureText,
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    WrongThread,
    ViewBudget,
    TextBudget,
    SelectorBudget,
    SelectionMissing,
    SelectionInvalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum TextInput {
    Unfocused {},
    Unsupported {
        reason: UnsupportedReason,
    },
    Unavailable {
        reason: UnavailableReason,
    },
    Focused {
        token: String,
        role: TextRole,
        identifier: String,
        label: String,
        text: String,
        start: usize,
        end: usize,
        marked: bool,
    },
}
impl TextInput {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if let Self::Focused {
            token,
            identifier,
            label,
            text,
            start,
            end,
            ..
        } = self
            && (!canonical_uuid(token)
                || identifier.encode_utf16().count() > 4096
                || label.encode_utf16().count() > 4096
                || text.encode_utf16().count() > 16384
                || start > end
                || *end > text.encode_utf16().count())
            {
                return Err(InvalidRequest("invalid UIKit first-responder state"));
            }
        Ok(())
    }
}

#[cfg(test)]
#[path = "ios_text_tests.rs"]
mod tests;
