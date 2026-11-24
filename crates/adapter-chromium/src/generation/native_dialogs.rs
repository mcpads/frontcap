use super::*;
use crate::dialog_control::DialogControl;
use frontcap_core::dialog::{DialogReply, DialogRules};
use std::sync::{Arc, Mutex};

pub(super) struct Driver {
    pub dialogs: DialogSession,
}

impl Drop for Driver {
    fn drop(&mut self) { self.retire(); }
}

fn kind(native: page::DialogType) -> DialogKind {
    match native {
        page::DialogType::Alert => DialogKind::Alert,
        page::DialogType::Confirm => DialogKind::Confirm,
        page::DialogType::Prompt => DialogKind::Prompt,
        page::DialogType::Beforeunload => DialogKind::Beforeunload,
    }
}


fn native_answer_request(id: &RequestId) -> String {
    format!("agent-answer-{}", frontcap_core::canonical::sha256_hex(id.as_str()))
}

/// An answer's receipt, with the page (context) it was given in.
type Given = (Option<String>, page::FrontcapDialogReceipt);

/// One page whose native dialogs a session serves: the launch page (no context) or a popup.
struct Served {
    context: Option<String>,
    control: DialogControl,
    /// Answers the dialogs the rules answer, as they open.
    task: tokio::task::JoinHandle<()>,
}

/// Independent dialog service for a generation whose main operation may be parked in native code. It
/// serves the launch page and every popup: a dialog in any of them blocks the renderer they share.
#[derive(Clone)]
pub struct DialogSession {
    pages: Arc<Mutex<Vec<Served>>>,
    /// Bumped whenever any page's inventory changes.
    changed: Arc<tokio::sync::watch::Sender<u64>>,
    /// The receipts of the answers given, by page: a popup that closes takes its retained receipts with
    /// it, so its last answers (a `beforeunload` that let it close) are recorded from these.
    given: Arc<Mutex<Vec<Given>>>,
    bindings: Arc<Mutex<DialogBindings>>,
    rules: Arc<Mutex<DialogRules>>,
    initial_date_ms: u64,
    acknowledged: Arc<Mutex<std::collections::BTreeSet<String>>>,
    retired: Arc<std::sync::atomic::AtomicBool>,
}

enum RecoveryWorker {
    Runnable(Session),
    Synchronous { renderer: String },
}

/// Owns a browser connection while a recovering recorder reconciles held dialogs before renderer
/// initialization. Dropping it does not answer dialogs; another connection can adopt the same ledger.
pub struct DialogRecovery {
    page: Session,
    fetch_pages: BTreeMap<String, Session>,
    worker_sessions: BTreeMap<String, Session>,
    shared_hold: Option<BlockingHold>,
    #[cfg(feature = "test-hooks")]
    interrupt_response_at: Option<VirtualTime>,
    /// How long after a synchronous fulfillment the engine's main-thread arrival stays unreported
    /// (`FRONTCAP_ARRIVAL_REPORT_DELAY=<ms>`), as on a renderer still reading that response.
    #[cfg(feature = "fault-injection")]
    arrival_report: (Option<Duration>, Option<Instant>),
    blocking_target: String,
    interception_armed: bool,
    popup_targets: std::collections::BTreeSet<String>,
    driver: Driver,
    identity: (LaunchId, Endpoint, PathBuf, RunId),
    paused: Option<cdp::Subscription<fetch::RequestPausedEvent>>,
    pending: Vec<cdp::Tagged<fetch::RequestPausedEvent>>,
    pub(super) blocking_answer_time: BTreeMap<String, VirtualTime>,
    pub(super) cancelled_navigation: Option<(RequestId, ActAction, VirtualTime)>,
}

fn renderer_request_number(id: &str) -> Option<(&str, u64)> {
    let (owner, number) = id.rsplit_once('.')?;
    Some((owner, number.parse().ok()?))
}

impl DialogRecovery {
    #[cfg(feature = "test-hooks")]
    pub fn test_interrupt_response_recovery_at(&mut self, at: VirtualTime) {
        self.interrupt_response_at = Some(at);
    }

    #[cfg(feature = "test-hooks")]
    pub fn test_abandon(self) {
        self.page.client.close();
    }

    /// Ordinary prefixes remain inspectable while a recovered synchronous task is released.
    pub fn preserve_interception_stop(&mut self) { self.interception_armed = true; }

    /// The engine's report that a renderer's main thread waits for a synchronous response's
    /// arrival. The renderer registers that wait only after reading the whole response, so the
    /// report can lag behind the fulfillment that caused it.
    async fn reported_main_thread_arrival(&self, clock: &str, renderer: String) -> Result<Option<String>, ChromiumError> {
        let arrival = self.page.call(&page::FrontcapGetSharedProcessParams::new(clock.to_owned(), renderer)).await?
            .main_thread_arrival_us;
        #[cfg(feature = "fault-injection")]
        if let (Some(lag), Some(fulfilled)) = self.arrival_report && fulfilled.elapsed() < lag { return Ok(None); }
        Ok(arrival)
    }

    fn fulfilled_synchronously(&mut self) {
        #[cfg(feature = "fault-injection")]
        { self.arrival_report.1 = Some(Instant::now()); }
    }

    fn clock_page(&self) -> &Session { &self.page }

    fn select_blocking_target(&mut self, target_id: &str) -> Result<(), ChromiumError> {
        if target_id != self.identity.1.target_id && !self.popup_targets.contains(target_id) {
            return Err(ChromiumError::Unsupported("blocking recovery requires a retained page target".into()));
        }
        self.blocking_target = target_id.into();
        Ok(())
    }

    fn blocking_page(&self) -> &Session {
        self.fetch_pages.get(&self.blocking_target).expect("selected target inventory is attached")
    }

    fn target_of_pause(&self, event: &fetch::RequestPausedEvent) -> Result<String, ChromiumError> {
        let session = self.pending.iter().find(|t| t.event.request_id == event.request_id)
            .and_then(|t| t.session.as_deref())
            .ok_or_else(|| ChromiumError::Shape("recovery pause has no target session".into()))?;
        self.fetch_pages.iter().find(|(_, page)| page.id == session).map(|(target, _)| target.clone())
            .ok_or_else(|| ChromiumError::Shape("recovery pause belongs to an unknown target".into()))
    }

