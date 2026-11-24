//! Adopting stopped network work on reconnect (exploration-surface.md, Adopting stopped network work).
//!
//! The engine keeps every paused request across controller loss (engine patch 22) and publishes them
//! again, in their order, when the new session enables interception. Those are an inventory of work the
//! dead recorder may already have decided, not new requests: each one bound in `interceptions.jsonl` is
//! restored with its ordinal, decision and absolute deadline, before anything progresses.
use super::*;
use crate::bindings::{Binding, BindingEntry, BindingJournal, Decision, Resolution};

/// Drain the replay notifications currently delivered. Bound requests missing from this queue
/// are checked against target receipts and awaited during adoption before treating them as lost.
pub(super) fn inventory(subs: &mut Subs) -> Result<Vec<fetch::RequestPausedEvent>, ChromiumError> {
    let mut found = Vec::new();
    while let Some(ev) = subs.paused.try_next()? {
        found.push(ev);
    }
    Ok(found)
}

/// The generation's live targets, each with its context (`None`: the launch page).
pub(super) type Targets = BTreeMap<String, Option<String>>;

/// The bindings of the `targets`, by native pause id. Bindings of another target belong to a page the
/// engine no longer has. Two different bindings of one pause contradict each other: refused.
fn bound_by_pause<'a>(entries: &'a [BindingEntry], targets: &Targets) -> Result<BTreeMap<&'a str, &'a Binding>, ChromiumError> {
    let mut bound: BTreeMap<&str, &Binding> = BTreeMap::new();
    for e in entries {
        if let BindingEntry::Bound(b) = e
            && targets.contains_key(&b.target_id)
            && let Some(earlier) = bound.insert(b.fetch_id.as_str(), b.as_ref())
            && earlier != b.as_ref()
        {
            return Err(ChromiumError::Shape(format!("native pause {} is bound twice ({} and {})", b.fetch_id, earlier.ordinal, b.ordinal)));
        }
    }
    Ok(bound)
}

/// The request record of each bound request the dead recorder had not written yet (still paused, or
/// answered before its records were written), at its original issue time and in ordinal order. They
/// count for trace-derived counters (ordinals, sequence steps, cassette lines) and are recorded with the
/// recovery's other events, so they never overtake reports the dead recorder had not written. A record
/// that would land before what the trace already holds contradicts the binding's guarantee.
pub(super) fn missing_request_records(events: &[BehaviorEvent], entries: &[BindingEntry], targets: &Targets) -> Result<Vec<(VirtualTime, RequestRecord)>, ChromiumError> {
    let recorded: std::collections::BTreeSet<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Request(r) => Some(r.id.as_str()),
            _ => None,
        })
        .collect();
    let last = events.last().map_or(VirtualTime::ZERO, |e| e.t_us);
    let mut missing: Vec<&Binding> = bound_by_pause(entries, targets)?.into_values().filter(|b| !recorded.contains(b.ordinal.as_str())).collect();
    missing.sort_by_key(|b| (b.issued_us, ordinal_number(&b.ordinal)));
    let mut recovered: Vec<_> = missing
        .into_iter()
        .map(|b| {
            if VirtualTime(b.issued_us) < last {
                return Err(ChromiumError::Shape(format!("{} is bound at {} us but the trace already holds events up to {} us", b.ordinal, b.issued_us, last.0)));
            }
            Ok((VirtualTime(b.issued_us), RequestRecord { rules_revision: b.rules_revision, id: b.ordinal.clone(), method: b.method.clone(), url: b.url.clone(), rule: Some(b.rule.clone()), entry: b.entry, context: targets.get(&b.target_id).cloned().flatten(), body: b.body.clone() }))
        })
        .collect::<Result<_, ChromiumError>>()?;
    for entry in entries {
        let (target, requested_us, request) = match entry {
            BindingEntry::StreamOpening { target_id, requested_us, request, .. } => (target_id, *requested_us, request.as_ref()),
            BindingEntry::StreamClosed(closure) => (&closure.target_id, closure.handshake.requested_us, &closure.handshake.request),
            BindingEntry::StreamAbandoned { target_id, handshake } => (target_id, handshake.requested_us, &handshake.request),
            BindingEntry::StreamHeld { target_id, requested_us, request, .. } => (target_id, *requested_us, request.as_ref()),
            _ => continue,
        };
        // The launch page's streams and its windows', each request naming its window.
        if let Some(window) = targets.get(target) && !recorded.contains(request.id.as_str())
            && !recovered.iter().any(|(_, saved)| saved.id == request.id) {
            if VirtualTime(requested_us) < last {
                return Err(ChromiumError::Shape(format!("{} stream request precedes durable trace", request.id)));
            }
            let mut request = request.clone();
            request.context = request.context.or_else(|| window.clone());
            recovered.push((VirtualTime(requested_us), request));
        }
    }
    recovered.sort_by_key(|(at, request)| (*at, ordinal_number(&request.id)));
    Ok(recovered)
}

