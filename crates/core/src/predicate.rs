pub use crate::event::{PointKind, SampleValue};
use crate::ids::WatchName;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "by", rename_all = "snake_case", deny_unknown_fields)]
pub enum Locator {
    Role {
        role: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// A leaf element whose trimmed text is exactly `text`, or with `contains: true` includes it.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        contains: bool,
    },
    /// `[data-testid=id]`.
    TestId { id: String },
    Css { selector: String },
    /// A reference an observation returned (`e<n>-<6 hex>`), valid while its document is current; no frame scope.
    Ref { id: String },
    /// `target` within the frames `frame` names; other locators match in every frame.
    Frame { frame: Box<Locator>, target: Box<Locator> },
    /// The top document's root element, or a frame scope's. With `selector`, search only that
    /// document (including its shadow roots), excluding child frame documents.
    Document {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        selector: Option<String>,
    },
}

/// A condition on the page for `wait until`, watches and recipes; `unknown` when it cannot be evaluated
/// or its target matches several nodes.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "is", rename_all = "snake_case", deny_unknown_fields)]
pub enum Predicate {
    Attached { target: Locator },
    Visible { target: Locator },
    /// Effective opacity (product over ancestors) of at least `milli / 1000`.
    OpacityAtLeast { target: Locator, milli: u16 },
    TextEquals { target: Locator, text: String },
    /// A form control's current value (input, textarea, select) equals `value`.
    ValueEquals { target: Locator, value: String },
    CountAtLeast { target: Locator, n: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PredicateError {
    #[error("opacity threshold {0} is outside 0..=1000")]
    OpacityOutOfRange(u16),
    #[error("locator is empty")]
    EmptySelector,
    #[error("{0:?} is not a node reference (e<n>-<6 hex>)")]
    BadRef(String),
    #[error("watch {0} is already registered")]
    Duplicate(WatchName),
    #[error("watch {0} is not registered")]
    NotFound(WatchName),
}

impl Locator {
    fn validate(&self) -> Result<(), PredicateError> {
        let text = match self {
            Locator::Frame { frame, target } => {
                frame.validate()?;
                return target.validate();
            }
            Locator::Role { role, .. } => role,
            Locator::Text { text, .. } => text,
            Locator::TestId { id } => id,
            Locator::Css { selector } => selector,
            Locator::Document { selector: Some(selector) } => selector,
            Locator::Document { selector: None } => return Ok(()),
            Locator::Ref { id } => {
                // `e<n>`, and `-<6 hex>` naming the generation that issued it (older recordings have none).
                let (number, tag) = id.split_once('-').map_or((id.as_str(), None), |(n, t)| (n, Some(t)));
                let digits = number.strip_prefix('e').unwrap_or_default();
                let tagged = tag.is_none_or(|t| t.len() == 6 && t.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
                return if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) && tagged { Ok(()) } else { Err(PredicateError::BadRef(id.clone())) };
            }
        };
        if text.trim().is_empty() { Err(PredicateError::EmptySelector) } else { Ok(()) }
    }

    /// The node references it uses, frame scopes included.
    pub fn refs(&self) -> Vec<&str> {
        match self {
            Locator::Ref { id } => vec![id.as_str()],
            Locator::Frame { frame, target } => [frame.refs(), target.refs()].concat(),
            _ => Vec::new(),
        }
    }

    /// Whether matching needs native accessibility information, including within frame scopes.
    pub fn has_role(&self) -> bool {
        match self {
            Locator::Role { .. } => true,
            Locator::Frame { frame, target } => frame.has_role() || target.has_role(),
            _ => false,
        }
    }
}

impl Predicate {
    pub fn target(&self) -> &Locator {
        match self {
            Predicate::Attached { target }
            | Predicate::Visible { target }
            | Predicate::OpacityAtLeast { target, .. }
            | Predicate::TextEquals { target, .. }
            | Predicate::ValueEquals { target, .. }
            | Predicate::CountAtLeast { target, .. } => target,
        }
    }

    pub fn validate(&self) -> Result<(), PredicateError> {
        self.target().validate()?;
        if let Predicate::OpacityAtLeast { milli, .. } = self
            && *milli > 1000
        {
            return Err(PredicateError::OpacityOutOfRange(*milli));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WatchRegistry {
    watches: BTreeMap<WatchName, Registered>,
}

/// A watch as kept: a launch page's is its predicate alone, as before popups could be watched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum Registered {
    Page(Predicate),
    Popup { predicate: Predicate, context: String },
}

impl Registered {
    fn predicate(&self) -> &Predicate {
        match self {
            Registered::Page(predicate) | Registered::Popup { predicate, .. } => predicate,
        }
    }
}

impl WatchRegistry {
    pub fn add(&mut self, name: WatchName, predicate: Predicate) -> Result<(), PredicateError> {
        self.add_in(name, predicate, None)
    }

    /// A watch sampled in `context` (a popup), or in the launch page without one.
    pub fn add_in(&mut self, name: WatchName, predicate: Predicate, context: Option<String>) -> Result<(), PredicateError> {
        predicate.validate()?;
        if self.watches.contains_key(&name) {
            return Err(PredicateError::Duplicate(name));
        }
        let registered = match context {
            Some(context) => Registered::Popup { predicate, context },
            None => Registered::Page(predicate),
        };
        self.watches.insert(name, registered);
        Ok(())
    }

    pub fn remove(&mut self, name: &WatchName) -> Result<Predicate, PredicateError> {
        self.watches.remove(name).map(|r| r.predicate().clone()).ok_or_else(|| PredicateError::NotFound(name.clone()))
    }

    pub fn get(&self, name: &WatchName) -> Option<&Predicate> {
        self.watches.get(name).map(Registered::predicate)
    }

    /// The popup a watch samples in; none for the launch page's.
    pub fn context(&self, name: &WatchName) -> Option<&str> {
        match self.watches.get(name)? {
            Registered::Popup { context, .. } => Some(context),
            Registered::Page(_) => None,
        }
    }

    /// Whether any watch samples in `context` (the launch page for none).
    pub fn any_in(&self, context: Option<&str>) -> bool {
        self.watches.keys().any(|name| self.context(name) == context)
    }

    pub fn list(&self) -> Vec<(&WatchName, &Predicate)> {
        self.watches.iter().map(|(name, r)| (name, r.predicate())).collect()
    }
}

#[cfg(test)]
#[path = "predicate_tests.rs"]
mod tests;