    /// Each retained page has a temporary Fetch consumer, separate from dialog/clock service.
    /// Collect them together before releasing the task; a continuation can enter another page.
    async fn read_blocking_inventory(&mut self) -> Result<(), ChromiumError> {
        if self.paused.is_none() {
            let paused = self.page.subscribe();
            let targets: Vec<_> = std::iter::once(self.identity.1.target_id.clone())
                .chain(self.popup_targets.iter().cloned()).collect();
            for target_id in targets {
                let attached = self.page.client.call(None, &target::AttachToTargetParams {
                    flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(target_id.clone()))
                }).await?;
                let page = Session::sharing_clock(self.page.client.clone(), attached.session_id.0, self.page.service_clock());
                self.page.client.caller().route(&page.id, &paused.feed());
                page.call(&fetch::EnableParams {
                    patterns: Some(vec![fetch::RequestPattern { url_pattern: Some("*".into()),
                        request_stage: Some(fetch::RequestStage::Request), ..Default::default() }]), ..Default::default()
                }).await?;
                self.fetch_pages.insert(target_id, page);
            }
            self.paused = Some(paused);
        }
        while let Some(event) = self.paused.as_mut().expect("subscribed").try_next_tagged()? {
            self.pending.push(event);
        }
        Ok(())
    }

    /// Fetch.enable may acknowledge before replaying a retained pause. Its absence is not an
    /// answer: wait for the pause or a receipt from the target that owns the durable binding.
    async fn await_bound_request(&mut self, binding: &crate::bindings::Binding) -> Result<(), ChromiumError> {
        let deadline = Instant::now() + Session::CALL_TIMEOUT;
        loop {
            self.read_blocking_inventory().await?;
            if self.pending.iter().any(|p| p.event.request_id.0 == binding.fetch_id) { return Ok(()); }
            let page = self.fetch_pages.get(&binding.target_id)
                .ok_or_else(|| ChromiumError::Shape("bound recovery request has no retained target".into()))?;
            if page.call(&fetch::FrontcapReceiptsParams::default()).await?.receipts.iter()
                .any(|r| r.request_id.0 == binding.fetch_id) { return Ok(()); }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(ChromiumError::Shape(format!("{}: no native evidence for the bound recovery request", binding.ordinal)));
            }
            if let Ok(event) = tokio::time::timeout(left.min(Duration::from_millis(50)),
                self.paused.as_mut().expect("subscribed").next_tagged()).await {
                self.pending.push(event?);
            }
        }
    }

    /// Reconcile an interrupted agent answer without issuing it again. A retained pause proves
    /// non-application; a disappeared pause requires the matching native receipt and dispatch.
    pub async fn blocking_answer_applied(&mut self, ordinal: &str, answer: &BlockingAnswer) -> Result<bool, ChromiumError> {
        use crate::bindings::{BindingEntry, BindingJournal, Resolution};
        let path = self.identity.2.join(self.identity.3.as_str()).join("interceptions.jsonl");
        let entries = BindingJournal::read(&path)?;
        let mut bindings = entries.iter().filter_map(|e| match e {
            BindingEntry::Bound(b) if b.ordinal == ordinal => Some(b.as_ref()), _ => None,
        });
        let binding = bindings.next().ok_or_else(|| ChromiumError::Shape(format!("{ordinal}: no binding for the interrupted answer")))?;
        if bindings.any(|b| b != binding) {
            return Err(ChromiumError::Unsupported("blocking answer recovery requires one unambiguous binding".into()));
        }
        self.select_blocking_target(&binding.target_id)?;
        self.await_bound_request(binding).await?;
        if self.pending.iter().any(|t| t.event.request_id.0 == binding.fetch_id) { return Ok(false); }
        let expected = match answer {
            BlockingAnswer::Respond(spec) => Resolution::Fulfill { status: spec.status, body_sha256: crate::bindings::body_sha256(spec) },
            BlockingAnswer::Fail(reason) => Resolution::Fail { reason: *reason },
        };
        let dispatched = entries.iter().any(|e| matches!(e, BindingEntry::Dispatched { ordinal: id, fetch_id, resolution }
            if id == ordinal && fetch_id == &binding.fetch_id && resolution == &expected));
        let receipts = self.blocking_page().call(&fetch::FrontcapReceiptsParams::default()).await?.receipts;
        let matches = receipts.iter().any(|r| r.request_id.0 == binding.fetch_id && match &expected {
            Resolution::Fulfill { status, body_sha256 } => r.outcome == fetch::FrontcapReceiptOutcome::Fulfilled
                && r.response_code == Some(i64::from(*status)) && r.body_sha256.as_ref() == Some(body_sha256),
            Resolution::Fail { .. } => r.outcome == fetch::FrontcapReceiptOutcome::Failed,
            Resolution::Continue => false,
        });
        if !dispatched || !matches {
            return Err(ChromiumError::Shape(format!("{ordinal}: interrupted answer has no matching native receipt and dispatch")));
        }
        Ok(true)
    }

    /// Apply the recorder's journaled lease-loss cleanup before renderer initialization. A
    /// synchronous load prevents even Page.enable from answering; Fetch runs in the browser.
    /// Keep the native receipt for normal adoption to publish the response exactly once.
    pub async fn abort_blocking_hold(&mut self, ordinal: &str) -> Result<(), ChromiumError> {
        use crate::bindings::{BindingEntry, BindingJournal, Decision, Resolution};
        let path = self.identity.2.join(self.identity.3.as_str()).join("interceptions.jsonl");
        let entries = BindingJournal::read(&path)?;
        let bindings: Vec<_> = entries.iter().filter_map(|e| match e {
            BindingEntry::Bound(b) if b.ordinal == ordinal => Some(b.as_ref()), _ => None,
        }).collect();
        let Some(binding) = bindings.first().copied() else {
            return Err(ChromiumError::Shape(format!("{ordinal}: blocking cleanup has no binding")));
        };
        if bindings.iter().any(|b| *b != binding)
            || !matches!(binding.decision, Decision::Hold)
        {
            return Err(ChromiumError::Unsupported("blocking cleanup requires one unambiguous bound hold".into()));
        }
        self.select_blocking_target(&binding.target_id)?;
        self.await_bound_request(binding).await?;
        let pending = self.pending.iter().position(|t| t.event.request_id.0 == binding.fetch_id)
            .map(|at| self.pending.remove(at).event);
        let sent = entries.iter().any(|e| matches!(e, BindingEntry::Dispatched { ordinal: id, fetch_id, resolution: Resolution::Fail { reason: FailReason::Aborted } }
            if id == ordinal && fetch_id == &binding.fetch_id));
        // Queue the pause before releasing the blocked task. Otherwise its old grant may
        // advance to the next tick before renderer initialization takes control again.
        let pause = self.clock_page().send(&emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause))?;
        let was_pending = pending.is_some();
        if let Some(event) = pending {
            if event.frontcap_blocks_main_thread != Some(true) || event.request.url != binding.url
                || event.request.method != binding.method
            {
                return Err(ChromiumError::Shape(format!("{ordinal}: native blocking request does not match its binding")));
            }
            if !sent {
                BindingJournal::open(&path)?.append(&BindingEntry::Dispatched {
                    ordinal: ordinal.into(), fetch_id: binding.fetch_id.clone(),
                    resolution: Resolution::Fail { reason: FailReason::Aborted },
                })?;
            }
            self.blocking_page().call(&fetch::FailRequestParams::new(event.request_id, cnet::ErrorReason::Aborted)).await?;
        }
        // The clock owner may still be waiting for an already fulfilled response
        // while another renderer's held request is cancelled. Restore that arrival
        // before awaiting the owner's renderer pause.
        self.resume_retained_shared_responses().await?;
        self.select_blocking_target(&binding.target_id)?;
        let settled = async {
            // A previous recovery may have delivered this answer before dying. Its successor
            // is then already in the adopted inventory, rather than arriving on the subscription.
            if self.pending.iter().any(|t| t.event.frontcap_blocks_main_thread == Some(true)) {
                return Ok(());
            }
            let answer = pause.answered();
            tokio::pin!(answer);
            loop {
                tokio::select! {
                    result = &mut answer => return Ok::<_, ChromiumError>(result?),
                    event = self.paused.as_mut().expect("subscribed").next_tagged() => {
                        let event = event?;
                        if event.event.frontcap_blocks_main_thread == Some(true) {
                            self.pending.push(event);
                            return Ok(());
                        }
                        self.pending.push(event);
                    }
                }
            }
        };
        tokio::time::timeout(Session::CALL_TIMEOUT, settled).await
            .map_err(|_| ChromiumError::CallTimeout("blocking recovery pause"))??;
        let receipts = self.blocking_page().call(&fetch::FrontcapReceiptsParams::default()).await?.receipts;
        if !receipts.iter().any(|r| r.request_id.0 == binding.fetch_id && r.outcome == fetch::FrontcapReceiptOutcome::Failed)
            || (!sent && !was_pending)
        {
            return Err(ChromiumError::Shape(format!("{ordinal}: blocking cleanup has no matching native failure receipt")));
        }
        Ok(())
    }

    /// Restore the instant of an earlier dependent answer, including one finished by a lost recovery.
    pub fn preserve_bound_answer_time(&mut self, ordinal: &str) -> Result<(), ChromiumError> {
        let path = self.identity.2.join(self.identity.3.as_str()).join("interceptions.jsonl");
        for entry in crate::bindings::BindingJournal::read(&path)? {
            if let crate::bindings::BindingEntry::Bound(binding) = entry
                && binding.ordinal == ordinal && (binding.target_id == self.identity.1.target_id || self.popup_targets.contains(&binding.target_id)) {
                self.preserve_blocking_answer_time(ordinal.into(), VirtualTime(binding.issued_us));
                return Ok(());
            }
        }
        Err(ChromiumError::Shape(format!("{ordinal}: recovered answer has no binding")))
    }

    /// Collect a browser-owned snapshot, including pauses with no durable binding. Event delivery
    /// can lag behind the snapshot; each identity needs its notification or a target-owned receipt.
    async fn collect_native_inventory(&mut self) -> Result<(), ChromiumError> {
        self.read_blocking_inventory().await?;
        let mut required = Vec::new();
        for (target, page) in &self.fetch_pages {
            required.extend(page.call(&fetch::FrontcapPendingRequestsParams::default()).await?.request_ids
                .into_iter().map(|id| (target.clone(), id.0)));
        }
        for (target, id) in required {
            let deadline = Instant::now() + Session::CALL_TIMEOUT;
            loop {
                self.read_blocking_inventory().await?;
                if self.pending.iter().any(|p| p.event.request_id.0 == id) { break; }
                if self.fetch_pages[&target].call(&fetch::FrontcapReceiptsParams::default()).await?.receipts.iter()
                    .any(|r| r.request_id.0 == id) { break; }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(ChromiumError::Shape(format!("{id}: native inventory notification did not arrive")));
                }
                if let Ok(event) = tokio::time::timeout(left.min(Duration::from_millis(50)),
                    self.paused.as_mut().expect("subscribed").next_tagged()).await {
                    self.pending.push(event?);
                }
            }
        }
        Ok(())
    }

    /// Stop the old grant, or discover the task preventing the renderer from acknowledging it.
    async fn wait_for_blocking_task(&mut self) -> Result<(), ChromiumError> {
        self.collect_native_inventory().await?;
        if self.pending.iter().any(|p| p.event.frontcap_blocks_main_thread == Some(true))
            || self.driver.dialogs.pending()?.is_some() { return Ok(()); }
        let pause = self.clock_page().send(&emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause))?;
        let answer = pause.answered();
        tokio::pin!(answer);
        loop {
            tokio::select! {
                result = &mut answer => { result?; return Ok(()); },
                _ = tokio::time::sleep(Duration::from_millis(5)) => {
                    if self.driver.dialogs.pending()?.is_some() { return Ok(()); }
                },
                event = self.paused.as_mut().expect("subscribed").next_tagged() => {
                    let event = event?;
                    let blocking = event.event.frontcap_blocks_main_thread == Some(true);
                    self.pending.push(event);
                    if blocking { return self.collect_native_inventory().await; }
                }
            }
        }
    }

    /// Recover a blocked task before renderer initialization, including loss before its first
    /// binding. Freeze ordinary document prefixes before selecting the synchronous request.
    pub async fn recover_blocking_request(&mut self, progressing: bool) -> Result<Option<BlockingHold>, ChromiumError> {
        use crate::bindings::{BindingEntry, BindingJournal, Decision};
        self.resume_retained_shared_responses().await?;
        if let Some(hold) = self.shared_hold.take() { return Ok(Some(hold)); }
        self.wait_for_blocking_task().await?;
        let mut blockers = self.pending.iter().filter(|p| p.event.frontcap_blocks_main_thread == Some(true));
        let Some(blocker) = blockers.next() else { return Ok(None) };
        if blockers.next().is_some() {
            return Err(ChromiumError::Unsupported("suspension recovery requires one blocked document task".into()));
        }
        let event = blocker.event.clone();
        let bundle = Bundle::open(&self.identity.2, &self.identity.3)?;
        let entries = BindingJournal::read(&bundle.root().join("interceptions.jsonl"))?;
        let bound: Vec<_> = entries.iter().filter_map(|e| match e {
            BindingEntry::Bound(b) => Some(b.as_ref()), _ => None,
        }).collect();
        let binding = if let Some(binding) = bound.iter().find(|b| b.fetch_id == event.request_id.0) {
            if bound.iter().any(|b| b.fetch_id == binding.fetch_id && b != binding) {
                return Err(ChromiumError::Shape("suspension recovery has contradictory bindings".into()));
            }
            (*binding).clone()
        } else {
            let (owner, last) = event.network_id.as_ref().and_then(|id| renderer_request_number(&id.0))
                .ok_or_else(|| ChromiumError::Unsupported("unbound blocker has no renderer request identity".into()))?;
            let mut prefix = Vec::new();
            for pause in &self.pending {
                let request = &pause.event;
                if request.frontcap_worker_id.is_some() || bound.iter().any(|b| b.fetch_id == request.request_id.0) { continue; }
                let Some((issuer, number)) = request.network_id.as_ref().and_then(|id| renderer_request_number(&id.0)) else {
                    return Err(ChromiumError::Unsupported("unbound document prefix has no renderer identity".into()));
                };
                if issuer != owner || number > last {
                    return Err(ChromiumError::Unsupported("unbound document prefixes require the blocked renderer".into()));
                }
                prefix.push((number, request.clone()));
            }
            prefix.sort_by_key(|(number, _)| *number);
            if prefix.len() > 64 { return Err(ChromiumError::Unsupported("unbound recovery exceeded 64 document requests".into())); }
            let mut selected = None;
            for (_, request) in prefix {
                let binding = self.bind_recovery_request(&bundle, None, &request, false, progressing)?;
                if request.request_id == event.request_id { selected = Some(binding); }
            }
            selected.ok_or_else(|| ChromiumError::Shape("native blocker was absent from its inventory".into()))?
        };
        let issued_us = event.frontcap_issue_time_us.as_deref().and_then(|s| s.parse::<u64>().ok())
            .and_then(|us| us.checked_sub(self.driver.dialogs.initial_date_ms.saturating_mul(1000)));
        if self.target_of_pause(&event)? != binding.target_id || event.request.url != binding.url
            || event.request.method != binding.method || issued_us != Some(binding.issued_us) {
            return Err(ChromiumError::Shape("suspension recovery contradicts the retained request binding".into()));
        }
        self.select_blocking_target(&binding.target_id)?;
        super::interception::interception_fault_point("initial_after_binding");
        if matches!(binding.decision, Decision::Hold) {
            return Ok(Some(BlockingHold { ordinal: binding.ordinal, method: binding.method, url: binding.url,
                rule: binding.rule, rules_revision: binding.rules_revision, t_us: VirtualTime(binding.issued_us) }));
        }
        self.resolve_blocking_request(&binding).await?;
        if let Some(hold) = self.shared_hold.take() { return Ok(Some(hold)); }
        self.next_blocking_hold(&binding.ordinal).await
    }

    /// Continue automatic decisions until the task ends or another held request needs a
    /// recorder-owned answer. Bindings link the chain across a second recorder loss.
    pub async fn next_blocking_hold(&mut self, previous: &str) -> Result<Option<BlockingHold>, ChromiumError> {
        if let Some(hold) = self.shared_hold.take() { return Ok(Some(hold)); }
        self.resume_retained_shared_responses().await?;
        let mut previous = previous.to_owned();
        let mut answered = 0;
        loop {
            if let Some(hold) = self.shared_hold.take() { return Ok(Some(hold)); }
            let Some(binding) = self.next_blocking_request(&previous).await? else { return Ok(None) };
            if matches!(binding.decision, crate::bindings::Decision::Hold) {
                return Ok(Some(BlockingHold { ordinal: binding.ordinal, method: binding.method, url: binding.url,
                    rule: binding.rule, rules_revision: binding.rules_revision, t_us: VirtualTime(binding.issued_us) }));
            }
            if answered == 64 {
                return Err(ChromiumError::Unsupported("blocking recovery exceeded 64 consecutive automatic requests".into()));
            }
            self.resolve_blocking_request(&binding).await?;
            answered += 1;
            previous = binding.ordinal;
        }
    }

    async fn resolve_blocking_request(&mut self, binding: &crate::bindings::Binding) -> Result<(), ChromiumError> {
        use crate::bindings::{BindingEntry, BindingJournal, Decision, Resolution};
        if matches!(binding.decision, Decision::Passthrough) {
            return self.continue_blocking_request(binding).await;
        }
        let (resolution, at) = match &binding.decision {
            Decision::Deliver { spec, due_us, .. } => (Resolution::Fulfill { status: spec.status,
                body_sha256: crate::bindings::body_sha256(spec) }, *due_us),
            Decision::Fail { reason } => (Resolution::Fail { reason: *reason }, binding.issued_us),
            _ => return Err(ChromiumError::Unsupported("pre-renderer recovery requires a held or automatic response".into())),
        };
        self.read_blocking_inventory().await?;
        let path = self.identity.2.join(self.identity.3.as_str()).join("interceptions.jsonl");
        let entries = BindingJournal::read(&path)?;
        let dispatched = entries.iter().any(|e| matches!(e, BindingEntry::Dispatched { ordinal, fetch_id, resolution: r }
            if ordinal == &binding.ordinal && fetch_id == &binding.fetch_id && r == &resolution));
        if let Some(index) = self.pending.iter().position(|t| t.event.request_id.0 == binding.fetch_id) {
            let event = self.pending.remove(index).event;
            if event.frontcap_blocks_main_thread != Some(true) || event.request.url != binding.url || event.request.method != binding.method {
                return Err(ChromiumError::Shape("automatic blocking response contradicts native inventory".into()));
            }
            // Queue before delivery so the old parent grant cannot escape after the task returns.
            let _pause = self.clock_page().send(&emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause))?;
            if !dispatched {
                BindingJournal::open(&path)?.append(&BindingEntry::Dispatched { ordinal: binding.ordinal.clone(),
                    fetch_id: binding.fetch_id.clone(), resolution: resolution.clone() })?;
            }
            super::interception::interception_fault_point(&format!("blocking_after_dispatch:{}", binding.ordinal));
            match &binding.decision {
                Decision::Deliver { spec, source, .. } => {
                    self.blocking_page().call(&super::interception::fulfillment(event.request_id, spec, *source,
                        binding.origin.as_deref(), Some(VirtualTime(at)), self.driver.dialogs.initial_date_ms)).await?;
                    self.fulfilled_synchronously();
                }
                Decision::Fail { reason } => {
                    self.blocking_page().call(&fetch::FailRequestParams::new(event.request_id, error_reason(*reason))).await?;
                }
                _ => unreachable!(),
            }
            super::interception::interception_fault_point(&format!("blocking_after_effect:{}", binding.ordinal));
        } else if !dispatched {
            return Err(ChromiumError::Shape("automatic blocking response disappeared before dispatch".into()));
        }
        let receipts = self.blocking_page().call(&fetch::FrontcapReceiptsParams::default()).await?.receipts;
        if !receipts.iter().any(|r| r.request_id.0 == binding.fetch_id && match &resolution {
            Resolution::Fulfill { status, body_sha256 } => r.outcome == fetch::FrontcapReceiptOutcome::Fulfilled
                && r.response_code == Some(i64::from(*status)) && r.body_sha256.as_ref() == Some(body_sha256),
            Resolution::Fail { .. } => r.outcome == fetch::FrontcapReceiptOutcome::Failed,
            _ => false,
        }) {
            return Err(ChromiumError::Shape("automatic blocking response has no matching native receipt".into()));
        }
        self.advance_shared_recovery_response(&binding.target_id, VirtualTime(at)).await?;
        self.preserve_blocking_answer_time(binding.ordinal.clone(), VirtualTime(at));
        Ok(())
    }

    /// Inspector detach retains fulfilled native arrivals even after Fetch removes
    /// their pauses. Reconcile them before waiting for a renderer pause reply.
    async fn resume_retained_shared_responses(&mut self) -> Result<(), ChromiumError> {
        use crate::bindings::{BindingEntry, BindingJournal, Decision, Resolution};
        let state = match self.page.call(&page::FrontcapGetSharedClockParams::default()).await {
            Ok(state) => state,
            Err(ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. }))
                if message == "shared clock is not prepared" => return Ok(()),
            Err(error) => return Err(error),
        };
        if !state.activated { return Ok(()); }
        let held = self.page.call(&page::FrontcapSuspendSharedClockParams::new(state.clock_id.clone())).await?;
        let now = held.unix_time_us.parse::<i128>().map_err(|_| ChromiumError::Shape("invalid retained shared instant".into()))?;
        self.collect_native_inventory().await?;
        let bundle = Bundle::open(&self.identity.2, &self.identity.3)?;
        let entries = BindingJournal::read(&bundle.root().join("interceptions.jsonl"))?;
        let mut recovered = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for (target, session) in &self.fetch_pages {
            let renderer = session.call(&page::FrontcapAdmitSharedWindowParams::new(state.clock_id.clone())).await?.renderer_id;
            let arrival = self.reported_main_thread_arrival(&state.clock_id, renderer.clone()).await?
                .map(|s| s.parse::<i128>()).transpose()
                .map_err(|_| ChromiumError::Shape("invalid retained shared arrival".into()))?;
            let Some(arrival) = arrival.filter(|t| *t > now) else { continue };
            let receipts = session.call(&fetch::FrontcapReceiptsParams::default()).await?.receipts;
            for entry in &entries {
                let BindingEntry::Bound(binding) = entry else { continue };
                if binding.target_id != *target || !binding.blocks_main_thread { continue; }
                let Decision::Deliver { spec, due_us, .. } = &binding.decision else { continue };
                if i128::from(self.driver.dialogs.initial_date_ms)*1000+i128::from(*due_us) != arrival { continue; }
                let body = crate::bindings::body_sha256(spec);
                let dispatched = entries.iter().any(|e| matches!(e, BindingEntry::Dispatched { ordinal, fetch_id,
                    resolution: Resolution::Fulfill { status, body_sha256 } }
                    if ordinal == &binding.ordinal && fetch_id == &binding.fetch_id && *status == spec.status && body_sha256 == &body));
                let received = receipts.iter().any(|r| r.request_id.0 == binding.fetch_id
                    && r.outcome == fetch::FrontcapReceiptOutcome::Fulfilled && r.response_code == Some(i64::from(spec.status))
                    && r.body_sha256.as_ref() == Some(&body));
                if dispatched && received {
                    if !seen.insert(renderer.clone()) {
                        return Err(ChromiumError::Shape("ambiguous retained shared response".into()));
                    }
                    recovered.push((binding.target_id.clone(), binding.ordinal.clone(), VirtualTime(*due_us)));
                }
            }
        }
        if let Some((target, _, due)) = recovered.first() {
            self.advance_shared_recovery_response(target, *due).await?;
            for (_, ordinal, due) in recovered { self.preserve_blocking_answer_time(ordinal, due); }
        }
        Ok(())
    }

    /// A delivered synchronous response still waits for the shared authority to reach it.
    async fn advance_shared_recovery_response(&mut self, target: &str, due: VirtualTime) -> Result<(), ChromiumError> {
        let state = match self.page.call(&page::FrontcapGetSharedClockParams::default()).await {
            Ok(state) => state,
            Err(ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. }))
                if message == "shared clock is not prepared" => return Ok(()),
            Err(error) => return Err(error),
        };
        if !state.activated { return Ok(()); }
        let held = self.page.call(&page::FrontcapSuspendSharedClockParams::new(state.clock_id.clone())).await?;
        let now = held.unix_time_us.parse::<i128>().map_err(|_| ChromiumError::Shape("invalid shared recovery instant".into()))?;
        let mut end = i128::from(self.driver.dialogs.initial_date_ms) * 1000 + i128::from(due.0);
        if now >= end { return Ok(()); }
        let owner = self.fetch_pages.get(target).ok_or_else(|| ChromiumError::Shape("shared response owner missing".into()))?
            .call(&page::FrontcapAdmitSharedWindowParams::new(state.clock_id.clone())).await?.renderer_id;
        let mut participants = BTreeMap::new();
        for session in self.fetch_pages.values() {
            let member = session.call(&page::FrontcapAdmitSharedWindowParams::new(state.clock_id.clone())).await?;
            participants.entry(member.renderer_id).or_insert_with(|| session.clone());
        }
        if state.participants.iter().filter(|p| !p.retired).any(|p| !p.alive || !p.confirmed || !p.armed || !participants.contains_key(&p.renderer_id)) {
            return Err(ChromiumError::Shape("shared recovery participants are incomplete".into()));
        }
        let mut arrivals = BTreeMap::from([(owner.clone(), end)]);
        for renderer in participants.keys() {
            let arrival = self.reported_main_thread_arrival(&state.clock_id, renderer.clone()).await?
                .map(|s| s.parse::<i128>()).transpose()
                .map_err(|_| ChromiumError::Shape("invalid shared recovery arrival".into()))?;
            if let Some(due) = arrival.filter(|t| *t > now) {
                arrivals.insert(renderer.clone(), due);
                end = end.max(due);
            }
        }
        let mut grant_bounds = BTreeMap::new();
        let mut pausing: BTreeMap<String, cdp::Answer> = BTreeMap::new();
        let mut granting: BTreeMap<String, (cdp::Answer, i128, i128)> = BTreeMap::new();
        let mut paused = std::collections::BTreeSet::new();
        let result: Result<(), ChromiumError> = async {
            let deadline = Instant::now() + Session::CALL_TIMEOUT;
            loop {
                if Instant::now() >= deadline { return Err(ChromiumError::CallTimeout("shared recovery response arrival")); }
                self.read_blocking_inventory().await?;
                let current = self.page.call(&page::FrontcapGetSharedClockParams::default()).await?;
                let at = current.unix_time_us.parse::<i128>().map_err(|_| ChromiumError::Shape("invalid shared recovery instant".into()))?;
                if current.failed || at > end { return Err(ChromiumError::Shape("shared recovery exceeded response boundary".into())); }
                let held = self.page.call(&page::FrontcapSuspendSharedClockParams::new(state.clock_id.clone())).await?;
                let at = held.unix_time_us.parse::<i128>().map_err(|_| ChromiumError::Shape("invalid shared recovery instant".into()))?;
                let mut next = self.service_shared_recovery_requests(at, &mut end, &state.clock_id, &mut arrivals).await?;
                if self.shared_hold.is_some() { return Ok(()); }
                #[cfg(feature = "test-hooks")]
                if self.interrupt_response_at.is_some_and(|t| at >= i128::from(self.driver.dialogs.initial_date_ms)*1000+i128::from(t.0)) {
                    self.interrupt_response_at = None;
                    return Err(ChromiumError::Shape("test interrupted shared response recovery".into()));
                }
                let completed: Vec<_> = arrivals.iter().filter(|(_, due)| **due <= at).map(|(renderer, _)| renderer.clone()).collect();
                for renderer in completed {
                    arrivals.remove(&renderer);
                    grant_bounds.remove(&renderer);
                    paused.remove(&renderer);
                }
                // A continuation can start its next synchronous request before a
                // renderer acknowledges Pause or a grant. Keep those replies owned
                // while servicing the native request instead of awaiting one here.
                let mut answered = Vec::new();
                for (renderer, answer) in &mut pausing {
                    if let Some(result) = answer.try_answer() {
                        result?;
                        answered.push(renderer.clone());
                    }
                }
                for renderer in answered {
                    pausing.remove(&renderer);
                    paused.insert(renderer);
                }
                let mut answered = Vec::new();
                for (renderer, (answer, issued, bound)) in &mut granting {
                    if let Some(result) = answer.try_answer() {
                        result?;
                        // A delayed acknowledgment can install a relative budget
                        // after its original instant. Pause again before regranting.
                        if *issued == at && *bound == end { grant_bounds.insert(renderer.clone(), end); }
                        else { grant_bounds.remove(renderer); }
                        answered.push(renderer.clone());
                    }
                }
                for renderer in answered { granting.remove(&renderer); }
                for (renderer, session) in &participants {
                    if arrivals.contains_key(renderer) || pausing.contains_key(renderer) || granting.contains_key(renderer) { continue; }
                    if paused.contains(renderer) {
                        if at < end {
                            let answer = session.send(&emulation::SetVirtualTimePolicyParams { bind_storage_clock: Some(false),
                                budget: Some((end-at) as f64 / 1000.0),
                                ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending) })?;
                            granting.insert(renderer.clone(), (answer, at, end));
                            paused.remove(renderer);
                        }
                    } else if at == end || grant_bounds.get(renderer) != Some(&end) {
                        pausing.insert(renderer.clone(), session.send(&emulation::SetVirtualTimePolicyParams {
                            bind_storage_clock: Some(false),
                            ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause) })?);
                    }
                }
                if at == end && participants.keys().all(|r| paused.contains(r)) { return Ok(()); }
                if at == end || participants.keys().any(|r| !arrivals.contains_key(r)
                    && (pausing.contains_key(r) || granting.contains_key(r) || grant_bounds.get(r) != Some(&end))) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue;
                }
                next = next.min(arrivals.values().copied().min().unwrap_or(end));
                self.page.call(&page::FrontcapResumeSharedClockParams {
                    bound_us: Some(next.to_string()),
                    ..page::FrontcapResumeSharedClockParams::new(state.clock_id.clone(), held.suspension_id)
                }).await?;
                if Instant::now() >= deadline { return Err(ChromiumError::CallTimeout("shared recovery response arrival")); }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        }.await;
        // Retain the hold on success and failure; ordinary adoption owns subsequent grants.
        self.page.call(&page::FrontcapSuspendSharedClockParams::new(state.clock_id)).await?;
        result
    }

    /// Select ordinary requests while held, releasing their request vote only after
    /// the next publication bound includes their response deadline.
    async fn service_shared_recovery_requests(&mut self, at: i128, end: &mut i128, clock: &str,
        arrivals: &mut BTreeMap<String, i128>) -> Result<i128, ChromiumError> {
        use crate::bindings::{BindingEntry, BindingJournal, Decision, Resolution};
        self.collect_native_inventory().await?;
        let bundle = Bundle::open(&self.identity.2, &self.identity.3)?;
        let epoch = i128::from(self.driver.dialogs.initial_date_ms) * 1000;
        let mut next = *end;
        let events: Vec<_> = self.pending.iter().map(|p| p.event.clone()).collect();
        // A renderer's main thread waits inside its synchronous load from the request's pause
        // until the response's arrival, and cannot answer a command meanwhile. The engine reports
        // that wait only after the renderer has read the response, so this native inventory and
        // this recovery's own deliveries decide it first.
        let mut waiting = std::collections::BTreeSet::new();
        for event in events.iter().filter(|e| e.frontcap_blocks_main_thread == Some(true) && e.frontcap_worker_id.is_none()) {
            let session = self.fetch_pages.get(&self.target_of_pause(event)?)
                .ok_or_else(|| ChromiumError::Shape("shared recovery request owner missing".into()))?;
            waiting.insert(session.call(&page::FrontcapAdmitSharedWindowParams::new(clock.to_owned())).await?.renderer_id);
        }
        for event in events {
            let worker = if let Some(worker) = &event.frontcap_worker_id {
                Some(self.recover_shared_worker_request(&bundle, &event, worker, clock).await?)
            } else { None };
            let synchronous = event.frontcap_blocks_main_thread == Some(true);
            let binding = self.bind_recovery_request(&bundle, None, &event, synchronous, true)?;
            let session = self.fetch_pages.get(&binding.target_id)
                .ok_or_else(|| ChromiumError::Shape("shared recovery request owner missing".into()))?.clone();
            if synchronous && matches!(binding.decision, Decision::Hold) {
                self.select_blocking_target(&binding.target_id)?;
                self.shared_hold = Some(BlockingHold { ordinal: binding.ordinal, method: binding.method,
                    url: binding.url, rule: binding.rule, rules_revision: binding.rules_revision,
                    t_us: VirtualTime(binding.issued_us) });
                return Ok(at);
            }
            if matches!(binding.decision, Decision::Hold)
                || (!synchronous && matches!(binding.decision, Decision::Intercept { .. })) {
                // Ordinary holds and interception stops retain their Fetch pause
                // and durable decision for adoption. Release only their time vote
                // so another renderer's synchronous response can reach its deadline.
                self.release_shared_request_vote(clock, &session, worker.as_ref(), &event,
                    |r| waiting.contains(r) || arrivals.get(r).is_some_and(|due| *due > at)).await?;
                continue;
            }
            let (due, resolution) = match &binding.decision {
                Decision::Deliver { spec, due_us, .. } => (epoch + i128::from(*due_us), Resolution::Fulfill {
                    status: spec.status, body_sha256: crate::bindings::body_sha256(spec),
                }),
                Decision::Fail { reason } => (epoch + i128::from(binding.issued_us), Resolution::Fail { reason: *reason }),
                _ => return Err(ChromiumError::Unsupported("shared recovery request decision requires additional service".into())),
            };
            if due < at { return Err(ChromiumError::Shape(format!("shared recovery passed ordinary deadline: request={}, issued={}, due={due}, at={at}", binding.url, binding.issued_us))); }
            if synchronous {
                let renderer = session.call(&page::FrontcapAdmitSharedWindowParams::new(clock.to_owned())).await?.renderer_id;
                if arrivals.get(&renderer).is_some_and(|previous| *previous > at) {
                    return Err(ChromiumError::Shape("shared recovery has overlapping main-thread arrivals in one renderer".into()));
                }
                arrivals.insert(renderer, due);
                *end = (*end).max(due);
            }
            if due == at || synchronous {
                // A blocked thread cannot process request-vote commands. Native delivery
                // registers its arrival deadline while publication remains suspended.
                next = next.min(due);
                BindingJournal::open(&bundle.root().join("interceptions.jsonl"))?.append(&BindingEntry::Dispatched {
                    ordinal: binding.ordinal.clone(), fetch_id: binding.fetch_id.clone(), resolution,
                })?;
                if synchronous {
                    super::interception::interception_fault_point(&format!("blocking_after_dispatch:{}", binding.ordinal));
                }
                match &binding.decision {
                    Decision::Deliver { spec, source, due_us, .. } => {
                        session.call(&super::interception::fulfillment(event.request_id.clone(), spec, *source,
                            binding.origin.as_deref(), Some(VirtualTime(*due_us)), self.driver.dialogs.initial_date_ms)).await?;
                        if synchronous { self.fulfilled_synchronously(); }
                    }
                    Decision::Fail { reason } => { session.call(&fetch::FailRequestParams::new(event.request_id.clone(), error_reason(*reason))).await?; }
                    _ => unreachable!(),
                }
                if synchronous {
                    super::interception::interception_fault_point(&format!("blocking_after_effect:{}", binding.ordinal));
                }
                self.pending.retain(|p| p.event.request_id != event.request_id);
            } else {
                next = next.min(due);
                self.release_shared_request_vote(clock, &session, worker.as_ref(), &event,
                    |r| waiting.contains(r) || arrivals.get(r).is_some_and(|due| *due > at)).await?;
            }
        }
        Ok(next)
    }

    async fn release_shared_request_vote(&self, clock: &str, session: &Session,
        worker: Option<&RecoveryWorker>, event: &fetch::RequestPausedEvent,
        waits_for_response: impl Fn(&str) -> bool) -> Result<(), ChromiumError> {
        let network = event.network_id.clone().ok_or_else(|| ChromiumError::Shape("shared recovery request has no network identity".into()))?;
        match worker {
            Some(RecoveryWorker::Synchronous { renderer }) => {
                let held = self.page.call(&page::FrontcapGetSharedClockParams::default()).await?;
                self.page.call(&page::FrontcapSetSharedWorkerRequestPausedParams::new(
                    clock.to_owned(), held.suspension_id, renderer.clone(), network, false)).await?;
            }
            Some(RecoveryWorker::Runnable(worker)) => { worker.call(&cnet::SetRequestPausesVirtualTimeParams::new(network, false)).await?; }
            None => {
                let renderer = session.call(&page::FrontcapAdmitSharedWindowParams::new(clock.to_owned())).await?.renderer_id;
                // The page's ordinary prefix cannot answer a Network command
                // while that same main thread waits for its synchronous response.
                // Its pending Fetch entry is retained and revisited after arrival.
                if waits_for_response(&renderer)
                    || self.reported_main_thread_arrival(clock, renderer).await?.is_some() { return Ok(()); }
                session.call(&cnet::SetRequestPausesVirtualTimeParams::new(network, false)).await?;
            }
        }
        Ok(())
    }

    async fn recover_shared_worker_request(&mut self, bundle: &Bundle, event: &fetch::RequestPausedEvent,
        worker: &str, clock: &str) -> Result<RecoveryWorker, ChromiumError>
    {
        use crate::bindings::{BindingEntry, BindingJournal, DocumentPlace};
        let network = event.network_id.as_ref().ok_or_else(|| ChromiumError::Shape("worker recovery request has no network identity".into()))?;
        let targets = self.page.client.call(None, &target::GetTargetsParams::default()).await?.target_infos;
        let mut lineages = super::worker_lineage::WorkerLineages::default();
        for info in targets.iter().filter(|t| t.r#type == "worker") {
            lineages.remember(&info.target_id.0, info)?;
        }
        let lineage = lineages.for_target(worker)?.ok_or_else(|| ChromiumError::Shape("worker recovery has no native constructor lineage".into()))?;
        let entries = BindingJournal::read(&bundle.root().join("interceptions.jsonl"))?;
        if let Some(previous) = super::worker_lineage::recovered_owners(&entries)?.get(&network.0) {
            if previous != &lineage { return Err(ChromiumError::Shape("recovered worker request changed constructor".into())); }
        } else {
            let events = BehaviorLog::read(&bundle.trace())?;
            let mut windows = vec![self.identity.1.target_id.clone()];
            for event in events {
                if let EventKind::Context(context) = event.kind
                    && context.event == frontcap_core::event::ContextEvent::Opened
                    && let Some(target) = context.target
                    && !windows.contains(&target) { windows.push(target); }
            }
            let mut place = DocumentPlace::Retired;
            for (window, target) in windows.iter().enumerate() {
                let Some(session) = self.fetch_pages.get(target) else { continue };
                let snapshot = session.call(&page::FrontcapGetDocumentPlacesParams::default()).await?;
                if let Some(document) = snapshot.documents.into_iter().find(|d| d.document_id == lineage.creator_document) {
                    let frames = document.frame_path.into_iter().map(|i| u32::try_from(i)
                        .map_err(|_| ChromiumError::Shape("invalid recovered worker document path".into()))).collect::<Result<Vec<_>, _>>()?;
                    place = DocumentPlace::Current { window: window as u64, frames };
                    break;
                }
            }
            let issued_us = event.frontcap_issue_time_us.as_deref().and_then(|s| s.parse::<u64>().ok())
                .and_then(|t| t.checked_sub(self.driver.dialogs.initial_date_ms.saturating_mul(1000)))
                .ok_or_else(|| ChromiumError::Shape("worker recovery request has no issue instant".into()))?;
            BindingJournal::open(&bundle.root().join("interceptions.jsonl"))?.append(&BindingEntry::WorkerRequestOwner {
                network_id: network.0.clone(), lineage, issued_us, document: Some(place),
            })?;
        }
        let target = self.target_of_pause(event)?;
        let renderer = self.fetch_pages[&target].call(&page::FrontcapAdmitSharedWindowParams::new(clock.to_owned())).await?.renderer_id;
        let process = self.page.call(&page::FrontcapGetSharedProcessParams::new(clock.to_owned(), renderer.clone())).await?;
        let native = process.workers.iter().find(|w| w.target_id == worker)
            .ok_or_else(|| ChromiumError::Shape("worker disappeared from shared process inventory".into()))?;
        if let Some(wait) = &native.synchronous_wait_id {
            if network.0.rsplit_once('.').map(|(_, number)| number) != Some(wait.as_str()) {
                return Err(ChromiumError::Unsupported("worker is blocked on a different request during recovery".into()));
            }
            return Ok(RecoveryWorker::Synchronous { renderer });
        }
        if let Some(session) = self.worker_sessions.get(worker) { return Ok(RecoveryWorker::Runnable(session.clone())); }
        let attached = self.page.client.call(None, &target::AttachToTargetParams {
            flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(worker.to_owned()))
        }).await?;
        let session = Session::sharing_clock(self.page.client.clone(), attached.session_id.0, self.page.service_clock());
        session.call(&cnet::EnableParams::default()).await?;
        self.worker_sessions.insert(worker.to_owned(), session.clone());
        Ok(RecoveryWorker::Runnable(session))
    }

    /// Continue external I/O once. A continued receipt is never turned into a new request,
    /// even when the previous recorder's proxy disappeared while the upstream was responding.
    async fn continue_blocking_request(&mut self, binding: &crate::bindings::Binding) -> Result<(), ChromiumError> {
        use crate::bindings::{BindingEntry, BindingJournal, Resolution};
        let bundle = Bundle::open(&self.identity.2, &self.identity.3)?;
        let path = bundle.root().join("interceptions.jsonl");
        let entries = BindingJournal::read(&path)?;
        let dispatched = entries.iter().any(|e| matches!(e, BindingEntry::Dispatched { ordinal, fetch_id, resolution: Resolution::Continue }
            if ordinal == &binding.ordinal && fetch_id == &binding.fetch_id));
        for entry in &entries {
            if let BindingEntry::RecordedResponse(recorded) = entry && recorded.ordinal == binding.ordinal {
                recorded.publish(bundle.root())?;
            }
        }
        self.read_blocking_inventory().await?;
        let _pause = self.clock_page().send(&emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause))?;
        if let Some(index) = self.pending.iter().position(|t| t.event.request_id.0 == binding.fetch_id
            && !super::interception::is_response_stage(&t.event)) {
            let event = self.pending.remove(index).event;
            if event.frontcap_blocks_main_thread != Some(true) || event.request.url != binding.url || event.request.method != binding.method {
                return Err(ChromiumError::Shape("external blocking request contradicts native inventory".into()));
            }
            if !dispatched {
                BindingJournal::open(&path)?.append(&BindingEntry::Dispatched { ordinal: binding.ordinal.clone(),
                    fetch_id: binding.fetch_id.clone(), resolution: Resolution::Continue })?;
            }
            super::interception::interception_fault_point(&format!("blocking_after_dispatch:{}", binding.ordinal));
            let headers = self.identity.1.proxy.as_ref().map(|_| {
                let mut headers: Vec<_> = super::adoption::paused_headers(&event).into_iter()
                    .filter(|(name, _)| !name.eq_ignore_ascii_case(frontcap_proxy_header()))
                    .map(|(name, value)| fetch::HeaderEntry { name, value }).collect();
                headers.push(fetch::HeaderEntry { name: frontcap_proxy_header().into(), value: binding.ordinal.clone() });
                headers
            });
            self.blocking_page().call(&fetch::ContinueRequestParams { headers,
                intercept_response: binding.recording.as_ref().map(|_| true),
                ..fetch::ContinueRequestParams::new(event.request_id) }).await?;
            super::interception::interception_fault_point(&format!("blocking_after_effect:{}", binding.ordinal));
        } else if !dispatched {
            return Err(ChromiumError::Shape("external blocking request disappeared before dispatch".into()));
        }
        loop {
            self.read_blocking_inventory().await?;
            if let Some(index) = self.pending.iter().position(|t| t.event.request_id.0 == binding.fetch_id
                && super::interception::is_response_stage(&t.event)) {
                let event = self.pending.remove(index).event;
                super::interception::interception_fault_point(&format!("blocking_before_capture:{}", binding.ordinal));
                self.capture_blocking_response(binding, &event).await?;
                super::interception::interception_fault_point(&format!("blocking_after_capture:{}", binding.ordinal));
                self.blocking_page().call(&fetch::ContinueRequestParams::new(event.request_id)).await?;
                super::interception::interception_fault_point(&format!("blocking_after_response:{}", binding.ordinal));
            }
            let receipts = self.blocking_page().call(&fetch::FrontcapReceiptsParams::default()).await?.receipts;
            let receipt = receipts.iter().find(|r| r.request_id.0 == binding.fetch_id)
                .ok_or_else(|| ChromiumError::Shape("continued blocking request has no native receipt".into()))?;
            if receipt.outcome != fetch::FrontcapReceiptOutcome::Continued {
                return Err(ChromiumError::Shape("continued blocking request has a contradictory native receipt".into()));
            }
            if receipt.completed == Some(true) {
                super::interception::interception_fault_point(&format!("blocking_after_completed:{}", binding.ordinal));
                return Ok(());
            }
            tokio::select! {
                event = self.paused.as_mut().expect("subscribed").next_tagged() => self.pending.push(event?),
                _ = tokio::time::sleep(Duration::from_millis(10)) => {},
            }
        }
    }

    async fn capture_blocking_response(&self, binding: &crate::bindings::Binding, event: &fetch::RequestPausedEvent) -> Result<(), ChromiumError> {
        use crate::bindings::{BindingEntry, BindingJournal, RecordedResponse};
        let (Some(plan), Some(status)) = (&binding.recording, event.response_status_code) else { return Ok(()) };
        let bundle = Bundle::open(&self.identity.2, &self.identity.3)?;
        let journal = bundle.root().join("interceptions.jsonl");
        for entry in BindingJournal::read(&journal)? {
            if let BindingEntry::RecordedResponse(recorded) = entry && recorded.ordinal == binding.ordinal {
                return recorded.publish(bundle.root());
            }
        }
        let body = match self.blocking_page().call(&fetch::GetResponseBodyParams::new(event.request_id.clone())).await {
            Ok(b) if b.base64_encoded => frontcap_core::encode::unbase64(&b.body),
            Ok(b) => Some(b.body.into_bytes()),
            Err(_) if matches!(status, 204 | 304) => Some(Vec::new()),
            Err(_) => None,
        }.ok_or_else(|| ChromiumError::Shape(format!("{}: no retained response body for blocking recording", binding.ordinal)))?;
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        for h in event.response_headers.iter().flatten() {
            headers.entry(h.name.clone()).and_modify(|v| { v.push('\n'); v.push_str(&h.value) }).or_insert_with(|| h.value.clone());
        }
        let path = bundle.root().join("cassettes").join(format!("{}.jsonl", plan.cassette));
        let prefix = match std::fs::read(&path) {
            Ok(bytes) => bytes, Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(), Err(e) => return Err(e.into()),
        };
        let recorded = RecordedResponse { ordinal: binding.ordinal.clone(), cassette: plan.cassette.clone(),
            offset: prefix.len(), prefix_sha256: frontcap_core::canonical::sha256_bytes_hex(&prefix),
            line: frontcap_core::cassette::Line { key: plan.key.clone(),
                response: frontcap_core::cassette::Response { status: status as u16, headers, body_base64: base64(&body) },
                latency: frontcap_core::cassette::Latency { virtual_ms: 0, wall_ms: 0 }, requires_progress: false } };
        BindingJournal::open(&journal)?.append(&BindingEntry::RecordedResponse(Box::new(recorded.clone())))?;
        super::interception::interception_fault_point(&format!("blocking_before_cassette:{}", binding.ordinal));
        recorded.publish(bundle.root())
    }

    async fn next_blocking_request(&mut self, previous: &str) -> Result<Option<crate::bindings::Binding>, ChromiumError> {
        use crate::bindings::{BindingEntry, BindingJournal};
        let bundle = Bundle::open(&self.identity.2, &self.identity.3)?;
        let path = bundle.root().join("interceptions.jsonl");
        let entries = BindingJournal::read(&path)?;
        let predecessor = entries.iter().find_map(|entry| match entry {
            BindingEntry::Bound(b) if b.ordinal == previous => Some(b.as_ref()), _ => None,
        }).ok_or_else(|| ChromiumError::Shape("consecutive request has no predecessor binding".into()))?;
        // A prior recovery may already have completed held cleanup, so no new answer selected
        // its target on this connection. The durable predecessor still fixes the session.
        self.select_blocking_target(&predecessor.target_id)?;
        let successors: Vec<_> = entries.iter().filter_map(|e| match e {
            BindingEntry::Bound(b) if b.continuation.as_deref() == Some(previous) => Some(b.as_ref()), _ => None,
        }).collect();
        if let Some(binding) = successors.first() {
            if successors.iter().any(|b| *b != *binding) {
                return Err(ChromiumError::Shape("blocking continuation has contradictory bindings".into()));
            }
            self.collect_bound_prefix(&entries, previous, binding).await?;
            self.select_blocking_target(&binding.target_id)?;
            return Ok(Some((*binding).clone()));
        }
        self.read_blocking_inventory().await?;
        if !self.pending.iter().any(|t| t.event.frontcap_blocks_main_thread == Some(true)) {
            let pause = self.clock_page().send(&emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause))?;
            let settled = async {
                let answer = pause.answered();
                tokio::pin!(answer);
                loop {
                    tokio::select! {
                        result = &mut answer => { result?; return Ok::<_, ChromiumError>(()); },
                        event = self.paused.as_mut().expect("subscribed").next_tagged() => {
                            let event = event?;
                            let blocking = event.event.frontcap_blocks_main_thread == Some(true);
                            self.pending.push(event);
                            if blocking { return Ok(()); }
                        }
                    }
                }
            };
            tokio::time::timeout(Session::CALL_TIMEOUT, settled).await
                .map_err(|_| ChromiumError::CallTimeout("consecutive request recovery pause"))??;
        }
        let Some(event) = self.pending.iter().find(|t| t.event.frontcap_blocks_main_thread == Some(true)).map(|t| t.event.clone()) else { return Ok(None) };
        let previous = predecessor;
        let already_bound = entries.iter().any(|entry| matches!(entry, BindingEntry::Bound(b) if b.fetch_id == event.request_id.0));
        if !already_bound {
            let (owner, first) = previous.network_id.as_deref().and_then(renderer_request_number)
                .ok_or_else(|| ChromiumError::Unsupported("blocking recovery has no predecessor renderer identity".into()))?;
            let (next_owner, last) = event.network_id.as_ref().and_then(|id| renderer_request_number(&id.0))
                .ok_or_else(|| ChromiumError::Unsupported("blocking recovery has no successor renderer identity".into()))?;
            if owner != next_owner || last <= first || last - first > 64 {
                return Err(ChromiumError::Unsupported("blocking recovery cannot establish a bounded renderer request prefix".into()));
            }
            // The paused synchronous request proves this task reached `last`. Fetch can publish an
            // earlier request late; wait for every identity in the interval before consuming a rule.
            loop {
                self.read_blocking_inventory().await?;
                let complete = (first + 1..=last).all(|number| self.pending.iter().any(|t|
                    t.event.network_id.as_ref().and_then(|id| renderer_request_number(&id.0)) == Some((owner, number))));
                if complete { break; }
                self.pending.push(self.paused.as_mut().expect("subscribed").next_tagged().await?);
            }
            let bound: Vec<_> = entries.iter().filter_map(|entry| match entry { BindingEntry::Bound(b) => Some(b.as_ref()), _ => None }).collect();
            if self.pending.iter().any(|t| t.event.frontcap_worker_id.is_none()
                && !bound.iter().any(|b| b.fetch_id == t.event.request_id.0)
                && !t.event.network_id.as_ref().and_then(|id| renderer_request_number(&id.0))
                    .is_some_and(|(issuer, number)| issuer == owner && number > first && number <= last)) {
                return Err(ChromiumError::Unsupported("blocking recovery cannot skip unselected sibling requests outside its renderer prefix".into()));
            }
            let mut prefix: Vec<_> = self.pending.iter().filter(|t| t.event.network_id.as_ref()
                .and_then(|id| renderer_request_number(&id.0))
                .is_some_and(|(issuer, number)| issuer == owner && number > first && number < last))
                .map(|t| t.event.clone()).collect();
            prefix.sort_by_key(|e| e.network_id.as_ref().and_then(|id| renderer_request_number(&id.0)).map(|(_, number)| number));
            // Document requests in one renderer share this complete issue-order interval, even
            // when a same-origin call enters another frame. Workers use a separate issuer order.
            if prefix.iter().any(|e| e.frontcap_blocks_main_thread == Some(true)) {
                return Err(ChromiumError::Unsupported("blocking recovery found another synchronous blocker in its prefix".into()));
            }
            for preceding in prefix.into_iter().filter(|e| e.frontcap_worker_id.is_none()) {
                // Ordinary answers stay in native inventory until renderer initialization. Their
                // bindings reserve sequence/cassette positions even if this recorder dies next.
                self.bind_recovery_request(&bundle, Some(previous), &preceding, false, previous.issued_during_progress)?;
            }
        }
        let binding = self.bind_recovery_request(&bundle, Some(previous), &event, true, previous.issued_during_progress)?;
        self.select_blocking_target(&binding.target_id)?;
        Ok(Some(binding))
    }

    /// A prior recovery may have bound the entire prefix but died before delivery. Native
    /// inventory replay can itself be delayed: absence from this client's queue is not completion.
    async fn collect_bound_prefix(&mut self, entries: &[crate::bindings::BindingEntry], previous: &str,
        successor: &crate::bindings::Binding) -> Result<(), ChromiumError>
    {
        use crate::bindings::BindingEntry;
        let previous = entries.iter().find_map(|e| match e {
            BindingEntry::Bound(b) if b.ordinal == previous => Some(b.as_ref()), _ => None,
        }).ok_or_else(|| ChromiumError::Shape("bound continuation has no predecessor".into()))?;
        let (owner, first) = previous.network_id.as_deref().and_then(renderer_request_number)
            .ok_or_else(|| ChromiumError::Shape("bound continuation has no renderer prefix".into()))?;
        let (next_owner, last) = successor.network_id.as_deref().and_then(renderer_request_number)
            .ok_or_else(|| ChromiumError::Shape("bound continuation has no successor renderer identity".into()))?;
        if owner != next_owner || last <= first || last - first > 64 {
            return Err(ChromiumError::Shape("bound continuation contradicts its renderer prefix".into()));
        }
        let required: Vec<_> = entries.iter().filter_map(|e| match e {
            BindingEntry::Bound(b) if (b.target_id == self.identity.1.target_id || self.popup_targets.contains(&b.target_id))
                && b.network_id.as_deref().and_then(renderer_request_number)
                    .is_some_and(|(issuer, number)| issuer == owner && number > first && number <= last) => Some(b.as_ref()),
            _ => None,
        }).collect();
        loop {
            self.read_blocking_inventory().await?;
            let mut receipts = Vec::new();
            for (target, page) in &self.fetch_pages {
                receipts.extend(page.call(&fetch::FrontcapReceiptsParams::default()).await?.receipts
                    .into_iter().map(|r| (target, r)));
            }
            // Workers do not consume this document task's sequence slots, but their native
            // identities can occupy the interval. A retry must collect those unbound pauses too.
            if (first + 1..=last).all(|number| self.pending.iter().any(|t|
                t.event.network_id.as_ref().and_then(|id| renderer_request_number(&id.0)) == Some((owner, number)))
                || required.iter().any(|b| b.network_id.as_deref().and_then(renderer_request_number) == Some((owner, number))
                    && receipts.iter().any(|(target, r)| **target == b.target_id && r.request_id.0 == b.fetch_id))) { return Ok(()) }
            self.pending.push(self.paused.as_mut().expect("subscribed").next_tagged().await?);
        }
    }

    fn bind_recovery_request(&self, bundle: &Bundle, previous: Option<&crate::bindings::Binding>,
        event: &fetch::RequestPausedEvent, blocking: bool, progressing: bool) -> Result<crate::bindings::Binding, ChromiumError>
    {
        use crate::bindings::{BindingEntry, BindingJournal, Decision};
        let path = bundle.root().join("interceptions.jsonl");
        let entries = BindingJournal::read(&path)?;
        let bound: Vec<_> = entries.iter().filter_map(|entry| match entry { BindingEntry::Bound(b) => Some(b.as_ref()), _ => None }).collect();
        let target_id = self.target_of_pause(event)?;
        if let Some(binding) = bound.iter().find(|b| b.fetch_id == event.request_id.0) {
            if previous.is_some_and(|p| binding.ordinal == p.ordinal) || binding.target_id != target_id
                || binding.url != event.request.url || binding.method != event.request.method {
                return Err(ChromiumError::Shape("consecutive request contradicts its retained binding".into()));
            }
            return Ok((*binding).clone());
        }
        let events = BehaviorLog::read(&bundle.trace())?;
        if event.resource_type == cnet::ResourceType::Document
            || event.request.method == "OPTIONS" || event.redirected_request_id.is_some() {
            return Err(ChromiumError::Unsupported("consecutive synchronous recovery does not yet admit documents, preflights or redirects".into()));
        }
        let issued_us = event.frontcap_issue_time_us.as_deref().and_then(|s| s.parse::<u64>().ok())
            .and_then(|us| us.checked_sub(self.driver.dialogs.initial_date_ms.saturating_mul(1000)))
            .ok_or_else(|| ChromiumError::Shape("consecutive request has no native issue instant".into()))?;
        if let Some(previous) = previous {
            let previous_answer = match previous.decision { Decision::Deliver { due_us, .. } => due_us, _ => previous.issued_us };
            if issued_us != previous_answer {
                return Err(ChromiumError::Unsupported("consecutive synchronous request changed the expected answer instant".into()));
            }
        }
        // Existing trace requests include streams; the binding ledger also includes requests
        // a previous recorder selected but never published to the trace.
        let max_trace = events.iter().filter_map(|e| match &e.kind { EventKind::Request(r) => Some(super::adoption::ordinal_number(&r.id)), _ => None }).max().unwrap_or(0);
        let max_bound = bound.iter().map(|b| super::adoption::ordinal_number(&b.ordinal)).max().unwrap_or(0);
        if entries.iter().any(|e| matches!(e, BindingEntry::StreamSelected(_) | BindingEntry::StreamHeld { .. } | BindingEntry::StreamOpening { .. }
            | BindingEntry::StreamClosed(_) | BindingEntry::StreamAbandoned { .. } | BindingEntry::StreamConnecting(_)
            | BindingEntry::StreamReplay(_) | BindingEntry::StreamSent { .. })) {
            return Err(ChromiumError::Unsupported("consecutive synchronous recovery with stream admission is not qualified".into()));
        }
        let headers = super::adoption::paused_headers(event);
        let ordinal = format!("req-{}", max_trace.max(max_bound).checked_add(1).ok_or_else(|| ChromiumError::Shape("request ordinal overflow".into()))?);
        let mut net = super::lifecycle::recovery_network(bundle, &entries)?;
        let selected = super::interception::select_request_decision(&mut net, event, VirtualTime(issued_us), false,
            progressing, &ordinal)?;
        if !matches!(selected.decision, Decision::Hold | Decision::Deliver { .. } | Decision::Fail { .. } | Decision::Passthrough) {
            return Err(ChromiumError::Unsupported("pre-renderer recovery cannot admit an interception stop".into()));
        }
        let document = events.iter().filter_map(|e| match &e.kind {
            EventKind::Navigation(n) if n.context.is_none() => Some(n.document), _ => None,
        }).max().unwrap_or(0);
        let recording = net.recording.remove(&ordinal).map(|(cassette, key, _, _)| crate::bindings::RecordingPlan { cassette, key });
        let recovered_worker_owners = super::worker_lineage::recovered_owners(&entries)?;
        let binding = crate::bindings::Binding {
            blocks_main_thread: event.frontcap_blocks_main_thread == Some(true),
            ordinal, target_id, fetch_id: event.request_id.0.clone(),
            network_id: event.network_id.as_ref().map(|id| id.0.clone()),
            worker_lineage: event.network_id.as_ref().and_then(|id| recovered_worker_owners.get(&id.0).cloned()),
            document: previous.map_or(document, |p| p.document), issued_us, issued_during_progress: progressing,
            rules_revision: net.rules_revision, rule: selected.rule, entry: selected.entry,
            decision: if self.interception_armed && event.frontcap_blocks_main_thread != Some(true) {
                Decision::Intercept { then: Box::new(selected.decision) }
            } else { selected.decision }, selection_taint: selected.taint,
            continuation: previous.filter(|_| blocking).map(|p| p.ordinal.clone()),
            frame_id: Some(event.frame_id.0.clone()), recording,
            url: event.request.url.clone(), method: event.request.method.clone(),
            body: RequestBody::of(event.request.post_data.as_deref(), event.request.has_post_data),
            origin: headers.iter().find(|(key, _)| key.eq_ignore_ascii_case("origin")).map(|(_, value)| value.clone()),
            document_request: false,
            navigation: None,
        };
        BindingJournal::open(&path)?.append(&BindingEntry::Bound(Box::new(binding.clone())))?;
        let point = if blocking { "blocking_after_binding" } else { "blocking_after_prefix_binding" };
        super::interception::interception_fault_point(&format!("{point}:{}", binding.ordinal));
        Ok(binding)
    }

    /// The recorder's durable suspension fixes the instant at which its blocking hold is answered.
    pub fn preserve_blocking_answer_time(&mut self, ordinal: String, at: VirtualTime) {
        self.blocking_answer_time.insert(ordinal, at);
    }

    /// Restore an attempted navigation whose retained beforeunload answer proves cancellation.
    /// Queue its action before adopted dialog evidence, without dispatching navigation again.
    pub fn preserve_cancelled_navigation(&mut self, request: RequestId, action: ActAction, t_us: VirtualTime) -> Result<(), ChromiumError> {
        if action.kind() != frontcap_core::action::ActionKind::Navigation {
            return Err(ChromiumError::InvalidArgument("a cancelled navigation requires a navigation action".into()));
        }
        self.cancelled_navigation = Some((request, action, t_us));
        Ok(())
    }

    pub fn dialogs(&self) -> DialogSession { self.driver.dialogs.clone() }

    pub(super) fn rules(&self) -> Arc<Mutex<DialogRules>> { self.driver.dialogs.rules.clone() }

    pub(super) fn bindings(&self) -> Arc<Mutex<DialogBindings>> { self.driver.dialogs.bindings.clone() }

    pub(super) fn matches(&self, o: &ReconnectOptions) -> bool {
        self.identity == (o.launch_id.clone(), o.endpoint.clone(), o.runs_root.clone(), o.run.clone())
            && self.driver.dialogs.initial_date_ms == o.profile.initial_date_ms
            && *self.driver.dialogs.rules.lock().unwrap() == o.dialog_rules
    }

    pub(super) async fn into_parts(mut self) -> Result<(Session, Driver, Vec<(String, String)>), ChromiumError> {
        if !self.fetch_pages.is_empty() { self.read_blocking_inventory().await?; }
        let inventory = self.pending.iter().map(|t|
            self.target_of_pause(&t.event).map(|target| (target, t.event.request_id.0.clone())))
            .collect::<Result<Vec<_>, _>>()?;
        // Detach every temporary consumer. Normal enrollment replays the native inventory on its
        // own sessions, including notifications that were delayed on the recovery connection.
        for page in self.fetch_pages.into_values() {
            self.page.client.call(None, &target::DetachFromTargetParams {
                session_id: Some(target::SessionID(page.id)), ..Default::default()
            }).await?;
        }
        Ok((self.page, self.driver, inventory))
    }

    pub(super) async fn attach(page: Session, rules: Arc<Mutex<DialogRules>>, bindings: Arc<Mutex<DialogBindings>>, o: &ReconnectOptions, popups: Vec<(String, String)>) -> Result<Self, ChromiumError> {
        let driver = Driver::attach(&page, rules, bindings, o.profile.initial_date_ms).await?;
        // The open windows too: a dialog any of them holds blocks the launch page's commands as well.
        let popup_targets = popups.iter().map(|(_, target)| target.clone()).collect();
        for (context, target) in popups {
            let session = page.client.call(None, &target::AttachToTargetParams {
                flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(target))
            }).await?.session_id.0;
            driver.dialogs.add_context(context, page.client.caller(), &session, page.service_clock()).await?;
        }
        Ok(Self { #[cfg(feature = "test-hooks")] interrupt_response_at: None,
            #[cfg(feature = "fault-injection")]
            arrival_report: (std::env::var("FRONTCAP_ARRIVAL_REPORT_DELAY").ok().and_then(|ms| ms.parse().ok()).map(Duration::from_millis), None),
            page, fetch_pages: BTreeMap::new(), worker_sessions: BTreeMap::new(), shared_hold: None, blocking_target: o.endpoint.target_id.clone(), interception_armed: false, popup_targets, driver, identity: (o.launch_id.clone(), o.endpoint.clone(), o.runs_root.clone(), o.run.clone()), cancelled_navigation: None, blocking_answer_time: BTreeMap::new(), paused: None, pending: Vec::new() })
    }
}

