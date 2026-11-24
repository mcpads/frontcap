//! Shared native time. Page additions are admitted at held policy boundaries.
//! Normal page closure requires native retirement proof; adoption reconstructs held membership.
//! Controller recovery retains native membership and explicit publication holds.
use super::*;
use super::progress::PolicyOutcome;

pub(super) struct SharedTime {
    id: String,
    target_renderers: BTreeMap<String, String>,
    participants: BTreeMap<String, Session>,
    arrivals: BTreeMap<String, SharedArrival>,
    retained_bound: Option<i128>,
    servicing_arrivals: bool,
    next_arrival: u64,
    // Nested policy or arrival control invalidates outer pause/grant receipts,
    // including a same-instant resume that consumes an idempotent hold token.
    control_revision: u64,
    unreleased_prefixes: std::collections::BTreeSet<String>,
    pending_drains: BTreeMap<String, cdp::Answer>,
    #[cfg(feature = "test-hooks")]
    admission_pause: Option<SharedAdmissionPause>,
}

impl SharedTime {
    pub(super) fn clock_id(&self) -> &str {
        &self.id
    }
}

#[cfg(feature = "test-hooks")]
struct SharedAdmissionPause {
    target: String,
    clock_id: String,
    reached: tokio::sync::oneshot::Sender<serde_json::Value>,
    release: tokio::sync::oneshot::Receiver<()>,
    binding: Option<SharedAdmissionBinding>,
}

#[cfg(feature = "test-hooks")]
struct SharedAdmissionBinding {
    url: String,
    world: runtime::ExecutionContextId,
    world_unique: String,
    main_unique: String,
    document: u64,
    read: tokio::sync::oneshot::Receiver<()>,
    receipt: tokio::sync::oneshot::Sender<serde_json::Value>,
}

#[derive(Clone)]
struct SharedArrival {
    due: VirtualTime,
    order: u64,
    completion: Option<(fetch::RequestId, ResponseRecord)>,
}

fn log_shared_admission_error(message: std::fmt::Arguments<'_>) {
    static EMITTED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let count = EMITTED.try_update(std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed, |value| (value < 65).then_some(value + 1));
    match count {
        Ok(previous) if previous < 64 => eprintln!("{message}"),
        Ok(_) => eprintln!("FRONTCAP_SHARED_ADMISSION_LOG_LIMIT further_errors_suppressed=true"),
        Err(_) => (),
    }
}

fn shared_popup_closure_matches(
    closures: &[page::FrontcapWindowClosure],
    target: &str,
    instant: &str,
) -> bool {
    let mut matching = closures.iter().filter(|closure| closure.target_id == target);
    matching.next().is_some_and(|closure| closure.closed && closure.unix_time_us == instant)
        && matching.next().is_none()
}

impl Generation {
    /// Copy an already enabled launch-session event feed without browser commands.
    #[cfg(feature = "test-hooks")]
    pub fn test_subscribe_shared_admission_event<E: cdp::Event>(
        &self,
        target: &str,
        clock_id: &str,
    ) -> Result<(String, cdp::Subscription<E>), ChromiumError> {
        if self.popup_id().is_some() || self.endpoint().target_id != target
            || self.shared_time.as_ref().is_none_or(|shared| shared.id != clock_id) {
            return Err(ChromiumError::Shape("admission event observer scope differs".into()));
        }
        let owner = self.launch_page();
        Ok((owner.id.clone(), owner.subscribe()))
    }

    /// Clone the original Fetch owner for one explicit admission-test request stimulus.
    #[cfg(feature = "test-hooks")]
    pub fn test_shared_admission_request_owner(
        &self, target: &str, clock_id: &str,
    ) -> Result<crate::session::Session, ChromiumError> {
        if self.popup_id().is_some() || self.endpoint().target_id != target
            || self.shared_time.as_ref().is_none_or(|shared| shared.id != clock_id) {
            return Err(ChromiumError::Shape("admission request owner scope differs".into()));
        }
        Ok(self.launch_page().clone())
    }

    /// Read one genuine report with the normal reader while its original world
    /// remains live. This scoped test rendezvous issues no browser command.
    #[cfg(feature = "test-hooks")]
    pub fn test_read_shared_admission_navigation(
        &mut self, url: String,
    ) -> Result<(tokio::sync::oneshot::Sender<()>, tokio::sync::oneshot::Receiver<serde_json::Value>, serde_json::Value), ChromiumError> {
        let world = self.world.clone().ok_or_else(|| ChromiumError::Shape("admission observer world missing".into()))?;
        let world_unique = self.world_unique.clone().ok_or_else(|| ChromiumError::Shape("admission observer identity missing".into()))?;
        let main_unique = self.main_context_unique.clone().ok_or_else(|| ChromiumError::Shape("admission main world missing".into()))?;
        if self.popup_id().is_some() || self.own_navigation || url.is_empty() {
            return Err(ChromiumError::Shape("admission page navigation scope differs".into()));
        }
        let identity = serde_json::json!({"observerContextId":world,"observerContextUnique":world_unique,
            "mainContextUnique":main_unique,"frameId":self.main_frame,"document":self.documents,
            "session":self.launch_page().id});
        let shared = self.shared_time.as_mut().ok_or_else(|| ChromiumError::Shape("admission shared clock missing".into()))?;
        let pause = shared.admission_pause.as_mut().ok_or_else(|| ChromiumError::Shape("admission rendezvous missing".into()))?;
        if pause.binding.is_some() {
            return Err(ChromiumError::Shape("admission binding rendezvous already installed".into()));
        }
        let (start, read) = tokio::sync::oneshot::channel();
        let (receipt, observed) = tokio::sync::oneshot::channel();
        pause.binding = Some(SharedAdmissionBinding { url, world, world_unique,
            main_unique, document: self.documents, read, receipt });
        Ok((start, observed, identity))
    }

    /// Pause one owned target after browser routing, before native admission.
    /// The rendezvous belongs to this generation's shared clock, never a global hook.
    #[cfg(feature = "test-hooks")]
    pub fn test_pause_shared_admission(
        &mut self,
        target: String,
        clock_id: String,
    ) -> Result<(tokio::sync::oneshot::Receiver<serde_json::Value>, tokio::sync::oneshot::Sender<()>), ChromiumError> {
        if !self.interception_pages().iter().any(|(id, _)| id == &target) {
            return Err(ChromiumError::Shape("admission test target is not owned by this generation".into()));
        }
        let shared = self.shared_time.as_mut().ok_or_else(||
            ChromiumError::Shape("admission test hook requires a shared clock".into()))?;
        if shared.id != clock_id || shared.admission_pause.is_some() {
            return Err(ChromiumError::Shape("admission test hook clock differs or is already installed".into()));
        }
        let (reached, observer) = tokio::sync::oneshot::channel();
        let (resume, release) = tokio::sync::oneshot::channel();
        shared.admission_pause = Some(SharedAdmissionPause { target, clock_id, reached, release, binding: None });
        Ok((observer, resume))
    }

    #[cfg(feature = "test-hooks")]
    async fn pause_shared_admission_for_test(
        &mut self,
        target: &str,
        renderer: &str,
        instant: &str,
    ) -> Result<(), ChromiumError> {
        let shared = self.shared_time.as_mut().expect("shared admission enabled");
        if !shared.admission_pause.as_ref().is_some_and(|pause| pause.target == target) {
            return Ok(());
        }
        let pause = shared.admission_pause.take().unwrap();
        if shared.id != pause.clock_id {
            return Err(ChromiumError::Shape("admission test hook authority changed".into()));
        }
        pause.reached.send(serde_json::json!({
            "target": target, "clockId": pause.clock_id,
            "rendererId": renderer, "unixTimeUs": instant,
        })).map_err(|_| ChromiumError::Shape("admission test observer dropped".into()))?;
        if let Some(binding) = pause.binding {
            binding.read.await.map_err(|_| ChromiumError::Shape("admission binding read sender dropped".into()))?;
            let at = VirtualTime(u64::try_from(instant.parse::<i128>()
                .map_err(|_| ChromiumError::Shape("admission held instant invalid".into()))?
                - i128::from(self.profile.initial_date_ms) * 1000)
                .map_err(|_| ChromiumError::Shape("admission held instant outside timeline".into()))?);
            if self.now != at || self.own_navigation || self.documents != binding.document
                || self.world.as_ref() != Some(&binding.world)
                || self.world_unique.as_ref() != Some(&binding.world_unique)
                || self.main_context_unique.as_ref() != Some(&binding.main_unique)
                || self.navigates.iter().any(|(url, _)| url == &binding.url) {
                return Err(ChromiumError::Shape("admission binding read original world or queue differs".into()));
            }
            self.drain_bindings(Duration::ZERO).await?;
            let found = self.navigates.iter().filter(|(url, _)| url == &binding.url).collect::<Vec<_>>();
            if self.now != at || self.own_navigation || self.documents != binding.document
                || self.world.as_ref() != Some(&binding.world)
                || self.world_unique.as_ref() != Some(&binding.world_unique)
                || self.main_context_unique.as_ref() != Some(&binding.main_unique)
                || found.len() != 1 || found[0].1 != at {
                return Err(ChromiumError::Shape("admission reader did not consume exactly one genuine navigation".into()));
            }
            binding.receipt.send(serde_json::json!({"target":target,"clockId":pause.clock_id,
                "rendererId":renderer,"heldUnixTimeUs":instant,"url":binding.url,
                "issueTimeUs":found[0].1.0,"nowUs":self.now.0,"world":binding.world,
                "worldUnique":binding.world_unique,"mainUnique":binding.main_unique,
                "document":binding.document,"beforeMatching":0,"afterMatching":1,
                "ownNavigation":false,"readerCalls":1,"reader":"drain_bindings(Duration::ZERO)"}))
                .map_err(|_| ChromiumError::Shape("admission binding receipt receiver dropped".into()))?;
        }
        pause.release.await.map_err(|_| ChromiumError::Shape("admission test release dropped".into()))?;
        Ok(())
    }

