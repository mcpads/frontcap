//! Generic XCTest catalogs. References are scoped to one observation, never app business names.
use super::request::{InvalidRequest, canonical_uuid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Table,
    CollectionView,
    ScrollView,
    Button,
    StaticText,
    TextField,
    SecureTextField,
    TextView,
    Cell,
    Link,
    Switch,
}
impl Role {
    fn element_type(self) -> u32 {
        match self {
            Self::Table => 26,
            Self::CollectionView => 32,
            Self::ScrollView => 46,
            Self::Button => 9,
            Self::StaticText => 48,
            Self::TextField => 49,
            Self::SecureTextField => 50,
            Self::TextView => 52,
            Self::Cell => 75,
            Self::Link => 42,
            Self::Switch => 40,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectorKey {
    Identifier,
    Label,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Text(String),
    Boolean(bool),
    Number(serde_json::Number),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub role: Role,
    #[serde(rename = "type")]
    pub element_type: u32,
    pub identifier: String,
    pub label: String,
    pub value: Option<Value>,
    pub selector_key: SelectorKey,
    pub selector_value: String,
    pub unique: bool,
    pub enabled: bool,
    pub selected: bool,
    pub hittable: bool,
    pub intersects_screen: bool,
    pub frame_mpt: [i64; 4],
    pub reference: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub observation: String,
    pub keyboard: bool,
    pub transitioning: bool,
    pub targets: Vec<Target>,
    pub scrolls: Vec<super::ios_scroll::Scroll>,
    pub text_input: super::ios_text::TextInput,
}
fn bounded(value: &str, units: usize) -> bool {
    value.encode_utf16().count() <= units
}
pub fn valid_reference(reference: &str, observation: &str) -> bool {
    if !canonical_uuid(observation) {
        return false;
    }
    reference
        .strip_prefix(observation)
        .and_then(|tail| tail.strip_prefix('-'))
        .and_then(|index| index.parse::<usize>().ok().map(|n| (index, n)))
        .is_some_and(|(index, n)| n < 2048 && n.to_string() == index)
}
impl Catalog {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if !canonical_uuid(&self.observation) || self.targets.len() > 2048 {
            return Err(InvalidRequest("invalid UIKit catalog identity or size"));
        }
        self.text_input.validate()?;
        if self.scrolls.len()>2048 { return Err(InvalidRequest("scroll catalog over budget")); }
        let mut tokens=std::collections::BTreeSet::new();
        for scroll in &self.scrolls { scroll.validate()?;
            if !tokens.insert(&scroll.token) { return Err(InvalidRequest("duplicate scroll token")); }
        }
        for (index, target) in self.targets.iter().enumerate() {
            let key = if target.identifier.is_empty() {
                SelectorKey::Label
            } else {
                SelectorKey::Identifier
            };
            let selected = match key {
                SelectorKey::Identifier => &target.identifier,
                SelectorKey::Label => &target.label,
            };
            let matches = self
                .targets
                .iter()
                .filter(|other| {
                    other.element_type == target.element_type
                        && match key {
                            SelectorKey::Identifier => &other.identifier,
                            SelectorKey::Label => &other.label,
                        } == selected
                })
                .count();
            let unique = !selected.is_empty() && matches == 1;
            let [x, y, width, height] = target.frame_mpt;
            if target.element_type != target.role.element_type()
                || target.reference != format!("{}-{index}", self.observation)
                || !bounded(&target.identifier, 4096)
                || !bounded(&target.label, 4096)
                || matches!(&target.value, Some(Value::Text(text)) if !bounded(text, 16384))
                || target.selector_key != key
                || &target.selector_value != selected
                || target.unique != unique
                || (target.role == Role::StaticText && target.hittable)
                || [x, y, width, height]
                    .iter()
                    .any(|n| !(-(i32::MAX as i64)..=i32::MAX as i64).contains(n))
                || width < 0
                || height < 0
                || (target.hittable
                    && (!unique
                        || !target.enabled
                        || !target.intersects_screen
                        || width == 0
                        || height == 0))
                || (target.intersects_screen && (width == 0 || height == 0))
            {
                return Err(InvalidRequest("inconsistent UIKit observed target"));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotAppliedReason {
    StaleObservation,
    UnknownTarget,
    UnsupportedTarget,
    AmbiguousTarget,
    DisabledTarget,
    UnhittableTarget,
    ObservationChanged,
    TransitionInProgress,
    InvalidPressContext,
    InputUnavailable,
    ScrollUnavailable,
}

#[cfg(test)]
#[path = "ios_targets_tests.rs"]
mod tests;