impl DialogSession {
    /// The owner retired this session, including before replacing a pristine target. Recovery must
    /// stop querying it; a replacement target gets its own driver before scripts can run.
    pub fn is_retired(&self) -> bool { self.retired.load(std::sync::atomic::Ordering::SeqCst) }

    /// The served pages, the launch page first.
    fn pages(&self) -> Vec<(Option<String>, DialogControl)> {
        self.pages.lock().unwrap().iter().map(|p| (p.context.clone(), p.control.clone())).collect()
    }

    /// The launch page's control.
    pub(super) fn launch_control(&self) -> DialogControl {
        self.pages.lock().unwrap()[0].control.clone()
    }

    /// Serves a popup's dialogs from now on, by the same rules and bindings. Its control parks `clock`,
    /// the generation's, while one of its dialogs is open.
    pub(super) async fn add_context(&self, context: String, caller: cdp::client::Caller, session: &str, clock: crate::service_clock::ServiceClock) -> Result<(), ChromiumError> {
        let control = DialogControl::attach_with(caller, session, clock).await?;
        let task = self.serve(control.clone(), Some(context.clone()));
        {
            // A popup served again (by a reconnecting recorder's own session) replaces its earlier service.
            let mut pages = self.pages.lock().unwrap();
            if let Some(at) = pages.iter().position(|p| p.context.as_deref() == Some(context.as_str())) {
                pages.remove(at).task.abort();
            }
            pages.push(Served { context: Some(context), control, task });
        }
        self.changed.send_modify(|n| *n += 1);
        Ok(())
    }