    /// Stop a test operation after its synchronous decision is durable, before delivery.
    #[cfg(feature = "test-hooks")]
    pub fn test_interrupt_synchronous_binding(&mut self, url: String) {
        self.interrupt_synchronous_binding = Some(url);
    }

    /// Admit the stopped generation without releasing its initial document tasks.
    pub(super) async fn enable_shared_time(&mut self) -> Result<(), ChromiumError> {
        self.alive()?;
        if !self.patched || self.shared_time.is_some() || self.entered.is_some() {
            return Err(ChromiumError::Unsupported(
                "shared time requires a stopped controlled launch page".into(),
            ));
        }
        let result = self.prepare_shared_time().await;
        if result.is_err() {
            self.dead = true;
        }
        result
    }

    #[cfg(feature = "test-hooks")]
    pub async fn test_enable_shared_time(&mut self) -> Result<(), ChromiumError> {
        if self.shared_time.is_some() { return Ok(()); }
        self.finish_current_deliveries().await?;
        self.enable_shared_time().await
    }

    async fn prepare_shared_time(&mut self) -> Result<(), ChromiumError> {
        let pages = self.interception_pages();
        let expected =
            (i128::from(self.profile.initial_date_ms) * 1000 + i128::from(self.now.0)).to_string();
        let mut participants = BTreeMap::new();
        let mut target_renderers = BTreeMap::new();
        let mut scope = None;
        for (target, session) in &pages {
            let clock = self
                .serviced(session.call(&emulation::FrontcapGetClockParams::default()))
                .await?;
            if clock.unix_time_us.as_ref() != Some(&expected) {
                return Err(ChromiumError::Shape(
                    "shared-time participant is not at the generation instant".into(),
                ));
            }
            let prepared = self
                .serviced(session.call(&emulation::FrontcapPrepareSharedClockParams::default()))
                .await?;
            if scope.as_ref().is_some_and(|id| id != &prepared.clock_id) {
                return Err(ChromiumError::Shape(
                    "shared-time participant belongs to another authority".into(),
                ));
            }
            scope = Some(prepared.clock_id);
            target_renderers.insert(target.clone(), clock.renderer_id.clone());
            participants
                .entry(clock.renderer_id)
                .or_insert_with(|| session.clone());
        }
        for session in participants.values() {
            self.serviced(session.call(&emulation::FrontcapArmSharedClockParams::default()))
                .await?;
        }
        let id =
            scope.ok_or_else(|| ChromiumError::Shape("shared-time membership is empty".into()))?;
        let page = self.page.clone();
        self.serviced(page.call(&page::FrontcapActivateSharedClockParams::new(id.clone())))
            .await?;
        self.shared_time = Some(SharedTime {
            id,
            target_renderers,
            participants,
            arrivals: BTreeMap::new(),
            retained_bound: None,
            servicing_arrivals: false,
            next_arrival: 0,
            control_revision: 0,
            unreleased_prefixes: Default::default(),
            pending_drains: BTreeMap::new(),
            #[cfg(feature = "test-hooks")]
            admission_pause: None,
        });
        Ok(())
    }

