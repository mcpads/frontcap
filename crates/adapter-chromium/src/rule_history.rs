//! Recover only published rule installations; unused input files do not change active behavior.
use crate::ChromiumError;
use frontcap_core::{event::{BehaviorEvent, EventKind}, rules::RuleSet};
use std::{collections::BTreeMap, path::Path};

pub(crate) struct RuleHistory {
    pub revision: u64,
    pub active: RuleSet,
    pub previous: BTreeMap<u64, RuleSet>,
}

pub(crate) fn load(dir: &Path, events: &[BehaviorEvent]) -> Result<RuleHistory, ChromiumError> {
    let mut sets = BTreeMap::new();
    for event in events {
        if let EventKind::Request(request) = &event.kind
            && !sets.contains_key(&request.rules_revision) {
            return Err(ChromiumError::Shape(format!("{} selected an unpublished rule installation {}", request.id, request.rules_revision)));
        }
        let EventKind::RulesInstalled(installation) = &event.kind else { continue };
        let revision = installation.revision;
        if revision != sets.len() as u64 {
            return Err(ChromiumError::Shape(format!("rule installation {revision} is out of order")));
        }
        let rules = RuleSet::from_json(&std::fs::read_to_string(dir.join(format!("{revision:04}.json")))?)?;
        if rules.sha256() != installation.sha256 || rules.rules.len() as u64 != u64::from(installation.count) {
            return Err(ChromiumError::Shape(format!("rule installation {revision} does not match its recorded input")));
        }
        sets.insert(revision, rules);
    }
    // A launch interrupted before its initial trace commit may still own its initial input. Later
    // files are never adopted without publication: a prepared replacement is not an installation.
    if sets.is_empty() {
        sets.insert(0, RuleSet::from_json(&std::fs::read_to_string(dir.join("0000.json"))?)?);
    }
    let (revision, active) = sets.pop_last().expect("initial rule set exists");
    Ok(RuleHistory { revision, active, previous: sets })
}

#[cfg(test)]
#[path = "rule_history_tests.rs"]
mod tests;