    /// Stops serving a popup that closed: its browser keeps neither dialogs nor receipts.
    pub(super) fn remove_context(&self, context: &str) {
        let mut pages = self.pages.lock().unwrap();
        if let Some(at) = pages.iter().position(|p| p.context.as_deref() == Some(context)) {
            pages.remove(at).task.abort();
        }
        drop(pages);
        self.changed.send_modify(|n| *n += 1);
    }

    /// Answers, as they open, the dialogs of `control`'s page that the rules answer; a dialog no rule
    /// answers (a `hold`) waits for the agent.
    fn serve(&self, control: DialogControl, context: Option<String>) -> tokio::task::JoinHandle<()> {
        let mut updates = control.subscribe();
        let answers = self.clone();
        tokio::spawn(async move {
            loop {
                let inventory = match updates.borrow_and_update().clone() {
                    Ok(inventory) => inventory,
                    Err(_) => {
                        answers.changed.send_modify(|n| *n += 1);
                        break;
                    }
                };
                if let Err(error) = answers.bind_inventory(&inventory) {
                    control.report_fault(error.to_string());
                    return;
                }
                for dialog in inventory.dialogs {
                    let Some(reply) = answers.rules.lock().unwrap().answer(kind(dialog.r#type), &dialog.message, Some(&dialog.default_prompt)) else { continue };
                    let (accept, prompt_text) = match reply {
                        DialogReply::Alert => (true, None),
                        DialogReply::Confirm(yes) | DialogReply::Beforeunload(yes) => (yes, None),
                        DialogReply::Prompt(text) => (text.is_some(), text),
                    };
                    let request_id = format!("native-auto-{}", frontcap_core::canonical::sha256_hex(&dialog.dialog_id));
                    let params = page::FrontcapAnswerDialogParams {
                        prompt_text, ..page::FrontcapAnswerDialogParams::new(dialog.dialog_id, request_id, accept)
                    };
                    match control.answer(&params).await {
                        Ok(receipt) => answers.given.lock().unwrap().push((context.clone(), receipt)),
                        Err(error) => {
                            control.report_fault(error.to_string());
                            return;
                        }
                    }
                }
                answers.changed.send_modify(|n| *n += 1);
                if updates.changed().await.is_err() { break }
            }
        })
    }

    /// The receipts `context`'s page holds or, once it is gone, those of the answers given there.
    pub(super) fn receipts_of(&self, context: Option<&str>) -> Vec<page::FrontcapDialogReceipt> {
        let live = self.pages().into_iter().find(|(c, _)| c.as_deref() == context).map(|(_, control)| control);
        match live.map(|control| control.subscribe().borrow().clone()) {
            Some(Ok(inventory)) => inventory.receipts,
            _ => self.given.lock().unwrap().iter().filter(|(c, _)| c.as_deref() == context).map(|(_, r)| r.clone()).collect(),
        }
    }

    fn bind_inventory(&self, inventory: &crate::dialog_control::DialogInventory) -> Result<(), ChromiumError> {
        let mut bindings = self.bindings.lock().unwrap();
        // Already answered dialogs precede a currently pending one, including after reconnect.
        for receipt in &inventory.receipts { bindings.bind(&receipt.dialog.dialog_id)?; }
        for dialog in &inventory.dialogs { bindings.bind(&dialog.dialog_id)?; }
        Ok(())
    }

    fn describe(&self, dialog: &page::FrontcapDialog, context: Option<String>) -> Result<frontcap_core::dialog::DialogDescriptor, ChromiumError> {
        let epoch = dialog.unix_time_us.as_deref().and_then(|s| s.parse::<i128>().ok())
            .ok_or_else(|| ChromiumError::Shape("native dialog has no valid virtual timestamp".into()))?;
        let elapsed = epoch - i128::from(self.initial_date_ms) * 1000;
        let t_us = VirtualTime(u64::try_from(elapsed).map_err(|_| ChromiumError::Shape("native dialog lies outside the generation timeline".into()))?);
        let frame_path = dialog.frame_path.iter().map(|index| u32::try_from(*index)
            .map_err(|_| ChromiumError::Shape("native dialog has an invalid frame path".into())))
            .collect::<Result<Vec<_>, _>>()?;
        if dialog.frame_urls.len() != frame_path.len() + 1 || dialog.frame_urls.last() != Some(&dialog.url) {
            return Err(ChromiumError::Shape("native dialog has inconsistent source ancestry".into()));
        }
        Ok(frontcap_core::dialog::DialogDescriptor {
            id: self.bindings.lock().unwrap().bind(&dialog.dialog_id)?, kind: kind(dialog.r#type),
            message: dialog.message.clone(), default_prompt: dialog.default_prompt.clone(),
            url: dialog.url.clone(), frame: dialog.frame_id.clone(),
            source: frontcap_core::dialog::DialogSource { frame_path, frame_urls: dialog.frame_urls.clone() }, t_us, context,
        })
    }

    /// The binding is durable before a pending logical dialog is exposed to a controller.
    pub fn pending(&self) -> Result<Option<frontcap_core::dialog::DialogDescriptor>, ChromiumError> {
        for (context, control) in self.pages() {
            let inventory = control.subscribe().borrow().clone().map_err(ChromiumError::Shape)?;
            self.bind_inventory(&inventory)?;
            for dialog in inventory.dialogs {
                if self.rules.lock().unwrap().answer(kind(dialog.r#type), &dialog.message, Some(&dialog.default_prompt)).is_none() {
                    return self.describe(&dialog, context).map(Some);
                }
            }
        }
        Ok(None)
    }

    /// Recovery can query before Page.enable, when page-domain event delivery is not enabled yet.
    pub async fn refresh_pending(&self) -> Result<Option<frontcap_core::dialog::DialogDescriptor>, ChromiumError> {
        for (_, control) in self.pages() { control.inspect().await?; }
        self.pending()
    }

    pub async fn wait_pending(&self) -> Result<frontcap_core::dialog::DialogDescriptor, ChromiumError> {
        let mut changed = self.changed.subscribe();
        loop {
            changed.borrow_and_update();
            if let Some(dialog) = self.pending()? { return Ok(dialog) }
            changed.changed().await.map_err(|e| ChromiumError::Shape(e.to_string()))?;
        }
    }

    fn receipt(&self, receipt: page::FrontcapDialogReceipt, request: &RequestId, context: Option<String>) -> Result<frontcap_core::dialog::DialogReceipt, ChromiumError> {
        Ok(frontcap_core::dialog::DialogReceipt {
            dialog: self.describe(&receipt.dialog, context)?, request_id: request.clone(), accept: receipt.accept,
            prompt_text: receipt.prompt_text,
            source: match receipt.source {
                page::FrontcapDialogReceiptSource::Answered => NativeDialogSource::Answered,
                page::FrontcapDialogReceiptSource::Cancelled => NativeDialogSource::Cancelled,
                page::FrontcapDialogReceiptSource::External => NativeDialogSource::External,
            },
        })
    }

    pub async fn answer(&self, dialog: &str, request: &RequestId, accept: bool, prompt_text: Option<String>) -> Result<frontcap_core::dialog::DialogReceipt, ChromiumError> {
        let native = self.bindings.lock().unwrap().native_id(dialog).map_err(|_| ChromiumError::NoSuchHold(dialog.into()))?;
        // The page that holds it, or the launch page, whose engine refuses a dialog it does not know.
        let pages = self.pages();
        let held = |inventory: &crate::dialog_control::DialogUpdate| inventory.as_ref().is_ok_and(|i| i.dialogs.iter().any(|d| d.dialog_id == native));
        let (context, control) = pages.iter().find(|(_, control)| held(&control.subscribe().borrow())).unwrap_or(&pages[0]).clone();
        let receipt = control.answer(&page::FrontcapAnswerDialogParams {
            prompt_text, ..page::FrontcapAnswerDialogParams::new(native, native_answer_request(request), accept)
        }).await?;
        self.given.lock().unwrap().push((context.clone(), receipt.clone()));
        self.receipt(receipt, request, context)
    }

    /// Reconcile a journaled dependent request without submitting it a second time.
    pub async fn saved_answer(&self, request: &RequestId) -> Result<Option<frontcap_core::dialog::DialogReceipt>, ChromiumError> {
        for (context, control) in self.pages() {
            let inventory = control.inspect().await?;
            self.bind_inventory(&inventory)?;
            if let Some(r) = inventory.receipts.into_iter().find(|r| r.request_id.as_deref() == Some(native_answer_request(request).as_str())) {
                return self.receipt(r, request, context).map(Some);
            }
        }
        Ok(None)
    }

    /// The dependent journal has durably saved its terminal answer. The generation still requires
    /// the corresponding trace record before it can forget the native receipt.
    pub fn acknowledge(&self, request: &RequestId) {
        self.acknowledged.lock().unwrap().insert(native_answer_request(request));
    }

    fn may_forget(&self, receipt: &page::FrontcapDialogReceipt) -> bool {
        match receipt.request_id.as_deref() {
            None => true,
            Some(id) => id == format!("native-auto-{}", frontcap_core::canonical::sha256_hex(&receipt.dialog.dialog_id))
                || self.acknowledged.lock().unwrap().contains(id),
        }
    }
}

impl Driver {
    pub(super) fn retire(&self) {
        self.dialogs.retired.store(true, std::sync::atomic::Ordering::SeqCst);
        for page in self.dialogs.pages.lock().unwrap().iter() { page.task.abort(); }
    }

    pub async fn attach(session: &Session, rules: Arc<Mutex<DialogRules>>, bindings: Arc<Mutex<DialogBindings>>, initial_date_ms: u64) -> Result<Self, ChromiumError> {
        let control = DialogControl::attach(session).await?;
        let dialogs = DialogSession {
            pages: Default::default(), changed: Arc::new(tokio::sync::watch::channel(0).0), given: Default::default(),
            rules, bindings, initial_date_ms, acknowledged: Default::default(), retired: Default::default(),
        };
        let task = dialogs.serve(control.clone(), None);
        dialogs.pages.lock().unwrap().push(Served { context: None, control, task });
        Ok(Self { dialogs })
    }
}

impl Generation {
    /// The browser-side native handle is independent of a running operation's mutable generation.
    pub fn native_dialog_control(&self) -> Option<DialogControl> {
        self.native_dialog_driver.as_ref().map(|driver| driver.dialogs.launch_control())
    }

    pub fn dialog_session(&self) -> Option<DialogSession> {
        self.native_dialog_driver.as_ref().map(|driver| driver.dialogs.clone())
    }

    /// The pages' inventories, the launch page first; a popup closed meanwhile has none.
    async fn dialog_inventories(&self) -> Result<Vec<(Option<String>, DialogControl, crate::dialog_control::DialogInventory)>, ChromiumError> {
        let Some(dialogs) = self.dialog_session() else { return Ok(Vec::new()) };
        let mut out = Vec::new();
        for (context, control) in dialogs.pages() {
            if let Err(reason) = control.subscribe().borrow().clone() {
                if context.is_some() && control.is_gone() { continue }
                return Err(ChromiumError::Shape(format!("native dialog control failed: {reason}")));
            }
            match control.inspect().await {
                Ok(inventory) => out.push((context, control, inventory)),
                Err(_) if context.is_some() && control.is_gone() => {}
                Err(error) => return Err(error),
            }
        }
        Ok(out)
    }

    pub(super) async fn drain_native_dialog_receipts(&mut self) -> Result<(), ChromiumError> {
        let inventories = self.dialog_inventories().await?;
        let held = inventories.iter().any(|(_, _, inventory)| !inventory.dialogs.is_empty());
        let live: Vec<Option<String>> = inventories.iter().map(|(context, _, _)| context.clone()).collect();
        // A page gone since its answers were given took its receipts along: those answers are recorded
        // from the receipts the answers returned.
        let gone: Vec<Given> = match self.dialog_session() {
            Some(dialogs) => {
                let mut given = dialogs.given.lock().unwrap();
                let (gone, kept) = std::mem::take(&mut *given).into_iter().partition(|(context, _)| !live.contains(context));
                *given = kept;
                gone
            }
            None => Vec::new(),
        };
        let receipts = inventories.into_iter().flat_map(|(context, _, inventory)| inventory.receipts.into_iter().map(move |r| (context.clone(), r)));
        for (context, receipt) in receipts.chain(gone) {
            let native_id = &receipt.dialog.dialog_id;
            let id = self.dialog_bindings.lock().unwrap().bind(native_id)?;
            if self.recorded_native_dialogs.contains(&id) {
                self.native_dialog_receipts_ready.insert(native_id.clone());
                continue;
            }
            if self.pending.iter().any(|(_, event)| matches!(event.recorded(), Some(EventKind::Dialog(d)) if d.native.as_ref().is_some_and(|n| n.id == id))) {
                continue;
            }
            let epoch = receipt.dialog.unix_time_us.as_deref().and_then(|s| s.parse::<i128>().ok())
                .ok_or_else(|| ChromiumError::Shape("native dialog has no valid virtual timestamp".into()))?;
            let elapsed = epoch - i128::from(self.profile.initial_date_ms) * 1000;
            let t = VirtualTime(u64::try_from(elapsed).map_err(|_| ChromiumError::Shape("native dialog lies outside the generation timeline".into()))?);
            if t < self.log.last_time() {
                return Err(ChromiumError::Shape("unrecorded native dialog predates the committed trace".into()));
            }
            let kind = kind(receipt.dialog.r#type);
            let reply = match kind {
                DialogKind::Alert => DialogReply::Alert,
                DialogKind::Confirm => DialogReply::Confirm(receipt.accept),
                DialogKind::Prompt => DialogReply::Prompt(receipt.accept.then_some(receipt.prompt_text)),
                DialogKind::Beforeunload => DialogReply::Beforeunload(receipt.accept),
            };
            let source = match receipt.source {
                page::FrontcapDialogReceiptSource::Answered => NativeDialogSource::Answered,
                page::FrontcapDialogReceiptSource::Cancelled => NativeDialogSource::Cancelled,
                page::FrontcapDialogReceiptSource::External => NativeDialogSource::External,
            };
            let mut record = DialogRecord {
                kind, message: receipt.dialog.message, answer: reply.recorded(),
                native: Some(NativeDialogEvidence { id, source }), observer: None, context,
            };
            // The observer establishes call order; only the browser receipt establishes the answer.
            // Match the earliest corresponding opening (including repeated identical calls).
            if let Some((_, event)) = self.pending.iter_mut().find(|(at, event)| {
                *at == t && matches!(event, PendingEvent::NativeDialogOpening(d) if d.kind == kind && d.message == record.message && d.context == record.context)
            }) {
                let PendingEvent::NativeDialogOpening(opening) = event else { unreachable!() };
                record.observer = opening.observer.clone();
                *event = PendingEvent::Recorded(Box::new(EventKind::Dialog(record)));
            } else {
                self.record(t, EventKind::Dialog(record));
            }
        }
        if !held && self.pending.iter().any(|(_, e)| matches!(e, PendingEvent::NativeDialogOpening(_))) {
            return Err(ChromiumError::Shape("native dialog opening has no retained answer receipt".into()));
        }
        // A given answer already in the trace needs no keeping.
        if let Some(dialogs) = self.dialog_session() {
            let bindings = self.dialog_bindings.lock().unwrap();
            let written = |native: &str| bindings.ordinal(native).is_some_and(|id| self.recorded_native_dialogs.contains(&id));
            dialogs.given.lock().unwrap().retain(|(_, r)| !written(&r.dialog.dialog_id));
        }
        Ok(())
    }

    pub(super) async fn forget_native_dialog_receipts(&mut self) {
        if self.native_dialog_receipts_ready.is_empty() { return }
        // Sync also covers complete trace lines loaded on reconnect before forgetting old receipts.
        if self.log.sync().is_err() { return }
        let Some(dialogs) = self.dialog_session() else { return };
        let Ok(inventories) = self.dialog_inventories().await else { return };
        for (_, control, inventory) in inventories {
            let ids: Vec<_> = inventory.receipts.iter().filter(|r| self.native_dialog_receipts_ready.contains(&r.dialog.dialog_id) && dialogs.may_forget(r))
                .map(|r| r.dialog.dialog_id.clone()).collect();
            if ids.is_empty() { continue }
            if control.forget(ids.clone()).await.is_ok() {
                for id in ids { self.native_dialog_receipts_ready.remove(&id); }
            }
        }
    }
}


/// Recovery metadata binds engine tokens to stable trace ordinals before the trace can refer to them.
#[derive(serde::Serialize, serde::Deserialize)]
struct DialogBinding {
    id: String,
    native_id: String,
}

pub(super) struct DialogBindings {
    out: frontcap_core::lines::Appender<std::fs::File>,
    by_native: BTreeMap<String, String>,
    by_ordinal: BTreeMap<String, String>,
    dirty: bool,
}

impl DialogBindings {
    pub fn open(path: &std::path::Path) -> Result<Self, ChromiumError> {
        let opened = frontcap_core::lines::open_append(path).map_err(|e| ChromiumError::Shape(e.to_string()))?;
        let mut by_native = BTreeMap::new();
        let mut by_ordinal = BTreeMap::new();
        for (index, line) in opened.lines.iter().enumerate() {
            let binding: DialogBinding = serde_json::from_str(line).map_err(|e| ChromiumError::Shape(format!("{} line {}: {e}", path.display(), index + 1)))?;
            if binding.id != format!("dialog-{}", index + 1) || by_native.contains_key(&binding.native_id) {
                return Err(ChromiumError::Shape(format!("{} line {}: conflicting native dialog binding", path.display(), index + 1)));
            }
            by_native.insert(binding.native_id.clone(), binding.id.clone());
            by_ordinal.insert(binding.id, binding.native_id);
        }
        let mut out = frontcap_core::lines::Appender::new(opened.file, opened.len);
        out.sync()?;
        Ok(Self { out, by_native, by_ordinal, dirty: false })
    }

    pub fn bind(&mut self, native_id: &str) -> Result<String, ChromiumError> {
        if !self.by_native.contains_key(native_id) {
            let binding = DialogBinding { id: format!("dialog-{}", self.by_native.len() + 1), native_id: native_id.into() };
            self.out.append_line(&frontcap_core::canonical::canonical_of(&binding))?;
            self.by_native.insert(binding.native_id.clone(), binding.id.clone());
            self.by_ordinal.insert(binding.id, binding.native_id);
            self.dirty = true;
        }
        if self.dirty {
            self.out.sync()?;
            self.dirty = false;
        }
        Ok(self.by_native[native_id].clone())
    }

    /// The ordinal of a native dialog already bound.
    pub fn ordinal(&self, native_id: &str) -> Option<String> {
        self.by_native.get(native_id).cloned()
    }

    pub fn native_id(&self, ordinal: &str) -> Result<String, ChromiumError> {
        self.by_ordinal.get(ordinal).cloned().ok_or_else(|| ChromiumError::Shape(format!("native dialog {ordinal} has no durable binding")))
    }
}

#[cfg(test)]
#[path = "native_dialogs_tests.rs"]
mod tests;