pub(super) fn ordinal_number(ordinal: &str) -> u64 {
    ordinal.strip_prefix("req-").and_then(|n| n.parse().ok()).unwrap_or(u64::MAX)
}

/// An engine issue stamp (decimal microseconds since the Unix epoch).
fn stamp_of(ev: &fetch::RequestPausedEvent) -> Option<u64> {
    ev.frontcap_issue_time_us.as_deref().and_then(|s| s.parse().ok())
}

impl Generation {
    /// Restores each retained, bound request as its binding decided, without progressing the page or
    /// deciding anything again; unbound pauses are queued for ordinary handling. Returns the ordinals
    /// adopted. Runs before any clock grant of the new connection.
    pub(super) async fn adopt_interceptions(
        &mut self,
        entries: &[BindingEntry],
        inventory: Vec<fetch::RequestPausedEvent>,
        events: &[BehaviorEvent],
        blocking_answer_time: &BTreeMap<String, VirtualTime>,
    ) -> Result<Vec<String>, ChromiumError> {
        self.net.worker_request_owners = worker_lineage::recovered_owners(entries)?;
        self.net.worker_request_issues = worker_lineage::recovered_issues(entries)?;
        self.net.worker_request_places = worker_lineage::recovered_places(entries)?;
        // A frozen owner was recorded from a Network announcement even when
        // its Fetch pause had not yet arrived at the previous controller.
        self.net.recovered_network_requests.extend(self.net.worker_request_owners.keys().cloned());
        self.refresh_worker_lineages().await?;
        let targets = self.known_targets();
        self.released_interceptions.extend(entries.iter().filter_map(|e| match e {
            BindingEntry::InterceptionReleased { target_id, fetch_id } if targets.contains_key(target_id) => Some(fetch_id.clone()),
            _ => None,
        }));
        let answered: std::collections::BTreeSet<&str> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Response(r) => Some(r.id.as_str()),
                _ => None,
            })
            .collect();
        let bound = bound_by_pause(entries, &targets)?;
        self.net.recovered_network_requests.extend(bound.values().filter_map(|b| b.network_id.clone()));
        // Every resolution the dead recorder was about to send, per native pause: a retry follows an
        // ambiguous first attempt, and either may be the one the engine accepted.
        let mut dispatched: BTreeMap<&str, Vec<&Resolution>> = BTreeMap::new();
        for e in entries {
            if let BindingEntry::Dispatched { fetch_id, resolution, .. } = e {
                dispatched.entry(fetch_id.as_str()).or_default().push(resolution);
            }
        }
        let mut inventory = inventory;
        if self.patched {
            // Fetch.enable can acknowledge before a delayed replay notification reaches this
            // connection. Releasing a temporary recovery consumer must not turn that delay into
            // evidence that a bound request disappeared. Every binding needs a pause or receipt.
            let expected = std::mem::take(&mut self.subs.recovery_inventory);
            let deadline = Instant::now() + Session::CALL_TIMEOUT;
            loop {
                let receipts = self.interception_receipts().await?;
                let complete = bound.values().filter(|b| !answered.contains(b.ordinal.as_str())).all(|b|
                    inventory.iter().any(|ev| ev.request_id.0 == b.fetch_id)
                    || receipts.get(&b.fetch_id).is_some_and(|(target, _)| target == &b.target_id))
                    && expected.iter().all(|(target, id)| inventory.iter().any(|ev| ev.request_id.0 == *id)
                        || receipts.get(id).is_some_and(|(owner, _)| owner == target));
                if complete { break; }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    let mut missing: Vec<_> = bound.values().filter(|b| !answered.contains(b.ordinal.as_str())
                        && !inventory.iter().any(|ev| ev.request_id.0 == b.fetch_id)
                        && !receipts.get(&b.fetch_id).is_some_and(|(target, _)| target == &b.target_id))
                        .map(|b| b.ordinal.as_str()).collect();
                    missing.extend(expected.iter().filter(|(target, id)| !inventory.iter().any(|ev| ev.request_id.0 == *id)
                        && !receipts.get(id).is_some_and(|(owner, _)| owner == target)).map(|(_, id)| id.as_str()));
                    return Err(ChromiumError::Shape(format!("{}: no native evidence of a retained pause or receipt after the replay deadline", missing.join(", "))));
                }
                if let Some(event) = self.subs.paused.next_timeout(left.min(Duration::from_millis(50))).await? {
                    inventory.push(event);
                }
            }
        }
        let retained: std::collections::BTreeSet<String> = inventory.iter().map(|ev| ev.request_id.0.clone()).collect();
        self.adopting = true;
        let mut adopted = Vec::new();
        let mut unbound = Vec::new();
        for ev in inventory {
            if let Some(handshake) = entries.iter().find_map(|entry| match entry {
                BindingEntry::StreamAbandoned { target_id, handshake } if targets.contains_key(target_id)
                    && handshake.anchors(&ev) => Some(handshake), _ => None,
            }) {
                self.fail_anchor(ev.request_id).await?;
                adopted.push(handshake.request.id.clone());
                continue;
            }
            if let Some(closure) = entries.iter().find_map(|entry| match entry {
                BindingEntry::StreamClosed(closure) if targets.contains_key(&closure.target_id)
                    && closure.handshake.anchors(&ev) => Some(closure), _ => None,
            }) {
                self.fail_anchor(ev.request_id).await?;
                adopted.push(closure.handshake.request.id.clone());
                continue;
            }
            if let Some(selected) = self.net.stream_selections.get(&ev.request_id.0)
                && answered.contains(selected.ordinal.as_str()) {
                // Opening/response recovery already restored this decision. Only release the
                // native anchor; reconnecting or emitting another handshake would contradict it.
                let ordinal = selected.ordinal.clone();
                self.fail_anchor(ev.request_id).await?;
                adopted.push(ordinal);
                continue;
            }
            if super::interception::is_response_stage(&ev) {
                // Continued by the dead recorder to record its response: the recording is lost with it.
                if let Some(b) = bound.get(ev.request_id.0.as_str()) {
                    self.net.response_stage.insert(ev.request_id.0.clone(), b.ordinal.clone());
                    self.recording_of(b, &ev);
                }
                self.finish_response_stage(ev).await?;
                continue;
            }
            let Some(b) = bound.get(ev.request_id.0.as_str()).copied() else {
                unbound.push(ev);
                continue;
            };
            if answered.contains(b.ordinal.as_str()) {
                return Err(ChromiumError::Shape(format!("{} has a recorded response but its request is still paused", b.ordinal)));
            }
            self.restore_bound(ev, b).await?;
            adopted.push(b.ordinal.clone());
        }
        // Bound requests no longer paused and never recorded as answered: the engine's receipt says how
        // each ended. Without receipts (the baseline engine) they stay unanswered, as before adoption.
        if self.patched {
            let receipts = self.interception_receipts().await?;
            for b in bound.values().filter(|b| !retained.contains(&b.fetch_id) && !answered.contains(b.ordinal.as_str())) {
                if self.reconcile_gone(b, receipts.get(&b.fetch_id).filter(|(target, _)| target == &b.target_id).map(|(_, receipt)| receipt), dispatched.get(b.fetch_id.as_str()).map_or(&[][..], Vec::as_slice),
                    blocking_answer_time.get(&b.ordinal).copied())? {
                    adopted.push(b.ordinal.clone());
                }
            }
            // Receipts of requests the trace already answers, and of pauses never bound (stop and stream
            // anchors, which leave no answer to record), are not needed again.
            self.receipts_ready.extend(receipts.keys().filter(|id| bound.get(id.as_str()).is_none_or(|b| answered.contains(b.ordinal.as_str()))).cloned());
        }
        // Decided in the declared order, as with a recorder attached: by issue instant, then issuer (a
        // worker's from its frozen owner, or from the worker the pause names when no recorder read its
        // announcement), then the issuer's own order. The engine publishes a request once its data is
        // collected, which a large body delays; DevTools ids number a renderer's requests in the order
        // they were made. As a round, they are all allocated before any answer.
        if self.patched {
            self.freeze_paused_worker_owners(&unbound).await?;
        }
        let devtools_number = |ev: &fetch::RequestPausedEvent| ev.network_id.as_ref()
            .map_or(u64::MAX, |id| id.0.rsplit('.').next().and_then(|n| n.parse().ok()).unwrap_or(u64::MAX));
        unbound.sort_by_cached_key(|ev| (stamp_of(ev).unwrap_or(u64::MAX), self.issuer_of_paused(ev), devtools_number(ev)));
        self.request_round = Some(super::request_collection::RequestRound { through: cdp::Arrival(u64::MAX), selected: Vec::new() });
        let mut adopted_unbound = Ok(());
        for ev in unbound {
            adopted_unbound = self.adopt_unbound(ev).await;
            if adopted_unbound.is_err() { break; }
        }
        let delivered = self.deliver_round().await;
        adopted_unbound?;
        delivered?;
        // Ordinals the dead recorder handed out continue after the highest it bound.
        let bound_max = entries.iter().filter_map(|e| match e {
            BindingEntry::Bound(b) => b.ordinal.strip_prefix("req-").and_then(|n| n.parse::<u64>().ok()),
            _ => None,
        });
        self.net.next_ordinal = bound_max.fold(self.net.next_ordinal, u64::max);
        // A deadline that passed while no recorder owned the clock is answered now, as late.
        self.deliver_due().await?;
        self.adopting = false;
        self.commit()?;
        Ok(adopted)
    }

    /// A bound request the engine no longer holds. Its receipt must show the dispatched answer accepted
    /// (recorded now, once) or the request ended by the page; a continued request needs nothing more.
    /// Returns whether its outcome is now recorded. No evidence, or contradicting evidence, fails the
    /// recovery rather than inventing an outcome.
    fn reconcile_gone(&mut self, b: &Binding, receipt: Option<&fetch::FrontcapReceipt>, dispatched: &[&Resolution], blocking_answer_time: Option<VirtualTime>) -> Result<bool, ChromiumError> {
        use fetch::FrontcapReceiptOutcome as Outcome;
        let decision = self.interception_decision(b);
        let source = match decision {
            Decision::Deliver { source, .. } => *source,
            Decision::Hold | Decision::Intercept { .. } => ResponseSource::Agent,
            _ => ResponseSource::Mock,
        };
        let sent = |matches: &dyn Fn(&Resolution) -> bool| dispatched.iter().any(|r| matches(r));
        let status = match receipt.map(|r| (r.outcome, r.response_code, r.body_sha256.as_deref())) {
            Some((Outcome::Fulfilled, code, digest))
                if sent(&|r| matches!(r, Resolution::Fulfill { status, body_sha256 } if code == Some(i64::from(*status)) && digest == Some(body_sha256.as_str()))) =>
            {
                Some((code.unwrap_or_default() as u16, source))
            }
            Some((Outcome::Failed, _, _)) if sent(&|r| matches!(r, Resolution::Fail { .. })) => Some((0, ResponseSource::Fail)),
            Some((Outcome::Cancelled, _, _)) => Some((0, ResponseSource::Fail)),
            Some((Outcome::Continued, _, _)) if sent(&|r| matches!(r, Resolution::Continue)) => {
                return Ok(self.adopt_continued(b, receipt.expect("matched a receipt")));
            }
            Some(r) => {
                return Err(ChromiumError::Shape(format!("{}: the engine's receipt {r:?} matches no dispatched answer {dispatched:?}", b.ordinal)));
            }
            None => {
                return Err(ChromiumError::Shape(format!("{}: its request is no longer paused and the engine has no receipt: no native evidence of how it ended", b.ordinal)));
            }
        };
        // An accepted delayed answer was sent at its deadline; anything else ended no later than the
        // recovery found it ended, which may be before a deadline never reached.
        let blocking_answer_time = blocking_answer_time.or((b.blocks_main_thread || b.continuation.is_some()).then_some(()).and(match decision {
            Decision::Deliver { due_us, .. } => Some(VirtualTime(*due_us)),
            Decision::Fail { .. } => Some(VirtualTime(b.issued_us)),
            _ => None,
        }));
        let at = if let Some(at) = blocking_answer_time {
            let valid = match decision {
                Decision::Hold => at.0 >= b.issued_us,
                Decision::Deliver { due_us, .. } => at.0 == *due_us,
                Decision::Fail { .. } => at.0 == b.issued_us,
                _ => false,
            };
            if !valid || at > self.now || at < self.log.last_time() {
                return Err(ChromiumError::Shape(format!("{}: blocking answer instant contradicts its binding or trace", b.ordinal)));
            }
            at
        } else { match (decision, receipt.map(|r| r.outcome)) {
            (Decision::Deliver { due_us, .. }, Some(Outcome::Fulfilled)) => VirtualTime(*due_us).max(self.log.last_time()),
            _ => self.now,
        }};
        let Some((status, source)) = status else { return Ok(false) };
        self.record(at, EventKind::Response(ResponseRecord { id: b.ordinal.clone(), status, source, rule: Some(b.rule.clone()) }));
        self.receipts_awaiting.push((b.ordinal.clone(), b.fetch_id.clone()));
        Ok(true)
    }

    /// A request the dead recorder passed through. Ended while no recorder ran: its response (or failure)
    /// is recorded from the engine's receipt. Still loading: it is followed again, and its end is recorded
    /// when this session sees it, like any passed-through request.
    fn adopt_continued(&mut self, b: &Binding, receipt: &fetch::FrontcapReceipt) -> bool {
        let Some(network_id) = b.network_id.clone() else { return false };
        if receipt.completed == Some(true) {
            let ok = receipt.net_error.unwrap_or(0) == 0;
            let status = if ok { receipt.response_code.unwrap_or(0) as u16 } else { 0 };
            let source = if ok { ResponseSource::Passthrough } else { ResponseSource::Fail };
            let at = if b.blocks_main_thread || b.continuation.is_some() { VirtualTime(b.issued_us) } else { self.now };
            self.record(at, EventKind::Response(ResponseRecord { id: b.ordinal.clone(), status, source, rule: Some(b.rule.clone()) }));
            if ok {
                self.record(at, EventKind::Taint(TaintRecord { source: TaintSource::PassthroughContent, detail: b.ordinal.clone() }));
            }
            self.receipts_awaiting.push((b.ordinal.clone(), b.fetch_id.clone()));
            return true;
        }
        if let Some(code) = receipt.response_code {
            self.net.status.insert(network_id.clone(), code);
        }
        if b.document_request {
            self.net.loading_documents.insert(network_id.clone());
        }
        // Its response reaches the page when this session sees it end (`moved`), not at its issue.
        self.net.passthrough.insert(network_id, crate::network::PassedThrough { ordinal: b.ordinal.clone(), fetch_id: b.fetch_id.clone(), rule: b.rule.clone(), issued: VirtualTime(b.issued_us), moved: true });
        true
    }

    /// A retained request no recorder bound: decided now by the rules, at the instant the engine stamped
    /// it with when it was issued. A redirect's next hop is issued when the network answered the previous
    /// one, while a passed-through request held virtual time: the engine's clock now. A document the page
    /// navigated to is issued at its replayed `navigate` report and decided now, before the recovery's own
    /// records; one with no report (the dead recorder's own navigation) waits for `resume_navigation`.
    async fn adopt_unbound(&mut self, ev: fetch::RequestPausedEvent) -> Result<(), ChromiumError> {
        if ev.resource_type == cnet::ResourceType::Document {
            let url = ev.request.url.split('#').next().unwrap_or_default().to_string();
            if self.patched && let Some(i) = self.navigates.iter().rposition(|(u, _)| *u == url) {
                // The report's instant, not the recovery's: events after it are only the replayed ones.
                let (_, issued) = self.navigates.remove(i);
                self.navigates.clear();
                let t = issued.max(self.log.last_time());
                if let Some(id) = &ev.network_id {
                    self.net.interceptions.handled(&id.0);
                }
                let main = ev.frame_id == self.main_frame;
                let (documents, within) = (self.documents, self.within_document);
                let before = self.net.last_ordinal();
                match self.handle_at(ev, t).await {
                    Err(e) if is_gone(&e) => {
                        if let Some(ordinal) = self.net.last_ordinal().filter(|o| Some(o) != before.as_ref()) {
                            self.request_gone(&ordinal);
                        }
                        return Ok(());
                    }
                    r => r?,
                }
                // Its document commits before the recovery reads the page again, as a live navigation does.
                if main && !self.event_stop.as_ref().is_some_and(|s| s.event == ProgressEvent::Interception) {
                    self.await_commit(documents, within).await?;
                }
                return Ok(());
            }
            self.queued.push((Instant::now(), ev));
            return Ok(());
        }
        if !self.patched {
            self.queued.push((Instant::now(), ev));
            return Ok(());
        }
        let t = if ev.redirected_request_id.is_some() {
            self.read_time().await?.max(self.now)
        } else if let Some(us) = stamp_of(&ev) {
            VirtualTime(us.saturating_sub(self.profile.initial_date_ms.saturating_mul(1000)))
        } else if let Some(issued) = ev.network_id.as_ref().and_then(|id| self.net.worker_request_issues.get(&id.0)) {
            // Browser-owned main scripts do not carry a renderer issue stamp.
            // A preceding controller may have frozen the announcement before
            // Fetch arrived; use its observed instant, never recovery wall time.
            *issued
        } else {
            return Err(ChromiumError::Shape(format!("{} {} was paused with no recorder and carries no issue instant", ev.request.method, ev.request.url)));
        };
        // Events the dead recorder wrote after its issue are already in the trace: its record follows them.
        let t = t.max(self.log.last_time());
        if let Some(id) = &ev.network_id {
            self.net.issued.insert(id.0.clone(), t);
        }
        let before = self.net.last_ordinal();
        match self.handle_at(ev, t).await {
            Err(e) if is_gone(&e) => {
                if let Some(ordinal) = self.net.last_ordinal().filter(|o| Some(o) != before.as_ref()) {
                    self.request_gone(&ordinal);
                }
                Ok(())
            }
            r => r,
        }
    }

    /// A `record` rule keeps the response a restored request passes through, as when it was decided.
    fn recording_of(&mut self, b: &Binding, ev: &fetch::RequestPausedEvent) {
        let Some(Action::Record { cassette }) = self.net.rules_at(b.rules_revision).and_then(|rules| rules.rules.iter().find(|r| r.id == b.rule)).map(|r| r.action.clone()) else { return };
        match request_body(&ev.request) {
            Some(body) => {
                let key = frontcap_core::cassette::Key::of(&ev.request.method, &ev.request.url, body.as_deref());
                self.net.recording.insert(b.ordinal.clone(), (frontcap_core::cassette::recording_name(b.rules_revision, &cassette), key, VirtualTime(b.issued_us), !b.issued_during_progress && ev.resource_type != cnet::ResourceType::Document));
            }
            None => {
                let detail = format!("{}: the request body is not available to the platform, so it is not recorded in {cassette}", b.ordinal);
                self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::CassetteMiss, detail }));
            }
        }
    }

    /// One retained request as its binding decided. A dispatched answer whose request is still paused
    /// never took effect, so the decision is carried out again; a held request waits for the agent.
    pub(super) async fn restore_bound(&mut self, ev: fetch::RequestPausedEvent, b: &Binding) -> Result<(), ChromiumError> {
        let held = |latency_us| Held {
            fetch_id: ev.request_id.clone(),
            rule: b.rule.clone(),
            rules_revision: b.rules_revision,
            latency_us,
            released: false,
            document: b.document,
            network_id: b.network_id.clone(),
            attempted: None,
            origin: b.origin.clone(),
            method: b.method.clone(),
            url: b.url.clone(),
            body: ev.request.post_data.clone(),
            body_unavailable: ev.request.post_data.is_none() && ev.request.has_post_data == Some(true),
            blocks: false,
        };
        match self.interception_decision(b) {
            Decision::Intercept { .. } => self.retain_interception(ev, b).await?,
            Decision::Deliver { spec, source, .. } if b.document_request && ev.frame_id == self.main_frame => {
                let (documents, within) = (self.documents, self.within_document);
                self.deliver_mock(ev, b.ordinal.clone(), &b.rule, b.rules_revision, spec.clone(), *source,
                    VirtualTime(b.issued_us), true, b.origin.clone(), false).await?;
                // A resumed document replaces the world we inspected while its request was held.
                // Observe the commit before any renderer command can address that old world.
                self.await_commit(documents, within).await?;
                self.drain().await?;
            }
            Decision::Deliver { spec, source, due_us, requires_progress } => {
                self.net.held.insert(b.ordinal.clone(), held(Some(spec.latency_ms.saturating_mul(1000))));
                self.net.agenda.schedule(VirtualTime(*due_us), Due::Response(Delivery { ordinal: b.ordinal.clone(), spec: spec.clone(), source: *source, requires_progress: *requires_progress }));
                self.release_held(&b.ordinal).await;
            }
            Decision::Hold => {
                self.net.held.insert(b.ordinal.clone(), held(None));
                self.release_held(&b.ordinal).await;
            }
            Decision::Passthrough => {
                self.recording_of(b, &ev);
                let headers = paused_headers(&ev);
                let is_document = ev.resource_type == cnet::ResourceType::Document;
                let main = is_document && ev.frame_id == self.main_frame;
                let (documents, within) = (self.documents, self.within_document);
                self.pass_through(ev, b.ordinal.clone(), &b.rule, &headers, is_document, VirtualTime(b.issued_us)).await?;
                if main {
                    self.await_commit(documents, within).await?;
                    self.drain().await?;
                }
            }
            Decision::Fail { reason } => {
                self.fail(&b.ordinal, ev.request_id, &b.rule, *reason).await?;
            }
        }
        Ok(())
    }
}

