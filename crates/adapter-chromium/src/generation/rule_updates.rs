//! Future network decisions change only after an installation is durably published.
use super::*;
use frontcap_core::{observation::ObservationBoundary, rules::RuleReceipt};

impl Generation {
    pub fn rule_receipt(&self, request: &RequestId) -> Result<Option<RuleReceipt>, ChromiumError> {
        self.read_trace(|events| events.iter().find_map(|event| match &event.kind {
            EventKind::RulesInstalled(installed) if installed.request_id.as_ref() == Some(request) =>
                Some(RuleReceipt { revision: installed.revision, sha256: installed.sha256.clone() }),
            _ => None,
        }))
    }

    /// Expand an ordered caller edit inside the recorder's serialized mutation boundary.
    pub async fn edit_network_rules(&mut self, request: &RequestId, expected_revision: u64,
        boundary: &ObservationBoundary, edit: &frontcap_core::rules::RuleEdit, origins: &[String]) -> Result<RuleReceipt, ChromiumError> {
        self.alive()?;
        if let Some(receipt) = self.rule_receipt(request)? { return Ok(receipt); }
        if self.net.rules_revision != expected_revision {
            return Err(ChromiumError::ObservationChanged(format!("rules revision {expected_revision} → {}", self.net.rules_revision)));
        }
        self.expect_boundary(boundary).await?;
        let count = self.net.rules.rules.len().checked_sub(origins.len() + 1)
            .ok_or_else(|| ChromiumError::Shape("installed rules lack launch defaults".into()))?;
        let mut caller = RuleSet { rules: self.net.rules.rules[..count].to_vec() };
        // Existing replay rules name immutable bundle copies, not recording-host paths.
        for rule in &mut caller.rules {
            if let Action::Replay { cassette, .. } = &mut rule.action {
                *cassette = self.bundle.root().join(&*cassette).display().to_string();
            }
        }
        match edit {
            frontcap_core::rules::RuleEdit::Add { rule, before } => {
                let index = match before {
                    Some(id) => caller.rules.iter().position(|rule| &rule.id == id)
                        .ok_or_else(|| ChromiumError::InvalidArgument(format!("caller rule {id} does not exist")))?,
                    None => caller.rules.len(),
                };
                caller.rules.insert(index, rule.as_ref().clone());
            }
            frontcap_core::rules::RuleEdit::Remove { id } => {
                let index = caller.rules.iter().position(|rule| &rule.id == id)
                    .ok_or_else(|| ChromiumError::InvalidArgument(format!("caller rule {id} does not exist")))?;
                caller.rules.remove(index);
            }
        }
        self.replace_network_rules(request, expected_revision, boundary, caller, origins).await
    }

    /// Replace caller rules while preserving the explicit launch defaults supplied by the recorder.
    /// Existing held requests and scheduled deliveries retain their selected decisions.
    pub async fn replace_network_rules(&mut self, request: &RequestId, expected_revision: u64,
        boundary: &ObservationBoundary, mut caller: RuleSet, origins: &[String]) -> Result<RuleReceipt, ChromiumError> {
        self.alive()?;
        if let Some(receipt) = self.rule_receipt(request)? { return Ok(receipt); }
        if self.net.rules_revision != expected_revision {
            return Err(ChromiumError::ObservationChanged(format!("rules revision {expected_revision} → {}", self.net.rules_revision)));
        }
        self.expect_boundary(boundary).await?;
        caller.validate()?;
        frontcap_core::rules::validate_passthrough_origins(origins)?;
        let revision = expected_revision.checked_add(1).ok_or_else(|| ChromiumError::InvalidArgument("rule installation counter exhausted".into()))?;
        let cassettes = lifecycle::keep_cassettes(&mut caller, self.bundle.root())?;
        // A later boundary/horizon refusal must replay from the assets already prepared here.
        // Key attempts by request identity: another retry at the same revision must not overwrite it.
        let attempts = self.bundle.root().join("inputs/rule-attempts");
        std::fs::create_dir_all(&attempts)?;
        let attempt = frontcap_core::canonical::sha256_hex(request.as_str());
        frontcap_core::state::save_atomic(&attempts.join(format!("{attempt}.json")), &caller)?;
        let rules = RuleSet::effective(caller.clone(), origins)?;
        // Drain requests announced during input preparation under the old rules and reject stale
        // observations. An unresolved ordering horizon must not defer the publication behind input.
        self.expect_boundary(boundary).await?;
        // Record-to-Replay conversion moves an unresolved stopped response from passthrough
        // inventory to the agenda. Its progress boundary still forbids publishing a replacement;
        // ordinary future mocks and caller-held requests retain their existing installation.
        let stopped_response = self.net.agenda.iter().any(|(at, event)| at <= self.now
            && matches!(event, Due::Response(delivery) if delivery.requires_progress
                && self.net.held.contains_key(&delivery.ordinal)));
        if !self.pending.is_empty() || self.net.horizon().is_some_and(|at| at <= self.now) || stopped_response {
            return Err(ChromiumError::NotActionable("resolve pending network evidence before installing rules".into()));
        }
        let inputs = self.bundle.root().join("inputs/rules");
        std::fs::create_dir_all(&inputs)?;
        frontcap_core::state::save_atomic(&inputs.join(format!("{revision:04}.json")), &caller)?;
        frontcap_core::state::save_atomic(&self.bundle.rules_dir().join(format!("{revision:04}.json")), &rules)?;
        rule_fault(request, "before_rules_publish");
        let receipt = RuleReceipt { revision, sha256: rules.sha256() };
        self.record(self.now, EventKind::RulesInstalled(RulesInstalledRecord {
            request_id: Some(request.clone()), revision, sha256: receipt.sha256.clone(), count: rules.rules.len() as u32,
        }));
        if let Err(error) = self.commit().and_then(|_| self.log.sync().map_err(ChromiumError::from)) {
            self.dead = true;
            return Err(ChromiumError::InterventionUncertain(format!("rule publication: {error}")));
        }
        rule_fault(request, "after_rules_publish");
        let previous = std::mem::replace(&mut self.net.rules, rules);
        // A rule installed unchanged keeps its consumption (sequence steps, replayed cassette lines and
        // sessions); a new or changed rule starts again. Recovery derives the same from the history.
        self.net.cursors = self.net.cursors.carried(&previous, &self.net.rules);
        let mut cassettes = cassettes;
        for (id, cassette) in cassettes.iter_mut() {
            if frontcap_core::rules::unchanged(&previous, &self.net.rules, id)
                && let Some(old) = self.net.cassettes.get(id)
            {
                cassette.served = old.served.clone();
                cassette.sessions_served = old.sessions_served.clone();
            }
        }
        self.net.previous_rules.insert(expected_revision, previous);
        self.net.rules_revision = revision;
        self.net.cassettes = cassettes;
        self.net.recorded_sessions = lifecycle::recorded_sessions(&self.net.rules, self.bundle.root(), revision);
        Ok(receipt)
    }
}

fn rule_fault(request: &RequestId, point: &str) {
    #[cfg(feature = "fault-injection")]
    if std::env::var("FRONTCAP_FAULT").ok().as_deref() == Some(&format!("{request}@{point}")) { std::process::abort(); }
    let _ = (request, point);
}
