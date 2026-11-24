//! Dialog rules (exploration-surface.md, Input and dialogs): explicit, ordered answers the platform gives
//! the page's dialogs in its place. Without a matching rule a dialog gets its dismissed answer, so nothing
//! is approved on the agent's behalf.
use crate::event::DialogKind;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DialogAnswer {
    Accept,
    Dismiss,
    Hold,
}

/// Answers dialogs of `kind` whose message matches `message` (`*` wildcard; either absent: any). An
/// accepted prompt gets `text` or its default; `hold` suspends the operation for an answer.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<DialogKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub answer: DialogAnswer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The rules in force, first match wins.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DialogRules(pub Vec<DialogRule>);

/// What the page receives from a dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogReply {
    Alert,
    Confirm(bool),
    Prompt(Option<String>),
    /// Whether the page is left.
    Beforeunload(bool),
}

impl DialogReply {
    /// As the trace's dialog record states it.
    pub fn recorded(&self) -> String {
        match self {
            DialogReply::Alert => "returned".into(),
            DialogReply::Confirm(accepted) => accepted.to_string(),
            DialogReply::Prompt(Some(text)) => serde_json::to_string(text).expect("a string serializes"),
            DialogReply::Prompt(None) => "null".into(),
            DialogReply::Beforeunload(true) => "accepted".into(),
            DialogReply::Beforeunload(false) => "dismissed".into(),
        }
    }
}

impl DialogRules {
    /// Retain every opening while an explicit dialog wait owns the progress grant.
    pub fn hold_all() -> Self {
        Self(vec![DialogRule { kind: None, message: None, answer: DialogAnswer::Hold, text: None }])
    }

    pub fn rule_for(&self, kind: DialogKind, message: &str) -> Option<&DialogRule> {
        self.0.iter().find(|r| r.kind.is_none_or(|k| k == kind) && r.message.as_deref().is_none_or(|p| crate::rules::glob(p.as_bytes(), message.as_bytes())))
    }

    /// The reply to a dialog of `kind` showing `message` (`default` is a prompt's default value). Without a
    /// rule: an alert returns, a confirm is false, a prompt null, and a page is left (`beforeunload`).
    /// `None` leaves the native dialog pending for a dependent answer.
    pub fn answer(&self, kind: DialogKind, message: &str, default: Option<&str>) -> Option<DialogReply> {
        if self.rule_for(kind, message).is_some_and(|r| r.answer == DialogAnswer::Hold) { return None }
        let accept = self.rule_for(kind, message).map(|r| (r.answer == DialogAnswer::Accept, r.text.clone()));
        Some(match kind {
            DialogKind::Alert => DialogReply::Alert,
            DialogKind::Confirm => DialogReply::Confirm(matches!(accept, Some((true, _)))),
            DialogKind::Prompt => match accept {
                Some((true, text)) => DialogReply::Prompt(Some(text.unwrap_or_else(|| default.unwrap_or_default().to_string()))),
                _ => DialogReply::Prompt(None),
            },
            DialogKind::Beforeunload => DialogReply::Beforeunload(accept.is_none_or(|(a, _)| a)),
        })
    }
}

/// Positional source captured by the browser at dialog opening. Child indices run from the root
/// inward; URLs include the root and every ancestor through the owner. Unlike inspector frame tokens,
/// this address can be checked in a fresh generation, including for cross-origin documents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogSource {
    pub frame_path: Vec<u32>,
    pub frame_urls: Vec<String>,
}

/// A native dialog at its opening boundary. `id` is the generation's logical dialog ordinal;
/// `frame` is an opaque owner identity; `source` is the address to check during replay; `context` names
/// the popup that raised it (none for the launch page).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogDescriptor {
    pub id: String,
    pub kind: DialogKind,
    pub message: String,
    pub default_prompt: String,
    pub url: String,
    pub frame: String,
    pub source: DialogSource,
    pub t_us: crate::time::VirtualTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialogReceipt {
    pub dialog: DialogDescriptor,
    pub request_id: crate::ids::RequestId,
    pub source: crate::event::NativeDialogSource,
    pub accept: bool,
    pub prompt_text: String,
}

#[cfg(test)]
#[path = "dialog_tests.rs"]
mod tests;