    /// Recover native membership without issuing renderer commands or releasing time.
    pub(super) async fn recover_shared_time(&mut self) -> Result<(), ChromiumError> {
        if !self.patched {
            return Ok(());
        }
        let inventory = match self
            .page
            .call(&page::FrontcapGetSharedClockParams::default())
            .await
        {
            Ok(inventory) => inventory,
            Err(ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. }))
                if message == "shared clock is not prepared" =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if !inventory.activated {
            return Ok(());
        }
        if inventory.failed {
            return Err(ChromiumError::Shape(
                "cannot recover failed shared clock".into(),
            ));
        }
        let held = self
            .page
            .call(&page::FrontcapSuspendSharedClockParams::new(
                inventory.clock_id.clone(),
            ))
            .await?;
        let mut participants = BTreeMap::new();
        let mut target_renderers = BTreeMap::new();
        for (target, session) in self.interception_pages() {
            let admitted = session
                .call(&page::FrontcapAdmitSharedWindowParams::new(
                    inventory.clock_id.clone(),
                ))
                .await?;
            if !inventory.participants.iter().any(|p| {
                p.renderer_id == admitted.renderer_id
                    && p.alive
                    && p.confirmed
                    && p.armed
                    && !p.retired
            }) {
                return Err(ChromiumError::Shape(
                    "recovered window has no live shared participant".into(),
                ));
            }
            target_renderers.insert(target, admitted.renderer_id.clone());
            participants.entry(admitted.renderer_id).or_insert(session);
        }
        if inventory
            .participants
            .iter()
            .filter(|p| !p.retired)
            .any(|p| !participants.contains_key(&p.renderer_id))
        {
            return Err(ChromiumError::Shape(
                "shared recovery is missing a participant owner".into(),
            ));
        }
        let retained_bound = Some(
            held.bound_us
                .parse::<i128>()
                .map_err(|_| ChromiumError::Shape("invalid recovered shared bound".into()))?,
        );
        self.shared_time = Some(SharedTime {
            id: inventory.clock_id,
            target_renderers,
            participants,
            retained_bound,
            arrivals: BTreeMap::new(),
            servicing_arrivals: false,
            next_arrival: 0,
            control_revision: 0,
            unreleased_prefixes: Default::default(),
            pending_drains: BTreeMap::new(),
            #[cfg(feature = "test-hooks")]
            admission_pause: None,
        });
        Ok(())
    }

    fn next_shared_control_revision(&mut self) -> Result<(), ChromiumError> {
        let shared = self.shared_time.as_mut().expect("shared control enabled");
        shared.control_revision = shared.control_revision.checked_add(1)
            .ok_or_else(|| ChromiumError::Shape("shared control revision exhausted".into()))?;
        Ok(())
    }

    fn shared_policy_activity(&self) -> (u64, u64) {
        let shared = self.shared_time.as_ref().expect("shared control enabled");
        (shared.control_revision, shared.next_arrival)
    }

    fn shared_policy_state(&self) -> SharedPolicyState {
        let shared = self.shared_time.as_ref().expect("shared policy enabled");
        SharedPolicyState {
            now: self.now,
            control_revision: shared.control_revision,
            next_arrival: shared.next_arrival,
            targets: shared.target_renderers.clone(),
            participants: shared.participants.iter()
                .map(|(renderer, session)| (renderer.clone(), session.id.clone())).collect(),
            pages: self.interception_pages().into_iter()
                .map(|(target, session)| (target, session.id)).collect(),
        }
    }

    /// Recollect only after observed nested control, membership or target routing
    /// changed. An unexplained clock disagreement and every CDP error still fail.
    async fn shared_policy_current(
        &self,
        id: &str,
        held: &page::FrontcapSuspendSharedClockReturns,
        state: &SharedPolicyState,
    ) -> Result<bool, ChromiumError> {
        if &self.shared_policy_state() != state {
            return Ok(false);
        }
        let owner = self.page.clone();
        let before = owner.call(&page::FrontcapGetSharedClockParams::default()).await?;
        if before.failed || !before.activated || before.clock_id != id {
            return Err(ChromiumError::Shape("shared-time authority changed or failed".into()));
        }
        if before.suspension_id != held.suspension_id || before.unix_time_us != held.unix_time_us {
            return Err(ChromiumError::Shape(format!(
                "shared policy hold changed without observed control: held={} actual={} suspension_matches={}",
                held.unix_time_us, before.unix_time_us, before.suspension_id == held.suspension_id,
            )));
        }
        for (target, session_id) in &state.pages {
            let renderer = state.targets.get(target).ok_or_else(||
                ChromiumError::Shape("shared policy target has no admitted renderer".into()))?;
            let session = Session::sharing_clock(owner.client.clone(), session_id.clone(), owner.service_clock());
            // This browser-owned query remains serviceable while the renderer
            // waits synchronously, and independently detects fixed-time retargets.
            let routed = session.call(&page::FrontcapGetSharedWindowRendererParams::new(id.to_owned())).await?;
            if routed.unix_time_us != held.unix_time_us {
                return Err(ChromiumError::Shape("shared target escaped its publication hold".into()));
            }
            if &routed.renderer_id != renderer {
                return Ok(false);
            }
        }
        let after = owner.call(&page::FrontcapGetSharedClockParams::default()).await?;
        if after.failed || !after.activated || after.clock_id != id {
            return Err(ChromiumError::Shape("shared-time authority changed or failed".into()));
        }
        if after.suspension_id != held.suspension_id || after.unix_time_us != held.unix_time_us {
            return Err(ChromiumError::Shape("shared policy lost its hold during routing inspection".into()));
        }
        if after.participants.iter().filter(|participant| !participant.retired)
            .any(|participant| !participant.alive || !participant.confirmed || !participant.armed) {
            return Err(ChromiumError::Shape("shared-time participant failed or is not armed".into()));
        }
        let live: std::collections::BTreeSet<_> = after.participants.iter()
            .filter(|participant| !participant.retired).map(|participant| &participant.renderer_id).collect();
        let expected: std::collections::BTreeSet<_> = state.participants.keys().collect();
        if live != expected {
            // Fresh admission and native retirement reconciliation, rather than
            // deleting an old vote or assuming a missing session has retired.
            return Ok(false);
        }
        let adapter_instant = i128::from(self.profile.initial_date_ms) * 1000 + i128::from(state.now.0);
        if held.unix_time_us != adapter_instant.to_string() {
            return Err(ChromiumError::Shape("shared policy adapter instant differs from its hold".into()));
        }
        Ok(&self.shared_policy_state() == state)
    }

    /// Called only inside policy's retained active-service deadline, including
    /// its earlier observer write. This loop never establishes a fresh budget.
    pub(super) async fn shared_policy(
        &mut self,
        params: emulation::SetVirtualTimePolicyParams,
        endpoint: Option<VirtualTime>,
        current_tasks_only: bool,
    ) -> Result<PolicyOutcome, ChromiumError> {
        self.next_shared_control_revision()?;
        let id = self.shared_time.as_ref().expect("shared policy enabled").id.clone();
        let owner = self.page.clone();
        'round: loop {
            // No renderer service around browser-owned hold/inventory commands:
            // returning a receipt from before nested progress would stale it.
            let held = owner.call(&page::FrontcapSuspendSharedClockParams::new(id.clone())).await?;
            let activity = self.shared_policy_activity();
            if !Box::pin(self.admit_shared_pages(&held, activity)).await?
                || self.shared_policy_activity() != activity {
                continue;
            }
            self.retire_shared_pages().await?;
            if self.shared_policy_activity() != activity {
                continue;
            }
            let state = self.shared_policy_state();
            if !self.shared_policy_current(&id, &held, &state).await? {
                continue;
            }
            let participants = self.shared_time.as_ref().unwrap().participants.clone();
            for (renderer, session) in &participants {
                let clock = self.serviced(session.call(&emulation::FrontcapGetClockParams::default()))
                    .await.inspect_err(|error| {
                        eprintln!("shared policy clock query failed: renderer={renderer:?} held={} error={error}", held.unix_time_us);
                    })?;
                if !self.shared_policy_current(&id, &held, &state).await? {
                    continue 'round;
                }
                if &clock.renderer_id != renderer || clock.unix_time_us.as_ref() != Some(&held.unix_time_us) {
                    let adapter_instant = i128::from(self.profile.initial_date_ms) * 1000 + i128::from(self.now.0);
                    return Err(ChromiumError::Shape(format!(
                        "shared-time renderer changed during progress: expected_renderer={renderer:?} actual_renderer={:?} held_unix_time_us={} actual_unix_time_us={:?} adapter_unix_time_us={adapter_instant} session={:?} owner={}",
                        clock.renderer_id, held.unix_time_us, clock.unix_time_us,
                        session.id, session.id == owner.id,
                    )));
                }
                self.serviced(session.call(&emulation::SetVirtualTimePolicyParams {
                    bind_storage_clock: Some(session.id == owner.id),
                    ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)
                })).await?;
                if !self.shared_policy_current(&id, &held, &state).await? {
                    continue 'round;
                }
            }
            if self.event_stop.is_some() {
                self.commit()?;
            }
            if params.policy == emulation::VirtualTimePolicy::Pause {
                return Ok(PolicyOutcome::Held);
            }
            let endpoint = endpoint.ok_or_else(||
                ChromiumError::Shape("shared progress requires a finite endpoint".into()))?;
            if self.policy_stop_reached(current_tasks_only) || self.now > endpoint
                || (self.now == endpoint && !current_tasks_only) {
                // The paused receipt round is valid. Do not fabricate an expiry
                // event or renew the duration after an already completed arrival.
                return Ok(PolicyOutcome::Held);
            }
            let budget = endpoint.since(self.now);
            let mut sessions: Vec<_> = participants.values().cloned().collect();
            sessions.sort_by_key(|session| session.id == owner.id);
            // Install local budgets while publication is still held. A current-
            // instant task may issue a synchronous request; keep renderer calls
            // serviceable and recollect the whole pause round if it does.
            for session in sessions {
                let mut grant = params.clone();
                grant.budget = Some(budget.as_micros() as f64 / 1000.0);
                grant.bind_storage_clock = Some(session.id == owner.id);
                self.serviced(session.call(&grant)).await?;
                if !self.shared_policy_current(&id, &held, &state).await? {
                    continue 'round;
                }
                if self.event_stop.is_some() {
                    self.commit()?;
                }
                if self.policy_stop_reached(current_tasks_only) {
                    // Some local grants may already be installed. Re-pause all
                    // participants before returning the retained stop boundary.
                    continue 'round;
                }
            }
            let bound = (i128::from(self.profile.initial_date_ms) * 1000 + i128::from(endpoint.0)).to_string();
            owner.call(&page::FrontcapResumeSharedClockParams {
                bound_us: Some(bound),
                ..page::FrontcapResumeSharedClockParams::new(id.clone(), held.suspension_id)
            }).await?;
            return Ok(PolicyOutcome::Granted);
        }
    }

    /// Browser-owned evidence for this admission round. Nested control may consume
    /// its hold; every unrelated authority or time change remains an error.
    async fn shared_admission_inventory(
        &self,
        held: &page::FrontcapSuspendSharedClockReturns,
        activity: (u64, u64),
    ) -> Result<Option<page::FrontcapGetSharedClockReturns>, ChromiumError> {
        let inventory = self.page.call(&page::FrontcapGetSharedClockParams::default()).await?;
        let shared = self.shared_time.as_ref().expect("shared admission enabled");
        if inventory.failed || !inventory.activated || inventory.clock_id != shared.id {
            return Err(ChromiumError::Shape("shared admission authority changed or failed".into()));
        }
        let now = i128::from(self.profile.initial_date_ms) * 1000 + i128::from(self.now.0);
        if self.shared_policy_activity() != activity || now.to_string() != held.unix_time_us {
            return Ok(None);
        }
        if inventory.suspension_id != held.suspension_id || inventory.unix_time_us != held.unix_time_us {
            return Err(ChromiumError::Shape("shared admission lost its publication hold without observed control".into()));
        }
        Ok(Some(inventory))
    }

    /// A detached session alone never retires ownership. Only a browser-retained
    /// completed close of this owned popup can invalidate an admission snapshot.
    /// The next normal round must still pass retire_shared_pages before resuming.
    async fn shared_popup_closed_during_admission(
        &self,
        target: &str,
        session: &Session,
        held: &page::FrontcapSuspendSharedClockReturns,
        activity: (u64, u64),
    ) -> Result<bool, ChromiumError> {
        let owner = self.launch_page().clone();
        // This narrow recovery is for launch-page-controlled admission of a popup.
        // Root loss or an actively addressed popup is not reclassified as closure.
        if self.popup_id().is_some() || session.id == owner.id || held.suspension_id.is_empty() {
            return Ok(false);
        }
        let owns_session = || self.contexts.lock().unwrap().popups.iter()
            .any(|popup| popup.target_id == target && popup.session_id == session.id);
        if !owns_session() {
            return Ok(false);
        }
        if self.shared_admission_inventory(held, activity).await?.is_none() {
            return Ok(false);
        }
        let closures = owner.call(&page::FrontcapGetSharedWindowClosuresParams::default()).await?;
        if !shared_popup_closure_matches(&closures.closures, target, &held.unix_time_us) {
            return Ok(false);
        }
        if self.shared_admission_inventory(held, activity).await?.is_none() {
            return Ok(false);
        }
        Ok(owns_session())
    }

    /// A new page starts paused. Publish adapter ownership only after native
    /// participant and routing receipts agree under the same publication hold.
    async fn admit_shared_pages(
        &mut self,
        held: &page::FrontcapSuspendSharedClockReturns,
        activity: (u64, u64),
    ) -> Result<bool, ChromiumError> {
        let instant = &held.unix_time_us;
        let now = instant.parse::<i128>()
            .map_err(|_| ChromiumError::Shape("invalid shared instant".into()))?;
        let at = VirtualTime(u64::try_from(now - i128::from(self.profile.initial_date_ms) * 1000)
            .map_err(|_| ChromiumError::Shape("shared admission outside timeline".into()))?);
        // A hold can interrupt a segment after the adapter's last known instant,
        // but admission must never rewind progress completed by nested service.
        if self.shared_policy_activity() != activity || self.now > at {
            return Ok(false);
        }
        self.now = at;
        let pages = self.interception_pages();
        for (target, session) in pages {
            let popup = self.contexts.lock().unwrap().popups.iter().any(|p| p.target_id == target);
            if popup {
                Box::pin(self.admit_held_windows(vec![target.clone()])).await.inspect_err(|error| log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=popup-admit target={target:?} session={:?} held={instant} error={error:?}", session.id)))?;
            }
            if self.shared_policy_activity() != activity || self.now != at {
                return Ok(false);
            }
            // Preserve arrival service: document responses may be needed before a
            // renderer can answer its clock query or the opener's pause command.
            if self.shared_arrival_service_active() {
                self.worker_enrollment_result()?;
                self.take_paused()?;
                Box::pin(self.handle_queued()).await.inspect_err(|error| log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=arrival-service target={target:?} session={:?} held={instant} error={error:?}", session.id)))?;
                if i128::from(self.profile.initial_date_ms) * 1000 + i128::from(self.now.0) != now {
                    return Ok(false);
                }
            }
            let id = self.shared_time.as_ref().unwrap().id.clone();
            let owner = match session.call(&page::FrontcapGetSharedWindowRendererParams::new(id.clone())).await.inspect_err(|error| log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=window-route target={target:?} session={:?} held={instant} error={error:?}", session.id))) {
                Ok(owner) => owner,
                Err(error @ ChromiumError::Cdp(cdp::CdpError::SessionDetached)) => {
                    if self.shared_popup_closed_during_admission(&target, &session, held, activity).await? {
                        return Ok(false);
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            };
            let Some(inventory) = self.shared_admission_inventory(held, activity).await? else {
                return Ok(false);
            };
            if owner.unix_time_us != *instant {
                return Err(ChromiumError::Shape("shared window routing escaped its admission hold".into()));
            }
            if self.shared_time.as_ref().unwrap().participants.contains_key(&owner.renderer_id) {
                if !inventory.participants.iter().any(|p| p.renderer_id == owner.renderer_id
                    && p.alive && p.confirmed && p.armed && !p.retired) {
                    // Routing and inventory are separate browser commands. A
                    // native retirement is evidence to recollect, not erase a vote.
                    if inventory.participants.iter().any(|p| p.renderer_id == owner.renderer_id && p.retired) {
                        return Ok(false);
                    }
                    return Err(ChromiumError::Shape("shared window route has no live armed native participant".into()));
                }
                #[cfg(feature = "test-hooks")]
                self.pause_shared_admission_for_test(&target, &owner.renderer_id, instant).await?;
                let admitted = match session.call(&page::FrontcapAdmitSharedWindowParams::new(id)).await.inspect_err(|error| log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=known-window-admit target={target:?} session={:?} held={instant} error={error:?}", session.id))) {
                    Ok(admitted) => admitted,
                    Err(ChromiumError::Cdp(cdp::CdpError::Protocol { code: -32000, message, data: None }))
                        if message == "shared window renderer is not armed" => {
                            // This native guard runs only after validating the
                            // active held authority and does not erase admission.
                            let _ = self.shared_admission_inventory(held, activity).await?;
                            return Ok(false);
                        }
                    Err(error @ ChromiumError::Cdp(cdp::CdpError::SessionDetached)) => {
                        // The popup can complete its close after routing was read.
                        // Require the same owned close receipt as the routing query;
                        // detachment alone never removes a participant or releases time.
                        if self.shared_popup_closed_during_admission(&target, &session, held, activity).await? {
                            return Ok(false);
                        }
                        return Err(error);
                    }
                    Err(error) => return Err(error),
                };
                let Some(inventory) = self.shared_admission_inventory(held, activity).await? else {
                    return Ok(false);
                };
                if admitted.renderer_id != owner.renderer_id {
                    // Native admission succeeded for a different selected process.
                    // Keep the explicit hold and rebuild ownership in a fresh round.
                    return Ok(false);
                }
                if !inventory.participants.iter().any(|p| p.renderer_id == admitted.renderer_id
                    && p.alive && p.confirmed && p.armed && !p.retired) {
                    if inventory.participants.iter().any(|p| p.renderer_id == admitted.renderer_id && p.retired) {
                        return Ok(false);
                    }
                    return Err(ChromiumError::Shape("admitted shared window has no live armed native participant".into()));
                }
                self.shared_time.as_mut().unwrap().target_renderers.insert(target, admitted.renderer_id);
                continue;
            }
            // A renderer blocked by a synchronous request still needs the existing
            // request service while its clock command is pending.
            let clock = if self.shared_arrival_service_active() {
                let query = emulation::FrontcapGetClockParams::default();
                let call = session.call(&query);
                tokio::pin!(call);
                loop {
                    self.worker_enrollment_result()?;
                    self.take_paused()?;
                    Box::pin(self.handle_queued()).await.inspect_err(|error| log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=clock-wait-service target={target:?} session={:?} held={instant} error={error:?}", session.id)))?;
                    if i128::from(self.profile.initial_date_ms) * 1000 + i128::from(self.now.0) != now {
                        return Ok(false);
                    }
                    tokio::select! {
                        result = &mut call => break result.inspect_err(|error| log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=arrival-renderer-clock target={target:?} session={:?} held={instant} error={error:?}", session.id)))?,
                        _ = tokio::time::sleep(Duration::from_millis(2)) => {},
                    }
                }
            } else {
                self.worker_enrollment_result()?;
                self.take_paused()?;
                while let Some(i) = self.queued.iter().position(|(_, ev)| self.blocks_main_thread(ev)) {
                    let (_, ev) = self.queued.remove(i);
                    Box::pin(self.serve_blocking(ev)).await.inspect_err(|error| log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=blocking-service target={target:?} session={:?} held={instant} error={error:?}", session.id)))?;
                    if i128::from(self.profile.initial_date_ms) * 1000 + i128::from(self.now.0) != now {
                        return Ok(false);
                    }
                    self.take_paused()?;
                }
                self.serviced(session.call(&emulation::FrontcapGetClockParams::default()))
                    .await.inspect_err(|error| {
                        log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=renderer-clock target={target:?} session={:?} held={instant} error={error:?}", session.id));
                    })?
            };
            if self.shared_admission_inventory(held, activity).await?.is_none() {
                return Ok(false);
            }
            if clock.unix_time_us.as_deref() != Some(instant.as_str()) {
                return Err(ChromiumError::Shape(format!(
                    "new shared-time page is not at the held instant: target={target} renderer={} expected={instant} actual={:?}",
                    clock.renderer_id, clock.unix_time_us,
                )));
            }
            let participant_id = if !self.shared_time.as_ref().unwrap().participants.contains_key(&clock.renderer_id) {
                let prepared = self.serviced(session.call(&emulation::FrontcapPrepareSharedClockParams::default())).await.inspect_err(|error| log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=renderer-prepare target={target:?} session={:?} held={instant} error={error:?}", session.id)))?;
                if prepared.clock_id != id {
                    return Err(ChromiumError::Shape("new shared-time page belongs to another authority".into()));
                }
                let Some(inventory) = self.shared_admission_inventory(held, activity).await? else {
                    return Ok(false);
                };
                if prepared.unix_time_us != *instant {
                    return Err(ChromiumError::Shape("prepared shared participant escaped its held instant".into()));
                }
                let mut matching = inventory.participants.iter().filter(|p| p.participant_id == prepared.participant_id);
                let participant = matching.next().ok_or_else(||
                    ChromiumError::Shape("prepared shared participant is absent from native inventory".into()))?;
                if matching.next().is_some() {
                    return Err(ChromiumError::Shape("prepared shared participant identity is ambiguous".into()));
                }
                if participant.retired || participant.renderer_id != clock.renderer_id {
                    // Prepare ran on another renderer or its vote already retired.
                    // Never publish the earlier renderer ID as this receipt's owner.
                    return Ok(false);
                }
                if !participant.alive || !participant.confirmed {
                    return Err(ChromiumError::Shape("prepared shared participant is not alive and confirmed".into()));
                }
                self.serviced(session.call(&emulation::FrontcapArmSharedClockParams::default())).await.inspect_err(|error| log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=renderer-arm target={target:?} session={:?} held={instant} error={error:?}", session.id)))?;
                let Some(inventory) = self.shared_admission_inventory(held, activity).await? else {
                    return Ok(false);
                };
                let routed = session.call(&page::FrontcapGetSharedWindowRendererParams::new(id.clone())).await.inspect_err(|error| log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=armed-window-route target={target:?} session={:?} held={instant} error={error:?}", session.id)))?;
                if routed.unix_time_us != *instant {
                    return Err(ChromiumError::Shape("armed shared route escaped its admission hold".into()));
                }
                if routed.renderer_id != clock.renderer_id {
                    return Ok(false);
                }
                let participant = inventory.participants.iter().find(|p| p.participant_id == prepared.participant_id)
                    .ok_or_else(|| ChromiumError::Shape("armed shared participant disappeared from native inventory".into()))?;
                if participant.retired {
                    return Ok(false);
                }
                if participant.renderer_id != clock.renderer_id || !participant.alive || !participant.confirmed || !participant.armed {
                    return Err(ChromiumError::Shape("armed shared receipt does not match its native participant".into()));
                }
                // The serviced Admit below may encounter a synchronous request
                // from this renderer. Publish its independently proven native vote
                // first, so the arrival owner can be recognized without inventing
                // membership from the older GetClock receipt.
                self.shared_time.as_mut().unwrap().participants
                    .entry(participant.renderer_id.clone()).or_insert_with(|| session.clone());
                Some(prepared.participant_id)
            } else {
                None
            };
            let admitted = match self.serviced(session.call(&page::FrontcapAdmitSharedWindowParams::new(id))).await.inspect_err(|error| log_shared_admission_error(format_args!("FRONTCAP_SHARED_ADMISSION_ERROR stage=prepared-window-admit target={target:?} session={:?} held={instant} error={error:?}", session.id))) {
                Ok(admitted) => admitted,
                Err(ChromiumError::Cdp(cdp::CdpError::Protocol { code: -32000, message, data: None }))
                    if message == "shared window renderer is not armed" => {
                        let _ = self.shared_admission_inventory(held, activity).await?;
                        return Ok(false);
                    }
                Err(error) => return Err(error),
            };
            let Some(inventory) = self.shared_admission_inventory(held, activity).await? else {
                return Ok(false);
            };
            if admitted.renderer_id != clock.renderer_id {
                return Ok(false);
            }
            let matching: Vec<_> = inventory.participants.iter().filter(|p| p.renderer_id == admitted.renderer_id
                && participant_id.as_ref().is_none_or(|id| &p.participant_id == id)).collect();
            if matching.len() != 1 {
                return Err(ChromiumError::Shape("admitted shared participant identity is absent or ambiguous".into()));
            }
            let participant = matching[0];
            if participant.retired {
                return Ok(false);
            }
            if !participant.alive || !participant.confirmed || !participant.armed {
                return Err(ChromiumError::Shape("admitted shared participant is not live and armed".into()));
            }
            // Only native receipts publish new adapter membership. Old votes stay
            // in the map until retire_shared_pages proves retirement/live ownership.
            let shared = self.shared_time.as_mut().unwrap();
            shared.participants.entry(admitted.renderer_id.clone()).or_insert(session);
            shared.target_renderers.insert(target, admitted.renderer_id);
        }
        let Some(inventory) = self.shared_admission_inventory(held, activity).await? else {
            return Ok(false);
        };
        if inventory.participants.iter().filter(|p| !p.retired)
            .any(|p| !self.shared_time.as_ref().unwrap().participants.contains_key(&p.renderer_id)) {
            // A preparation may have succeeded immediately before retargeting.
            // Keep its native vote: a later round must find an owner or retirement.
            return Ok(false);
        }
        Ok(true)
    }

    /// Native window admission or closure can hold a quiet grant before budget expiry.
    /// Consume that boundary before waiting for the grant to finish.
    pub(super) async fn shared_window_boundary(&mut self) -> Result<bool, ChromiumError> {
        if self.shared_time.is_none() {
            return Ok(false);
        }
        let closures = self
            .page
            .call(&page::FrontcapGetSharedWindowClosuresParams::default())
            .await?;
        let admission = self.contexts.lock().unwrap().popups.iter().any(|popup| {
            !popup.closed && (popup.held || !self.shared_time.as_ref().unwrap()
                .target_renderers.contains_key(&popup.target_id))
        });
        if closures.closures.is_empty() && !admission {
            return Ok(false);
        }
        if !closures.closures.is_empty() {
            self.retire_shared_pages().await?;
        }
        if admission {
            // A new window may reuse an advancing renderer. Publication being
            // held does not prevent its current-instant script from running.
            // Pause existing schedulers before releasing the held document.
            let shared = self.shared_time.as_ref().unwrap();
            let id = shared.id.clone();
            let participants = shared.participants.values().cloned().collect::<Vec<_>>();
            self.page.call(&page::FrontcapSuspendSharedClockParams::new(id)).await?;
            self.now = self.read_time().await?.max(self.now);
            for session in participants {
                self.serviced(session.call(&emulation::SetVirtualTimePolicyParams::new(
                    emulation::VirtualTimePolicy::Pause,
                ))).await?;
            }
        }
        // Use the same bounded policy entry point as progress. There is no
        // separate, unbounded caller of shared-policy reconciliation.
        self.policy(emulation::VirtualTimePolicy::Pause, None).await?;
        Ok(true)
    }

    /// A detached page is not proof that its process may stop voting. Retain its
    /// ownership until either another live page uses that renderer or the browser
    /// reports normal process cleanup. Failed/lost renderers never take this path.
    pub(super) async fn retire_shared_pages(&mut self) -> Result<(), ChromiumError> {
        let service = self.page.service_clock();
        let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
        loop {
            let closures = self
                .page
                .call(&page::FrontcapGetSharedWindowClosuresParams::default())
                .await?
                .closures;
            if closures.iter().any(|closure| !closure.closed) {
                if service.elapsed() >= deadline {
                    return Err(ChromiumError::CallTimeout("native shared window closure"));
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
                continue;
            }
            let pages = self.interception_pages();
            let targets: Vec<_> = pages.iter().map(|(target, _)| target.clone()).collect();
            let inventory = self
                .page
                .call(&page::FrontcapGetSharedClockParams::default())
                .await?;
            let shared = self.shared_time.as_ref().unwrap();
            if inventory.failed || inventory.clock_id != shared.id {
                return Err(ChromiumError::Shape(
                    "shared authority failed during page retirement".into(),
                ));
            }
            let retired: std::collections::BTreeSet<_> = inventory
                .participants
                .iter()
                .filter(|p| p.retired)
                .map(|p| p.renderer_id.clone())
                .collect();
            let mut live = BTreeMap::new();
            for (target, session) in &pages {
                if let Some(renderer) = shared.target_renderers.get(target) {
                    live.entry(renderer.clone())
                        .or_insert_with(|| session.clone());
                }
            }
            // Native cleanup and Target.detached are independent notifications.
            // Wait for both; a target disappearance alone cannot erase a vote.
            let pending = closures
                .iter()
                .any(|closure| targets.contains(&closure.target_id))
                || live.keys().any(|renderer| retired.contains(renderer))
                || shared
                    .participants
                    .keys()
                    .any(|renderer| !live.contains_key(renderer) && !retired.contains(renderer));
            if pending {
                if service.elapsed() >= deadline {
                    return Err(ChromiumError::CallTimeout(
                        "native shared renderer retirement",
                    ));
                }
                self.take_contexts()?;
                tokio::time::sleep(Duration::from_millis(2)).await;
                continue;
            }
            if shared
                .arrivals
                .keys()
                .any(|renderer| !live.contains_key(renderer))
            {
                return Err(ChromiumError::Unsupported(
                    "shared renderer retired with an outstanding arrival".into(),
                ));
            }
            let shared = self.shared_time.as_mut().unwrap();
            shared
                .target_renderers
                .retain(|target, _| targets.contains(target));
            shared
                .pending_drains
                .retain(|session, _| pages.iter().any(|(_, page)| &page.id == session));
            shared.participants = live;
            if !closures.is_empty() {
                let id = shared.id.clone();
                let held = self
                    .page
                    .call(&page::FrontcapSuspendSharedClockParams::new(id.clone()))
                    .await?;
                for closure in &closures {
                    if closure.unix_time_us != held.unix_time_us {
                        return Err(ChromiumError::Shape(
                            "shared window closure escaped its held instant".into(),
                        ));
                    }
                }
                let instant = held
                    .unix_time_us
                    .parse::<i128>()
                    .map_err(|_| ChromiumError::Shape("invalid shared closure instant".into()))?;
                let at = VirtualTime(
                    u64::try_from(instant - i128::from(self.profile.initial_date_ms) * 1000)
                        .map_err(|_| {
                            ChromiumError::Shape("shared closure outside timeline".into())
                        })?,
                );
                if self.log.last_time() > at {
                    return Err(ChromiumError::Shape(
                        "shared closure follows a later trace event".into(),
                    ));
                }
                self.now = at;
                self.record_contexts();
                for closure in closures {
                    self.page
                        .call(&page::FrontcapAcknowledgeSharedWindowClosureParams::new(
                            id.clone(),
                            closure.target_id,
                        ))
                        .await?;
                }
            }
            return Ok(());
        }
    }

    pub(super) async fn shared_arrival(
        &mut self,
        request: &fetch::RequestId,
        due: VirtualTime,
    ) -> Result<(), ChromiumError> {
        let id = self
            .shared_time
            .as_ref()
            .expect("shared arrival enabled")
            .id
            .clone();
        let target = self.target_of_fetch(request);
        // Hold publication before resolving ownership or fulfilling the request.
        // A new window may already be executing in a registered renderer, whose
        // main thread cannot answer a clock query until this request is served.
        let held = self
            .page
            .call(&page::FrontcapSuspendSharedClockParams::new(id.clone()))
            .await?;
        let renderer = if let Some(renderer) = self
            .shared_time
            .as_ref()
            .unwrap()
            .target_renderers
            .get(&target)
        {
            renderer.clone()
        } else {
            let admitted = self
                .fetch_session(request)
                .call(&page::FrontcapAdmitSharedWindowParams::new(id))
                .await?;
            let shared = self.shared_time.as_mut().unwrap();
            if !shared.participants.contains_key(&admitted.renderer_id) {
                return Err(ChromiumError::Shape(
                    "shared synchronous owner is not an admitted renderer".into(),
                ));
            }
            shared
                .target_renderers
                .insert(target, admitted.renderer_id.clone());
            admitted.renderer_id
        };
        if self
            .shared_time
            .as_ref()
            .unwrap()
            .arrivals
            .contains_key(&renderer)
        {
            return Err(ChromiumError::Unsupported(
                "multiple main-thread arrivals in one renderer".into(),
            ));
        }
        let now = held
            .unix_time_us
            .parse::<i128>()
            .map_err(|_| ChromiumError::Shape("invalid shared instant".into()))?;
        self.now = VirtualTime(
            u64::try_from(now - i128::from(self.profile.initial_date_ms) * 1000)
                .map_err(|_| ChromiumError::Shape("shared arrival outside timeline".into()))?,
        );
        let bound = held
            .bound_us
            .parse::<i128>()
            .map_err(|_| ChromiumError::Shape("invalid shared endpoint".into()))?;
        let arrival = i128::from(self.profile.initial_date_ms) * 1000 + i128::from(due.0);
        let shared = self.shared_time.as_mut().unwrap();
        shared.retained_bound = Some(
            shared
                .retained_bound
                .unwrap_or(bound)
                .max(bound)
                .max(arrival),
        );
        let order = shared.next_arrival;
        shared.next_arrival = order
            .checked_add(1)
            .ok_or_else(|| ChromiumError::Shape("shared arrival order exhausted".into()))?;
        shared.arrivals.insert(
            renderer,
            SharedArrival {
                due,
                order,
                completion: None,
            },
        );
        Ok(())
    }

    pub(super) fn shared_arrival_service_active(&self) -> bool {
        self.shared_time
            .as_ref()
            .is_some_and(|s| s.servicing_arrivals)
    }

    pub(super) fn shared_request_owner_waiting(&self, request: &fetch::RequestId) -> bool {
        self.shared_time.as_ref().is_some_and(|shared| {
            shared
                .target_renderers
                .get(&self.target_of_fetch(request))
                .is_some_and(|renderer| shared.arrivals.contains_key(renderer))
        })
    }

    // Called only for a newly created hold, before any release command is sent.
    pub(super) fn retain_shared_prefix_hold(&mut self, event: &fetch::RequestPausedEvent) -> bool {
        // A worker load belongs to its runnable worker, even while the page waits.
        if event
            .network_id
            .as_ref()
            .is_some_and(|id| self.worker_request_session(&id.0).is_some())
        {
            return false;
        }
        let request = &event.request_id;
        if !self.shared_request_owner_waiting(request) {
            return false;
        }
        self.shared_time
            .as_mut()
            .unwrap()
            .unreleased_prefixes
            .insert(request.0.clone());
        true
    }

    pub(super) fn shared_prefix_never_released(&self, request: &fetch::RequestId) -> bool {
        self.shared_time
            .as_ref()
            .is_some_and(|shared| shared.unreleased_prefixes.contains(&request.0))
    }

    async fn release_shared_prefixes(
        &mut self,
        paused: &std::collections::BTreeSet<String>,
    ) {
        let shared = self.shared_time.as_ref().unwrap();
        let pending: Vec<_> = self.net.held.iter().filter(|(_, hold)| {
            shared.unreleased_prefixes.contains(&hold.fetch_id.0)
                && shared.target_renderers.get(&self.target_of_fetch(&hold.fetch_id))
                    .is_some_and(|renderer| paused.contains(renderer)
                        && !shared.arrivals.contains_key(renderer))
        }).map(|(ordinal, hold)| (ordinal.clone(), hold.fetch_id.0.clone())).collect();
        for (ordinal, request) in pending {
            // The owner can answer again and publication is still held. A future
            // response must no longer count as work completing at this instant.
            self.shared_time.as_mut().unwrap().unreleased_prefixes.remove(&request);
            self.release_held(&ordinal).await;
        }
    }

    pub(super) async fn set_shared_worker_request_pauses(
        &self,
        session: &str,
        request: &str,
        pauses: bool,
    ) -> Result<(), ChromiumError> {
        let shared = self.shared_time.as_ref().expect("shared worker control");
        let renderer = self
            .interception_pages()
            .into_iter()
            .find(|(_, page)| page.id == session)
            .and_then(|(target, _)| shared.target_renderers.get(&target))
            .ok_or_else(|| ChromiumError::Shape("shared worker process missing".into()))?;
        let held = self
            .page
            .call(&page::FrontcapGetSharedClockParams::default())
            .await?;
        self.page
            .call(&page::FrontcapSetSharedWorkerRequestPausedParams::new(
                shared.id.clone(),
                held.suspension_id,
                renderer.clone(),
                cdp::protocol::network::RequestId(request.to_owned()),
                pauses,
            ))
            .await?;
        Ok(())
    }

    pub(super) fn shared_session_waiting(&self, session: &str) -> bool {
        let Some(shared) = &self.shared_time else {
            return false;
        };
        self.interception_pages().iter().any(|(target, page)| {
            page.id == session
                && shared
                    .target_renderers
                    .get(target)
                    .is_some_and(|renderer| shared.arrivals.contains_key(renderer))
        })
    }

    /// Current-task drainage may itself run a task that starts a synchronous load.
    /// Keep the command receipt while recollecting with that new native frontier.
    pub(super) async fn shared_drain_current_tasks(
        &mut self,
        session: &Session,
    ) -> Result<bool, ChromiumError> {
        let existing = self
            .shared_time
            .as_mut()
            .unwrap()
            .pending_drains
            .remove(&session.id);
        let mut answer = match existing {
            Some(answer) => answer,
            None => session.send(&emulation::FrontcapDrainCurrentTasksParams::default())?,
        };
        let deadline = tokio::time::Instant::now() + self.watchdog;
        loop {
            self.service_shared_blockers().await?;
            let ready = answer.try_answer();
            if self.shared_session_waiting(&session.id) {
                if let Some(result) = ready {
                    result?;
                } else {
                    self.shared_time
                        .as_mut()
                        .unwrap()
                        .pending_drains
                        .insert(session.id.clone(), answer);
                }
                return Ok(false);
            }
            if let Some(result) = ready {
                result?;
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ChromiumError::CallTimeout("shared current-task drainage"));
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    fn poll_shared_drains(&mut self) -> Result<bool, ChromiumError> {
        let shared = self.shared_time.as_mut().unwrap();
        let mut completed = Vec::new();
        for (session, answer) in &mut shared.pending_drains {
            if let Some(result) = answer.try_answer() {
                result?;
                completed.push(session.clone());
            }
        }
        for session in completed {
            shared.pending_drains.remove(&session);
        }
        Ok(self
            .shared_time
            .as_ref()
            .unwrap()
            .pending_drains
            .keys()
            .all(|session| self.shared_session_waiting(session)))
    }

    pub(super) async fn shared_request_frontier(
        &self,
    ) -> Result<Option<SharedFrontier>, ChromiumError> {
        let Some(shared) = self.shared_time.as_ref().filter(|s| !s.arrivals.is_empty()) else {
            return Ok(None);
        };
        let deadline = tokio::time::Instant::now() + self.watchdog;
        loop {
            let state = self
                .page
                .call(&page::FrontcapGetSharedClockParams::default())
                .await?;
            if state.failed {
                return Err(ChromiumError::Shape(
                    "shared authority failed during arrival".into(),
                ));
            }
            let now = state
                .unix_time_us
                .parse::<i128>()
                .map_err(|_| ChromiumError::Shape("invalid shared instant".into()))?;
            let mut revisions = BTreeMap::new();
            let mut main_arrivals = BTreeMap::new();
            let mut blocked_revisions = BTreeMap::new();
            let mut workers = BTreeMap::new();
            for participant in &state.participants {
                if !shared.arrivals.contains_key(&participant.renderer_id) {
                    continue;
                }
                if let (Some(proposal), Some(revision)) = (
                    &participant.proposed_time_us,
                    &participant.proposal_revision,
                ) && proposal.parse::<i128>().is_ok_and(|p| p > now)
                {
                    revisions.insert(participant.renderer_id.clone(), revision.clone());
                    continue;
                }
                if participant.proposal_revision.is_none() {
                    continue;
                }
                // A worker load may keep the process proposal at now. Establish the
                // main thread's native arrival independently, then include every live
                // worker in collection instead of calling that blocked main thread.
                let mut books = vec![self.subs.workers.clone()];
                books.extend(
                    self.contexts
                        .lock()
                        .unwrap()
                        .popups
                        .iter()
                        .map(|p| p.workers.clone()),
                );
                let mut candidates: Vec<_> = books
                    .iter()
                    .flat_map(|book| {
                        book.lock()
                            .unwrap()
                            .sessions
                            .iter()
                            .filter(|(_, w)| {
                                w.enrollment == super::WorkerEnrollment::Ready
                                    && w.synchronous_wait.is_none()
                            })
                            .map(|(session, _)| session.clone())
                            .collect::<Vec<_>>()
                    })
                    .collect();
                candidates.sort();
                candidates.dedup();
                {
                    // When no application thread can answer, the independent native pipe
                    // reads joined task/enrollment votes without posting probe tasks.
                    // Retain the aggregate revision across collection in this path.
                    let snapshot = self
                        .page
                        .call(&page::FrontcapGetSharedProcessParams::new(
                            shared.id.clone(),
                            participant.renderer_id.clone(),
                        ))
                        .await?;
                    let expected = i128::from(self.profile.initial_date_ms) * 1000
                        + i128::from(shared.arrivals[&participant.renderer_id].due.0);
                    if !snapshot.tasks_pending
                        && snapshot
                            .main_thread_arrival_us
                            .as_deref()
                            .and_then(|t| t.parse::<i128>().ok())
                            == Some(expected)
                        && expected > now
                        && snapshot
                            .workers
                            .iter()
                            .all(|w| w.thread_started && w.synchronous_wait_id.is_some())
                    {
                        workers.extend(snapshot.workers.into_iter().map(|w| {
                            let worker = emulation::FrontcapWorker {
                                target_id: w.target_id,
                                creator_id: w.creator_id,
                                creator_is_worker: w.creator_is_worker,
                                sequence: w.sequence,
                                script_url: w.script_url,
                                thread_started: w.thread_started,
                                synchronous_wait_id: w.synchronous_wait_id,
                                frame_opportunity: w.frame_opportunity,
                                frame_receiver: None,
                            };
                            (worker.target_id.clone(), worker)
                        }));
                        blocked_revisions.insert(
                            participant.renderer_id.clone(),
                            participant
                                .proposal_revision
                                .clone()
                                .expect("checked native revision"),
                        );
                        main_arrivals.insert(participant.renderer_id.clone(), expected.to_string());
                        continue;
                    }
                    // Probe only runnable workers in this native process, rather
                    // than an unrelated process or a known synchronous waiter.
                    candidates.retain(|id| {
                        books.iter().any(|book| {
                            book.lock().unwrap().sessions.get(id).is_some_and(|worker| {
                                snapshot.workers.iter().any(|native| {
                                    native.target_id == worker.target
                                        && native.thread_started
                                        && native.synchronous_wait_id.is_none()
                                })
                            })
                        })
                    });
                }
                for id in candidates {
                    let session = Session::sharing_clock(
                        self.page.client.clone(),
                        id,
                        self.page.service_clock(),
                    );
                    let snapshot = session
                        .call(&emulation::FrontcapGetProcessWorkersParams::default())
                        .await?;
                    if snapshot.renderer_id != participant.renderer_id {
                        continue;
                    }
                    let expected = i128::from(self.profile.initial_date_ms) * 1000
                        + i128::from(shared.arrivals[&participant.renderer_id].due.0);
                    if snapshot
                        .main_thread_arrival_us
                        .as_deref()
                        .and_then(|t| t.parse::<i128>().ok())
                        != Some(expected)
                        || expected <= now
                    {
                        continue;
                    }
                    workers.extend(
                        snapshot
                            .workers
                            .into_iter()
                            .map(|w| (w.target_id.clone(), w)),
                    );
                    main_arrivals.insert(participant.renderer_id.clone(), expected.to_string());
                    break;
                }
            }
            if revisions.len() + main_arrivals.len() == shared.arrivals.len() {
                let sessions = self
                    .interception_pages()
                    .into_iter()
                    .filter_map(|(target, session)| {
                        shared
                            .target_renderers
                            .get(&target)
                            .filter(|r| shared.arrivals.contains_key(*r))
                            .map(|_| session.id)
                    })
                    .collect();
                return Ok(Some(SharedFrontier {
                    sessions,
                    revisions,
                    main_arrivals,
                    blocked_revisions,
                    worker_signature: serde_json::to_string(&workers).map_err(|e| {
                        ChromiumError::Shape(format!("invalid worker inventory: {e}"))
                    })?,
                    workers,
                }));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ChromiumError::CallTimeout(
                    "blocked renderer shared proposal frontier",
                ));
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    // Response completion may service another request before returning. Erase
    // that nested driver's type so recorder tasks can prove Send without
    // expanding the whole admission/request graph in each caller.
    pub(super) fn finish_shared_arrival(
        &mut self,
        request: fetch::RequestId,
        record: ResponseRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ChromiumError>> + Send + '_>> {
        Box::pin(self.finish_shared_arrival_inner(request, record))
    }

    async fn finish_shared_arrival_inner(
        &mut self,
        request: fetch::RequestId,
        record: ResponseRecord,
    ) -> Result<(), ChromiumError> {
        let target = self.target_of_fetch(&request);
        let shared = self.shared_time.as_mut().expect("shared arrival enabled");
        let renderer = shared
            .target_renderers
            .get(&target)
            .ok_or_else(|| ChromiumError::Shape("shared arrival owner missing".into()))?;
        shared
            .arrivals
            .get_mut(renderer)
            .ok_or_else(|| ChromiumError::Shape("shared arrival missing".into()))?
            .completion = Some((request, record));
        if shared.servicing_arrivals {
            return Ok(());
        }
        shared.servicing_arrivals = true;
        let result = self.drive_shared_arrivals().await;
        self.shared_time.as_mut().unwrap().servicing_arrivals = false;
        result
    }

    async fn service_shared_blockers(&mut self) -> Result<(), ChromiumError> {
        self.take_paused()?;
        while let Some(index) = self
            .queued
            .iter()
            .position(|(_, event)| self.blocks_main_thread(event))
        {
            let (_, event) = self.queued.remove(index);
            Box::pin(self.serve_blocking(event)).await?;
            self.take_paused()?;
        }
        Ok(())
    }

    async fn drive_shared_arrivals(&mut self) -> Result<(), ChromiumError> {
        self.next_shared_control_revision()?;
        let owner = self.page.clone();
        let clock = self.shared_time.as_ref().unwrap().id.clone();
        let mut pausing: BTreeMap<String, cdp::Answer> = BTreeMap::new();
        let mut granting: BTreeMap<String, cdp::Answer> = BTreeMap::new();
        let mut paused = std::collections::BTreeSet::new();
        let mut last = self.now;
        let mut deadline = tokio::time::Instant::now() + self.watchdog;
        loop {
            let held = owner
                .call(&page::FrontcapSuspendSharedClockParams::new(clock.clone()))
                .await?;
            let activity = self.shared_policy_activity();
            let instant = held
                .unix_time_us
                .parse::<i128>()
                .map_err(|_| ChromiumError::Shape("invalid shared instant".into()))?;
            self.now = VirtualTime(
                u64::try_from(instant - i128::from(self.profile.initial_date_ms) * 1000)
                    .map_err(|_| ChromiumError::Shape("shared instant outside timeline".into()))?,
            );
            if self.now != last {
                last = self.now;
                deadline = tokio::time::Instant::now() + self.watchdog;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ChromiumError::CallTimeout("shared response arrival"));
            }
            // Responses are completed by native deadline, not Rust call-stack order.
            let mut completed: Vec<_> = self
                .shared_time
                .as_ref()
                .unwrap()
                .arrivals
                .iter()
                .filter(|(_, a)| a.due <= self.now)
                .map(|(r, a)| (a.order, r.clone()))
                .collect();
            completed.sort_by_key(|(order, _)| *order);
            for (_, renderer) in completed {
                let arrival = self
                    .shared_time
                    .as_mut()
                    .unwrap()
                    .arrivals
                    .remove(&renderer)
                    .unwrap();
                if arrival.due != self.now {
                    return Err(ChromiumError::Shape(
                        "shared arrival passed its deadline".into(),
                    ));
                }
                let (request, record) = arrival.completion.ok_or_else(|| {
                    ChromiumError::Shape("shared arrival reached before fulfillment".into())
                })?;
                self.receipts_awaiting.push((record.id.clone(), request.0));
                if let Some(q) = self.quiet.as_mut() {
                    q.end(&format!("request:{}", record.id), self.now);
                }
                self.record(self.now, EventKind::Response(record));
                self.clock_record(
                    ClockOp::SynchronousRequest,
                    ClockPhase::End,
                    self.now,
                    Some(ClockOutcome::Reached),
                    None,
                );
                self.arrived = true;
            }
            // Another runnable renderer can create a window during this wait.
            // Enroll it while publication is held, then use the refreshed set for
            // every pause/grant round instead of retaining the entry-time set.
            let blocking = std::mem::replace(&mut self.blocking, false);
            let admitted = self.admit_shared_pages(&held, activity).await;
            self.blocking = blocking;
            if !admitted? {
                continue;
            }
            self.retire_shared_pages().await?;
            let participants = self.shared_time.as_ref().unwrap().participants.clone();
            // Native retirement above proves these renderer commands no longer
            // own work. Their detached sessions cannot supply control receipts.
            pausing.retain(|renderer, _| participants.contains_key(renderer));
            granting.retain(|renderer, _| participants.contains_key(renderer));
            paused.retain(|renderer| participants.contains_key(renderer));
            self.service_shared_blockers().await?;
            let drains_ready = self.poll_shared_drains()?;
            // A pause sent before its Fetch notification may sit behind a synchronous
            // wait. Retain its answer and service that wait instead of awaiting it here.
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
            let mut granted = Vec::new();
            for (renderer, answer) in &mut granting {
                if let Some(result) = answer.try_answer() {
                    result?;
                    granted.push(renderer.clone());
                }
            }
            for renderer in granted {
                granting.remove(&renderer);
            }
            let blocked = &self.shared_time.as_ref().unwrap().arrivals;
            for (renderer, session) in &participants {
                if !blocked.contains_key(renderer)
                    && !paused.contains(renderer)
                    && !pausing.contains_key(renderer)
                    && !granting.contains_key(renderer)
                {
                    pausing.insert(
                        renderer.clone(),
                        session.send(&emulation::SetVirtualTimePolicyParams {
                            bind_storage_clock: Some(session.id == owner.id),
                            ..emulation::SetVirtualTimePolicyParams::new(
                                emulation::VirtualTimePolicy::Pause,
                            )
                        })?,
                    );
                }
            }
            if !drains_ready
                || participants
                    .keys()
                    .any(|r| !blocked.contains_key(r) && !paused.contains(r))
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
                continue;
            }
            let blocking = std::mem::replace(&mut self.blocking, false);
            let serviced: Result<bool, ChromiumError> = async {
                self.handle_queued().await?;
                self.deliver_due().await?;
                // Pause/drain receipts can follow a window-open notification
                // before its asynchronous enrollment task updates the page book.
                self.settle_shared_window_enrollment().await?;
                self.admit_shared_pages(&held, activity).await
            }
            .await;
            self.blocking = blocking;
            if !serviced? {
                continue;
            }
            if self
                .shared_time
                .as_ref()
                .unwrap()
                .participants
                .keys()
                .ne(participants.keys())
            {
                // The new renderer must join the pause round before any grant.
                continue;
            }
            let done = self.shared_time.as_ref().unwrap().arrivals.is_empty();
            self.release_shared_prefixes(&paused).await;
            if done && self.event_stop.is_some() {
                self.commit()?;
                if self.event_stop.as_ref().is_some_and(|stop| !stop.triggers.is_empty()) {
                    // All arrivals and current-task boundaries are complete.
                    // A reached event stop must not renew the old segment grant.
                    let shared = self.shared_time.as_mut().unwrap();
                    shared.retained_bound = None;
                    shared.unreleased_prefixes.clear();
                    return Ok(());
                }
            }
            let shared = self.shared_time.as_ref().unwrap();
            let retained = shared
                .retained_bound
                .expect("arrival retains its original bound");
            let due = shared.arrivals.values().map(|a| a.due).min();
            let next = due.map_or(retained, |t| {
                let next = self.net.agenda_next().map_or(t, |n| n.min(t)).max(self.now);
                i128::from(self.profile.initial_date_ms) * 1000 + i128::from(next.0)
            });
            let policy = if self.net.held.values().any(|h| !h.released) {
                emulation::VirtualTimePolicy::Advance
            } else {
                emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending
            };
            // A new synchronous request can overtake a grant even after Pause replied:
            // the current task continues after its previous synchronous response. Keep
            // the grant reply by renderer; a registered arrival may progress under the
            // native bound without waiting for that blocked renderer's inspector.
            // Before any later grant, acknowledge this command and pause again while
            // publication is held, replacing its now-stale relative budget.

            for (renderer, session) in &participants {
                if !shared.arrivals.contains_key(renderer) {
                    granting.insert(
                        renderer.clone(),
                        session.send(&emulation::SetVirtualTimePolicyParams {
                            budget: Some((next - instant) as f64 / 1000.0),
                            bind_storage_clock: Some(session.id == owner.id),
                            max_virtual_time_task_starvation_count: Some(i64::from(
                                self.profile.starvation_limit,
                            )),
                            ..emulation::SetVirtualTimePolicyParams::new(policy)
                        })?,
                    );
                    paused.remove(renderer);
                }
            }
            loop {
                self.service_shared_blockers().await?;
                let mut granted = Vec::new();
                for (renderer, answer) in &mut granting {
                    if let Some(result) = answer.try_answer() {
                        result?;
                        granted.push(renderer.clone());
                    }
                }
                for renderer in granted {
                    granting.remove(&renderer);
                }
                if granting.keys().all(|renderer| {
                    self.shared_time
                        .as_ref()
                        .unwrap()
                        .arrivals
                        .contains_key(renderer)
                }) {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    return Err(ChromiumError::CallTimeout("shared paused renderer grant"));
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            // A new arrival discovered while sending grants can only shorten this
            // browser bound. Native proposals still retain all earlier work.
            let shared = self.shared_time.as_ref().unwrap();
            let next = shared
                .arrivals
                .values()
                .map(|a| i128::from(self.profile.initial_date_ms) * 1000 + i128::from(a.due.0))
                .min()
                .map_or(next, |arrival| next.min(arrival));
            owner
                .call(&page::FrontcapResumeSharedClockParams {
                    bound_us: Some(next.to_string()),
                    ..page::FrontcapResumeSharedClockParams::new(clock.clone(), held.suspension_id)
                })
                .await?;
            if done && shared.arrivals.is_empty() {
                let shared = self.shared_time.as_mut().unwrap();
                shared.retained_bound = None;
                shared.unreleased_prefixes.clear();
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
}

pub(super) struct SharedFrontier {
    pub(super) sessions: Vec<String>,
    pub(super) workers: BTreeMap<String, emulation::FrontcapWorker>,
    revisions: BTreeMap<String, String>,
    worker_signature: String,
    main_arrivals: BTreeMap<String, String>,
    blocked_revisions: BTreeMap<String, String>,
}

impl PartialEq for SharedFrontier {
    fn eq(&self, other: &Self) -> bool {
        self.sessions == other.sessions
            && self.revisions == other.revisions
            && self.worker_signature == other.worker_signature
            && self.main_arrivals == other.main_arrivals
            && self.blocked_revisions == other.blocked_revisions
    }
}

/// Adapter-owned evidence that a serviced command has not replaced this round.
/// Native target routing is checked independently; it may change at fixed time.
#[derive(PartialEq, Eq)]
struct SharedPolicyState {
    now: VirtualTime,
    control_revision: u64,
    next_arrival: u64,
    targets: BTreeMap<String, String>,
    participants: BTreeMap<String, String>,
    pages: BTreeMap<String, String>,
}

#[cfg(test)]
#[path = "shared_time_tests.rs"]
mod tests;