/// A paused request's headers, one value per name, as rules and pass-through see them.
pub(super) fn paused_headers(ev: &fetch::RequestPausedEvent) -> BTreeMap<String, String> {
    ev.request.headers.0.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect()
}

/// The binding journal's entries for a reconnect; a line cut by the dead recorder is ignored.
pub(super) fn read_bindings(bundle: &Bundle) -> Result<Vec<BindingEntry>, ChromiumError> {
    BindingJournal::read(&bundle.root().join(INTERCEPTIONS))
}


impl Generation {
    fn interception_decision<'a>(&self, b: &'a Binding) -> &'a Decision {
        match &b.decision {
            Decision::Intercept { then } if self.released_interceptions.contains(&b.fetch_id) => then,
            decision => decision,
        }
    }
}

pub(super) fn missing_interception_records(events: &[BehaviorEvent], entries: &[BindingEntry], targets: &Targets)
    -> Result<Vec<(VirtualTime, InterceptionRecord)>, ChromiumError>
{
    let recorded: std::collections::BTreeSet<_> = events.iter().filter_map(|e| match &e.kind {
        EventKind::Interception(r) => Some(r.ordinal.as_str()), _ => None,
    }).collect();
    let last = events.last().map_or(VirtualTime::ZERO, |e| e.t_us);
    bound_by_pause(entries, targets)?.into_values()
        .filter(|b| matches!(b.decision, Decision::Intercept { .. }) && !recorded.contains(b.ordinal.as_str()))
        .map(|b| {
            let t = VirtualTime(b.issued_us);
            if t < last { return Err(ChromiumError::Shape(format!("{} interception predates the retained trace", b.ordinal))); }
            Ok((t, InterceptionRecord { ordinal: b.ordinal.clone(), method: b.method.clone(), url: b.url.clone() }))
        }).collect()
}
