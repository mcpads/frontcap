use super::*;

/// Only `Granted` authorizes a caller to await a new budget-expired receipt.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum PolicyOutcome {
    Granted,
    Held,
}

impl Generation {
    /// Establish a watermark while frozen, before the recorder publishes dispatch or grants time.
    pub async fn arm_event(&mut self, event: ProgressEvent) -> Result<EventArm, ChromiumError> {
        self.alive()?;
        if !self.patched {
            return Err(ChromiumError::Unsupported("next-event stops require the patched engine".into()));
        }
        self.drain().await?;
        // Do not resolve queued interceptions while arming: those deliveries belong to the
        // dispatched progress request, not its pre-dispatch preparation.
        self.commit()?;
        let presentation = if event == ProgressEvent::Presentation {
            match self.presented_frame().await {
                Ok(frame) => Some(frame),
                Err(ChromiumError::NoFrame | ChromiumError::StaleFrame) => None,
                Err(e) => return Err(e),
            }
        } else { None };
        let delivers_from = if event == ProgressEvent::Response { self.undelivered_response()? } else { None };
        Ok(EventArm { after_seq: self.next_seq(), presentation, delivers_from })
    }

    /// The first response recorded since the last progress ended. Page callbacks run only while time is
    /// granted, so such a response has been handed to the browser but not yet delivered to the page.
    fn undelivered_response(&self) -> Result<Option<u64>, ChromiumError> {
        self.read_trace(|events| {
            let since = events.iter().rev()
                .find(|e| matches!(&e.kind, EventKind::Clock(c) if c.phase == ClockPhase::End))
                .map_or(0, |e| e.seq + 1);
            events.iter().find(|e| e.seq >= since && matches!(&e.kind, EventKind::Response(r) if r.source != ResponseSource::Preflight)).map(|e| e.seq)
        })
    }

    /// Resume from the original watermark and absolute budget, including a trigger already recorded
    /// before a recorder fault. No new event is consumed merely because the controller reconnected.
    pub async fn next_event(&mut self, event: ProgressEvent, arm: &EventArm, goal: VirtualTime) -> Result<ClockOutcome, ChromiumError> {
        if event != ProgressEvent::Dialog { return self.next_event_inner(event, arm, goal).await; }
        let rules = self.dialog_rules.lock().unwrap().clone();
        self.set_dialog_rules(frontcap_core::dialog::DialogRules::hold_all()).await?;
        let result = self.next_event_inner(event, arm, goal).await;
        let restored = self.set_dialog_rules(rules).await;
        let outcome = result?;
        restored?;
        Ok(outcome)
    }

    async fn next_event_inner(&mut self, event: ProgressEvent, arm: &EventArm, goal: VirtualTime) -> Result<ClockOutcome, ChromiumError> {
        self.alive()?;
        if !self.patched {
            return Err(ChromiumError::Unsupported("next-event stops require the patched engine".into()));
        }
        let mut triggers = Vec::new();
        // A response handed over while stopped is delivered by this wait's progress: it counts, but the
        // wait still runs that delivery before stopping.
        let undelivered = arm.delivers_from.filter(|_| event == ProgressEvent::Response);
        if event != ProgressEvent::Presentation {
            let from = undelivered.unwrap_or(arm.after_seq).min(arm.after_seq);
            triggers.extend(self.read_trace(|events| {
                events.iter().filter(|e| e.seq >= from).filter_map(|e| event.trace_trigger(e.seq, e.t_us, &e.kind)).collect::<Vec<_>>()
            })?);
        } else {
            match self.presented_frame().await {
                Ok(frame) if Some(&frame) != arm.presentation.as_ref() => triggers.push(ProgressTrigger::Presentation {
                    t_us: self.now, painted_at_us: frame.painted_at, document: self.documents,
                    context: self.popup_id().unwrap_or_else(|| super::PAGE.into()),
                }),
                Ok(_) | Err(ChromiumError::NoFrame | ChromiumError::StaleFrame) => {},
                Err(e) => return Err(e),
            }
        }
        let wall = Instant::now();
        let observer_stop = matches!(event, ProgressEvent::Navigation | ProgressEvent::Exception | ProgressEvent::Focus);
        if observer_stop && triggers.is_empty() {
            let installed: Result<(), ChromiumError> = async {
                let expr = match event {
                    ProgressEvent::Focus => "globalThis.__frontcap.stopAtFocus()",
                    ProgressEvent::Exception => "globalThis.__frontcap.stopAtException()",
                    _ => "globalThis.__frontcap.stopAtNavigation()",
                };
                if let Some(id) = self.stop_script.take() {
                    self.page.call(&page::RemoveScriptToEvaluateOnNewDocumentParams::new(id)).await?;
                }
                self.stop_script = Some(self.world_script(expr).await?);
                if let Some(world) = self.world.clone() { self.page.eval(expr, Some(world)).await?; }
                Ok(())
            }.await;
            if let Err(e) = installed {
                let _ = self.set_stop(None).await;
                return Err(e);
            }
        }
        self.clock_record(ClockOp::Next, ClockPhase::Start, goal, None, None);
        self.event_stop = Some(EventStop { event, after_seq: arm.after_seq, triggers });
        // Recovery re-reads the trace: once the delivery ran, a later progress has ended and the
        // original triggers stand on their own.
        let delivered = undelivered.is_some_and(|from| self.undelivered_response().ok().flatten() != Some(from));
        let run = if self.event_hit() && (undelivered.is_none() || delivered) { Ok(RunEnd::Stopped) } else { self.run_cancellable(goal, None).await };
        let cleared = if observer_stop { self.set_stop(None).await } else { Ok(()) };
        let stop = self.event_stop.take().expect("armed next-event wait");
        let run = run?;
        cleared?;
        let outcome = if matches!(run, RunEnd::Cancelled) { ClockOutcome::Cancelled }
            else if stop.triggers.is_empty() { ClockOutcome::EventTimedOut { event } }
            else { ClockOutcome::Event { event, triggers: stop.triggers } };
        self.clock_record(ClockOp::Next, ClockPhase::End, goal, Some(outcome.clone()), Some(wall));
        self.commit()?;
        Ok(outcome)
    }

    pub(super) fn event_hit(&self) -> bool {
        self.event_stop.as_ref().is_some_and(|s| !s.triggers.is_empty())
    }

    /// Finishes the caller-authorized input interval a fault interrupted, without renewing its budget.
    pub async fn finish_input_progress(&mut self, goal: VirtualTime) -> Result<(), ChromiumError> {
        self.alive()?;
        if self.now >= goal {
            return Ok(());
        }
        let wall = Instant::now();
        self.clock_record(ClockOp::Drag, ClockPhase::Start, goal, None, None);
        self.run_until(goal, None).await?;
        self.clock_record(ClockOp::Drag, ClockPhase::End, goal, Some(ClockOutcome::Reached), Some(wall));
        self.commit()
    }

    pub(super) async fn policy(&mut self, p: emulation::VirtualTimePolicy, budget: Option<VirtualDuration>) -> Result<PolicyOutcome, ChromiumError> {
        // Freeze the requested endpoint before any observer command can service a
        // synchronous arrival. Such service may finish beyond this endpoint; it
        // never renews the caller's original relative budget.
        let endpoint = budget.map(|by| self.now.saturating_add(by));
        let current_tasks_only = budget.is_some_and(|by| by.0 == 0);
        // Budgets are granted, and expire, on the launch page's session.
        if let Some(context) = self.popup_id() {
            return Err(ChromiumError::Unsupported(format!("virtual time progresses with the launch page addressed, not {context}")));
        }
        if self.shared_time.is_none() {
            return self.install_policy(p, budget, endpoint, current_tasks_only).await;
        }
        let owner = self.page.clone();
        let id = self.shared_time.as_ref().expect("shared policy enabled").clock_id().to_owned();
        let service = owner.service_clock();
        // This single deadline starts before the first progress-capable observer
        // write and remains in force through every nested arrival and recollection.
        // Agent decision holds park active service, without restarting the budget.
        let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
        let result = match service.timeout_at(deadline,
            self.install_policy(p, budget, endpoint, current_tasks_only)).await {
            Ok(result) => result,
            Err(_) => Err(ChromiumError::CallTimeout("shared policy reconciliation")),
        };
        if result.is_err() {
            // Cancellation may bypass nested Rust cleanup, and serviced work may
            // have consumed an earlier native hold. Do not reuse this generation.
            // Cleanup gets a separate wall-clock bound: a remaining agent hold
            // must not park this browser-owned suspension attempt indefinitely.
            self.dead = true;
            match tokio::time::timeout(Session::CALL_TIMEOUT,
                owner.call(&page::FrontcapSuspendSharedClockParams::new(id))).await {
                Ok(Ok(_)) => (),
                Ok(Err(error)) => eprintln!("shared policy failure could not retain publication hold: {error}"),
                Err(_) => eprintln!("shared policy failure publication hold attempt timed out"),
            }
        }
        result
    }

    async fn install_policy(
        &mut self,
        p: emulation::VirtualTimePolicy,
        budget: Option<VirtualDuration>,
        endpoint: Option<VirtualTime>,
        current_tasks_only: bool,
    ) -> Result<PolicyOutcome, ChromiumError> {
        if p != emulation::VirtualTimePolicy::Pause {
            self.timeline.moving(crate::frames::wall_now(), self.documents, self.changes);
        }
        // The end of each grant is kept in the observer world, which outlives the session (S11): a
        // relaunched recorder measures a runaway (S13) against what the engine was actually granted.
        // A task running when the policy changes may be waiting on a synchronous request (engine patch 53).
        self.take_contexts()?;
        let page = self.page.clone();
        if let Some(endpoint) = endpoint {
            let end = endpoint.0;
            // Only an actually completed synchronous arrival may raise this
            // request's watermark, including the known current instant after a
            // document replacement. An old, superseded future grant may not.
            while let Some(world) = self.world_unique.clone() {
                match self.serviced(page.eval_unique(&format!("(() => {{ const state = globalThis.__frontcap; const end = BigInt('{end}'); const known = BigInt('{}'); const arrived = BigInt(state.synchronousArrivalUs ?? '0'); const completed = arrived > known ? arrived : known; state.grantedUs = String(completed > end ? completed : end); }})()", self.now.0), &world)).await {
                    Ok(_) => break,
                    Err(error) => self.retire_missing_observer(&world, error)?,
                }
            }
        }
        let params = emulation::SetVirtualTimePolicyParams {
            budget: budget.map(|by| by.as_micros() as f64 / 1000.0),
            stop_on_dialog: self.patched.then_some(self.event_stop.as_ref().is_some_and(|s| s.event == ProgressEvent::Dialog)),
            // The engine accepts a starvation limit only for policies that let time advance.
            max_virtual_time_task_starvation_count: (p != emulation::VirtualTimePolicy::Pause)
                .then_some(i64::from(self.profile.starvation_limit)),
            ..emulation::SetVirtualTimePolicyParams::new(p)
        };
        if self.shared_time.is_some() {
            return self.shared_policy(params, endpoint, current_tasks_only).await;
        }
        self.serviced(page.call(&params)).await?;
        Ok(if p == emulation::VirtualTimePolicy::Pause { PolicyOutcome::Held } else { PolicyOutcome::Granted })
    }

    /// Response and stream stops still deliver the current instant's callbacks.
    /// Other event stops and predicate stop requests retain their frozen boundary.
    pub(super) fn policy_stop_reached(&self, current_tasks_only: bool) -> bool {
        self.stop_hit.is_some() || (self.event_hit() && (!current_tasks_only
            || self.event_stop.as_ref().is_none_or(|stop| !matches!(stop.event,
                ProgressEvent::Response | ProgressEvent::Stream))))
    }

    /// Handles requests already issued but not yet intercepted, so an operation ends with every event of
    /// its interval in the trace. One whose interception does not arrive within `INTERCEPTION_WAIT` of
    /// wall time stops holding the trace back and is recorded as taint.
    pub(super) async fn settle_issued(&mut self) -> Result<(), ChromiumError> {
        // Each request waited for from its own announcement; the whole wait is bounded too.
        let deadline = tokio::time::Instant::now() + Session::CALL_TIMEOUT;
        loop {
            self.drain_sent(Duration::ZERO).await?;
            self.take_paused()?;
            self.handle_queued().await?;
            self.drain_network().await?;
            if self.net.interceptions.is_empty() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                self.give_up_interceptions();
                break;
            }
            if let Some(by) = Instant::now().checked_sub(INTERCEPTION_WAIT) {
                let gone = self.net.interceptions.give_up_before(cdp::Arrival(u64::MAX), Some(by));
                self.record_given_up(gone);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        self.commit()
    }

    /// Handles the interceptions of requests already issued that arrive within `within`, without giving
    /// up on the others.
    pub(super) async fn take_issued(&mut self, within: Duration) -> Result<(), ChromiumError> {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            self.drain_sent(Duration::ZERO).await?;
            self.take_paused()?;
            self.handle_queued().await?;
            self.drain_network().await?;
            if self.net.interceptions.is_empty() || tokio::time::Instant::now() >= deadline {
                // Observer reports made after a request was issued waited for its handling: publish them.
                return self.drain_bindings(Duration::ZERO).await;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Lets the observer drop reports that are in the trace. Best effort: an unacknowledged report is
    /// only read back, and skipped by its `rseq`, after a relaunch.
    pub(super) async fn acknowledge(&self) {
        if let Some(world) = self.world.clone() {
            let _ = self.page.eval(&format!("globalThis.__frontcap.ack({})", self.written_rseq), Some(world)).await;
        }
    }

    /// The patched engine exposes its exact native microsecond clock. Date.now()
    /// truncates fractional budgets and is retained only for the baseline fallback.
    /// Take new document contexts first for that fallback's observer-world read.
    pub(super) async fn read_time(&mut self) -> Result<VirtualTime, ChromiumError> {
        self.take_contexts()?;
        if self.patched {
            if self.shared_time.is_some() {
                let time = self.page.call(&page::FrontcapGetSharedClockParams::default()).await?;
                let epoch = time.unix_time_us.parse::<i128>().map_err(|_| ChromiumError::Shape("invalid shared time".into()))?;
                return u64::try_from(epoch - i128::from(self.profile.initial_date_ms)*1000).map(VirtualTime)
                    .map_err(|_| ChromiumError::Shape("shared time outside timeline".into()));
            }
            let page = self.page.clone();
            let time = self.serviced(page.call(&emulation::GetVirtualTimeParams::default())).await?;
            let epoch = time.unix_time_us.parse::<i128>().map_err(|_| ChromiumError::Shape("invalid native virtual clock".into()))?;
            let elapsed = epoch - i128::from(self.profile.initial_date_ms) * 1000;
            return u64::try_from(elapsed).map(VirtualTime).map_err(|_| ChromiumError::Shape("native virtual clock outside the generation timeline".into()));
        }
        let v = self.page.eval("Date.now()", self.world.clone()).await?;
        let ms = v.as_f64().ok_or_else(|| ChromiumError::Shape(v.to_string()))?;
        Ok(ms_epoch_to_virtual(ms, self.profile.initial_date_ms))
    }

    pub(super) fn clock_record(&mut self, op: ClockOp, phase: ClockPhase, goal: VirtualTime, outcome: Option<ClockOutcome>, wall: Option<Instant>) {
        self.record(self.now, EventKind::Clock(ClockRecord {
            op,
            phase,
            goal_us: Some(goal),
            outcome,
            wall_ms: wall.map(|w| w.elapsed().as_millis() as u64),
        }));
    }

    pub async fn advance(&mut self, by: VirtualDuration) -> Result<ClockOutcome, ChromiumError> {
        self.alive()?;
        let wall = Instant::now();
        let goal = self.now.saturating_add(by);
        self.clock_record(ClockOp::Advance, ClockPhase::Start, goal, None, None);
        let outcome = match self.run_cancellable(goal, None).await? {
            RunEnd::Cancelled => ClockOutcome::Cancelled,
            _ => ClockOutcome::Reached,
        };
        self.clock_record(ClockOp::Advance, ClockPhase::End, goal, Some(outcome.clone()), Some(wall));
        self.commit()?;
        Ok(outcome)
    }

    /// Complete already-admitted deliveries at this instant before returning a progress boundary.
    /// The zero-length grant permits loading callbacks, but its native fence forbids time advance.
    pub(super) async fn finish_current_deliveries(&mut self) -> Result<(), ChromiumError> {
        if !self.patched {
            return Ok(());
        }
        // A due page task can start a resource before its Fetch/Network announcement reaches
        // the adapter. Service native loading even when the adapter has no outstanding id yet;
        // otherwise a frame at this instant can wait forever on that unannounced resource.
        while self.subs.expired.try_next()?.is_some() {}
        let started = self.page.service_clock().elapsed();
        let mut outcome = self.policy(emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending, Some(VirtualDuration::from_micros(0))).await?;
        loop {
            if outcome == PolicyOutcome::Held {
                // A nested synchronous completion may have consumed this zero
                // grant's instant, or reached an event stop without issuing a
                // grant. Neither case has a new expiry for us to await.
                if self.policy_stop_reached(true) {
                    self.drain().await?;
                    return Ok(());
                }
                if self.page.service_clock().elapsed().saturating_sub(started) > self.watchdog {
                    return self.unresponsive(self.now).await;
                }
                while self.subs.expired.try_next()?.is_some() {}
                outcome = self.policy(emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending, Some(VirtualDuration::from_micros(0))).await?;
                continue;
            }
            match self.wait(started, true).await? {
                Wait::Expired => {
                    // Budget expiry alone does not include a newly released
                    // worker's enrollment and startup tasks. Close their native
                    // boundary before reporting delivery at this instant.
                    let mut pages = vec![self.page.id.clone()];
                    pages.extend(self.contexts.lock().unwrap().popups.iter()
                        .filter(|p| !p.closed && !p.held).map(|p| p.session_id.clone()));
                    pages.sort();
                    pages.dedup();
                    for page in pages {
                        let session = Session::sharing_clock(self.page.client.clone(), page, self.page.service_clock());
                        self.serviced(session.call(&emulation::FrontcapDrainCurrentTasksParams::default())).await?;
                    }
                    let at = self.read_time().await?;
                    if at != self.now {
                        self.dead = true;
                        return Err(ChromiumError::Shape(format!("delivery at {} us advanced to {} us", self.now.0, at.0)));
                    }
                    self.recorded_pause().await?;
                    self.drain().await?;
                    return Ok(());
                }
                Wait::Request(ev) => {
                    self.handle_request(*ev).await?;
                    if self.event_stop.is_some() { self.commit()?; }
                    let reached_stop = self.event_hit() && self.event_stop.as_ref().is_none_or(|s| !matches!(s.event, ProgressEvent::Response | ProgressEvent::Stream));
                    if reached_stop || self.stop_hit.is_some() {
                        self.policy(emulation::VirtualTimePolicy::Pause, None).await?;
                        self.drain().await?;
                        return Ok(());
                    }
                }
                Wait::Quiet => {
                    if self.shared_window_boundary().await? {
                        outcome = self.policy(emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending, Some(VirtualDuration::from_micros(0))).await?;
                    }
                },
                Wait::Answer(a) => self.answer_blocking(a).await,
                Wait::Watchdog => return self.unresponsive(self.now).await,
            }
        }
    }

    pub(super) async fn run_until(&mut self, goal: VirtualTime, stop: Option<&WatchName>) -> Result<RunEnd, ChromiumError> {
        let previous = std::mem::replace(&mut self.progressing, true);
        let previous_watch = std::mem::replace(&mut self.frame_watch, stop.cloned());
        let result = self.run_until_inner(goal, stop, true).await;
        self.frame_watch = previous_watch;
        self.progressing = previous;
        // A stop belongs to this progress request, including when its frame refuses completion.
        if result.is_err() { self.frame_stop = None; }
        let end = result?;
        self.worker_enrollment_result()?;
        Ok(end)
    }

    /// A wait's progress, which the controller may cancel.
    pub(super) async fn run_cancellable(&mut self, goal: VirtualTime, stop: Option<&WatchName>) -> Result<RunEnd, ChromiumError> {
        let previous = std::mem::replace(&mut self.cancellable, true);
        let result = self.run_until(goal, stop).await;
        self.cancellable = previous;
        result
    }

    /// Set from any task to cancel the running wait at its next frame tick.
    pub fn cancel_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
        self.cancel_requested.clone()
    }

    /// Read from any task: the virtual microseconds a running wait has reached, at its last stop.
    pub fn progress_handle(&self) -> std::sync::Arc<std::sync::atomic::AtomicU64> {
        self.progress.clone()
    }

    /// The instant the next request's waits are cancelled at, as a replay of a cancelled request arms it;
    /// none for an uncancelled one.
    pub fn arm_cancel(&mut self, at: Option<VirtualTime>) {
        self.cancel_at = at;
        self.cancelled_at = None;
    }

    /// Where the last request's wait stopped by cancellation, if it did.
    pub fn take_cancelled(&mut self) -> Option<VirtualTime> {
        self.cancel_at = None;
        self.cancelled_at.take()
    }

    /// Service a matched worker's response while its original native frame
    /// command retains ownership. Advancing cannot authorize another frame batch.
    pub(super) async fn progress_before_frame_completion(&mut self, goal: VirtualTime) -> Result<RunEnd, ChromiumError> {
        let watch = self.frame_watch.clone();
        self.run_until_inner(goal, watch.as_ref(), false).await
    }

    async fn run_until_inner(&mut self, goal: VirtualTime, stop: Option<&WatchName>, render_frames: bool) -> Result<RunEnd, ChromiumError> {
        // The watchdog bounds active host service without progress, excluding agent decision holds.
        // It does not bound the whole run, which many short segments can legitimately exceed.
        let service = self.page.service_clock();
        let mut started = service.elapsed();
        self.resume_interceptions().await?;
        loop {
            self.progress.store(self.now.0, std::sync::atomic::Ordering::Relaxed);
            // What was recorded before this stop: an idle frame tick counts what the stop records as new.
            let recorded = self.recorded;
            self.after_arrival().await?;
            self.drain().await?;
            // Requests that arrived during an `advance` segment are handled at its end, a fixed instant.
            self.take_paused()?;
            let handled = !self.queued.is_empty();
            self.handle_queued().await?;
            if handled {
                // Handling may wait on the wall clock (a stream's handshake): that is not the engine's.
                started = service.elapsed();
            }
            self.deliver_due().await?;
            // An already-reached dialog/navigation/predicate stop must not start the next
            // document's callbacks merely because its response is outstanding at this instant.
            if self.event_stop.is_some() { self.commit()?; }
            let condition_hit = self.stop_hit.as_ref().is_some_and(|hit|
                stop == Some(hit) && self.last_sample(hit) == Some(&SampleValue::True));
            // A response or stream trigger records transport fulfillment; the instant's other deliveries
            // and their callbacks still belong to that stop. Navigation/dialog triggers instead stop
            // before new page work.
            let response_stop = self.event_stop.as_ref().is_some_and(|s| matches!(s.event, ProgressEvent::Response | ProgressEvent::Stream));
            if (!self.event_hit() || response_stop) && !condition_hit {
                self.finish_current_deliveries().await?;
                // Stream callbacks must follow native response callbacks at the same instant,
                // including streams opened while the delivery barrier serviced new requests.
                self.deliver_due().await?;
                // Server slots follow scripted agenda events, as recorded stream replay does.
                self.deliver_upstream().await?;
            }
            // Response delivery is a serviceable stop before presentation. Retain every coincident
            // response; do not authorize an extra render after the requested boundary was reached.
            if self.event_stop.is_some() { self.commit()?; }
            if self.event_hit() {
                self.sample_frozen().await?;
                self.settle_issued().await?;
                return Ok(RunEnd::Stopped);
            }
            // Engine patch 6: the page renders at frame ticks, after the instant's tasks and deliveries.
            if render_frames {
                if self.pending_render.is_some() { self.render_frame(recorded, started).await?; }
                else { self.render_at_tick(recorded, started).await?; }
                if let Some(end) = self.frame_stop.take() {
                    self.commit()?;
                    self.acknowledge().await;
                    self.sample_frozen().await?;
                    self.settle_issued().await?;
                    return Ok(end);
                }
            }
            self.commit()?;
            self.acknowledge().await;
            if self.event_hit() {
                self.sample_frozen().await?;
                self.settle_issued().await?;
                return Ok(RunEnd::Stopped);
            }
            // A stop request names a watch; it ends the wait only when that watch's observer has recorded it
            // true. Requests page script makes to the stop address carry no sample and change nothing. One
            // handled while a frame rendered comes after this stop's drain: the sample before it is read first.
            if self.stop_hit.is_some() {
                self.drain().await?;
            }
            if let Some(hit) = self.stop_hit.take()
                && stop == Some(&hit)
                && self.last_sample(&hit) == Some(&SampleValue::True)
            {
                self.sample_frozen().await?;
                self.settle_issued().await?;
                return Ok(RunEnd::Stopped);
            }
            // A cancellation stops the wait at the frame tick after the run first saw it, reached as a goal is,
            // so a replay that arms the same instant stops there by the same path. It wins over a goal at
            // the same instant.
            if self.cancellable {
                if self.cancel_at.is_none() && self.cancel_requested.load(std::sync::atomic::Ordering::SeqCst) {
                    let frame = VirtualDuration(self.profile.frame_interval_us.max(1));
                    self.cancel_at = Some(frontcap_core::clock::frame_tick(frontcap_core::clock::frame_slot(self.now, frame) + 1, frame));
                }
                if let Some(at) = self.cancel_at
                    && self.now >= at
                {
                    self.sample_frozen().await?;
                    self.settle_issued().await?;
                    self.cancelled_at = Some(at);
                    return Ok(RunEnd::Cancelled);
                }
            }
            if self.now >= goal {
                self.sample_frozen().await?;
                self.settle_issued().await?;
                return Ok(RunEnd::Reached);
            }
            if service.elapsed().saturating_sub(started) > self.watchdog {
                return self.unresponsive(goal).await;
            }
            // Held requests must not stop virtual time, so `advance` is used while any exists, in segments
            // that end before any held request's deadline; otherwise the engine pauses at the start of each
            // new fetch, exactly at its issue time.
            if self.starving && self.net.answered.is_empty() {
                self.starving = false;
            }
            // A held request the engine released (patch 1) needs no segments: a new fetch stops the clock.
            let holding = self.net.held.values().any(|h| !h.released) || self.starving;
            let mut max_segment = holding.then(|| self.net.min_latency().map_or(HOLD_QUANTUM, |l| l.min(HOLD_QUANTUM)));
            if self.starving {
                // One frame interval on the platform's millisecond frame grid.
                let frame = VirtualDuration(self.profile.frame_interval_us.max(1).div_ceil(1000) * 1000);
                max_segment = max_segment.map(|m| m.min(frame));
            }
            // A passed-through stream's messages are delivered at slots on the frame interval (time-model.md,
            // Streams): the run stops at each.
            let frame = VirtualDuration(self.profile.frame_interval_us.max(1));
            let slots = (!self.pass.is_empty()).then_some(frame);
            let bound = match self.cancel_at {
                Some(at) if self.cancellable => goal.min(at),
                _ => goal,
            };
            let mut next = next_stop(self.now, &StopBounds { goal: bound, agenda_next: self.net.agenda_next(), max_segment, frame_interval: slots });
            // Engine patch 6: the page renders at frame ticks — each one, unless the last rendered nothing and
            // nothing has happened since, when the first tick at or after the page's next task.
            if render_frames && self.patched && next > self.now {
                let tick_after = |t: VirtualTime| {
                    let k = frame_slot(t, frame);
                    let at = frontcap_core::clock::frame_tick(k, frame);
                    if at >= t && at > self.now { at } else { frontcap_core::clock::frame_tick(k + 1, frame) }
                };
                let frame_stop = match self.frames_idle {
                    Some(idle) if idle.recorded == self.recorded && idle.documents == self.documents => idle.wake.map(tick_after),
                    _ => Some(tick_after(VirtualTime(self.now.0 + 1))),
                };
                if let Some(t) = frame_stop {
                    next = next.min(t);
                }
            }
            let budget = next.since(self.now);
            if holding {
                // Virtual time flows under `advance` whatever is outstanding: a passed-through response
                // reaches the page when it arrives.
                for p in self.net.passthrough.values_mut() {
                    p.moved = true;
                }
                if self.policy(emulation::VirtualTimePolicy::Advance, Some(budget)).await? == PolicyOutcome::Held {
                    self.recorded_pause().await?;
                    started = service.elapsed();
                    continue;
                }
                loop {
                    match self.wait(started, false).await? {
                        Wait::Expired => {
                            if let Some(t) = self.expiry_of(next).await? {
                                self.now = t;
                                self.recorded_pause().await?;
                                started = service.elapsed();
                                break;
                            }
                        }
                        // A request the main thread waits for is answered at once; others are handled at
                        // the segment's end.
                        Wait::Request(ev) if self.blocks_main_thread(&ev) => {
                            self.serve_blocking(*ev).await?;
                            if self.shared_time.is_some() && self.event_hit() {
                                self.policy(emulation::VirtualTimePolicy::Pause, None).await?;
                                self.now = self.read_time().await?.max(self.now);
                                started = service.elapsed();
                                break;
                            }
                        },
                        Wait::Request(ev) => self.queued.push((Instant::now(), *ev)),
                        Wait::Answer(a) => self.answer_blocking(a).await,
                        Wait::Quiet => unreachable!("quiet is not awaited while holding"),
                        Wait::Watchdog => return self.unresponsive(goal).await,
                    }
                }
                continue;
            }
            if self.policy(emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending, Some(budget)).await? == PolicyOutcome::Held {
                self.recorded_pause().await?;
                started = service.elapsed();
                continue;
            }
            loop {
                match self.wait(started, true).await? {
                    // The page is in the middle of a task: answered without a policy change.
                    Wait::Request(ev) if self.blocks_main_thread(&ev) => {
                        self.serve_blocking(*ev).await?;
                        if self.shared_time.is_some() && self.event_hit() {
                            self.policy(emulation::VirtualTimePolicy::Pause, None).await?;
                            self.now = self.read_time().await?.max(self.now);
                            started = service.elapsed();
                            break;
                        }
                    },
                    Wait::Answer(a) => self.answer_blocking(a).await,
                    Wait::Expired => {
                        if let Some(t) = self.expiry_of(next).await? {
                            self.now = t.max(self.now);
                            self.recorded_pause().await?;
                            started = service.elapsed();
                            break;
                        }
                    }
                    Wait::Request(ev) if ev.resource_type == cnet::ResourceType::Document && ev.frame_id == self.main_frame => {
                        // Handle earlier requests at the navigation's issue instant. An armed
                        // navigation stop pauses before delivering the document; ordinary progress
                        // lets the existing grant continue unless a response deadline needs a stop.
                        // Child-frame documents go through the next arm.
                        let url = ev.request.url.split('#').next().unwrap_or_default().to_string();
                        if let Some(t) = self.navigates.iter().find(|(u, _)| *u == url).map(|(_, t)| *t) {
                            self.now = self.now.max(t);
                        }
                        let stop_at_commit = self.event_stop.as_ref().is_some_and(|s| matches!(s.event, ProgressEvent::Navigation | ProgressEvent::Dialog));
                        let (documents, within) = (self.documents, self.within_document);
                        let stop_at_interception = self.event_stop.as_ref().is_some_and(|s| s.event == ProgressEvent::Interception);
                        if stop_at_commit || stop_at_interception {
                            // Engine patch 28 keeps the old renderer serviceable while Fetch holds
                            // the new document. Pause before delivery so commit freezes its loader.
                            self.policy(emulation::VirtualTimePolicy::Pause, None).await?;
                        }
                        // Shared publication has reached the navigation's issue instant.
                        // Resolve that browser-owned instant before selecting the document:
                        // scheduling it against the previous segment start would make a
                        // renderer replacement wait for its own undelivered response.
                        if self.shared_time.is_some() {
                            self.now = self.read_time().await?.max(self.now);
                        }
                        self.queued.push((Instant::now(), *ev));
                        self.handle_queued().await?;
                        if stop_at_interception { break; }
                        if stop_at_commit {
                            // The policy now answers before commit. Do not grant the next segment
                            // while the frozen document is still arriving or use its predecessor's world.
                            self.await_commit(documents, within).await?;
                            self.drain().await?;
                        }
                        let shared_navigation = self.shared_time.is_some();
                        if self.net.agenda_next().is_some_and(|due| due < next)
                            || stop_at_commit || shared_navigation
                        {
                            self.policy(emulation::VirtualTimePolicy::Pause, None).await?;
                            self.now = self.read_time().await?.max(self.now);
                            started = service.elapsed();
                            break;
                        }
                    }
                    Wait::Request(ev) => {
                        // The pause command may service a later synchronous request. Keep this
                        // earlier request visible to its prefix collector before yielding.
                        self.queued.push((Instant::now(), (*ev).clone()));
                        self.policy(emulation::VirtualTimePolicy::Pause, None).await?;
                        self.now = self.issue_time(&ev).await?.max(self.now);
                        self.handle_queued().await?;
                        // The engine may have moved past the issue before the request reached the platform (a
                        // document load's completion step): the next budget runs from where it is, so it ends
                        // where the interrupted one would have, and that one's late expiry ends nothing.
                        self.now = self.read_time().await?.max(self.now);
                        started = service.elapsed();
                        break;
                    }
                    Wait::Quiet => {
                        if self.shared_window_boundary().await? {
                            started = service.elapsed();
                            break;
                        }
                        if let Some(t) = self.dialog_boundary().await? {
                            self.now = t.max(self.now);
                            started = service.elapsed();
                            break;
                        }
                        if let Some(t) = self.starved_fetch().await? {
                            // From here on, segments of one frame interval under `advance`, which does not
                            // wait for the held-up fetch, until the frames have released it (S15).
                            let detail = format!("a fetch after input waited for a frame at {} ms", t.0 / 1000);
                            self.record(self.now.max(self.log.last_time()), EventKind::Taint(TaintRecord { source: TaintSource::FrameStarvation, detail }));
                            self.starving = true;
                            started = service.elapsed();
                            break;
                        }
                    }
                    Wait::Watchdog => return self.unresponsive(goal).await,
                }
            }
        }
    }

    /// The instant virtual time is stuck at when a running `pauseIfNetworkFetchesPending` budget has not
    /// moved although the platform answered every request it saw and the page answers: a fetch the
    /// engine holds until a frame that a paused clock never produces (S15, engine patches 1 and 6).
    pub(super) async fn starved_fetch(&mut self) -> Result<Option<VirtualTime>, ChromiumError> {
        if self.net.answered.is_empty() || self.net.held.values().any(|h| !h.released) || !self.net.passthrough.is_empty() || !self.net.interceptions.is_empty() {
            return Ok(None);
        }
        self.drain_network().await?;
        if self.net.answered.is_empty() {
            return Ok(None);
        }
        let Ok(t) = tokio::time::timeout(Duration::from_millis(200), self.read_time()).await else { return Ok(None) };
        let t = t?;
        Ok((t <= self.now).then_some(t))
    }

    /// Whether a budget-expired event ends the budget that runs to `end`. A budget interrupted by a
    /// request stays pending in the engine and reports its own expiry later, possibly inside a later
    /// budget; such an event finds virtual time short of `end` and is not this budget's end.
    pub(super) async fn expiry_of(&mut self, end: VirtualTime) -> Result<Option<VirtualTime>, ChromiumError> {
        let t = self.read_time().await?;
        let end = if self.patched { end.0 } else { end.0 / 1000 * 1000 };
        if let Some(boundary) = self.dialog_boundary().await? { return Ok(Some(boundary)); }
        Ok((t.0 >= end).then_some(t))
    }

    /// Dialog receipts remain authoritative when navigation parks renderer event delivery.
    /// Resolve an accepted navigation before issuing renderer observations behind its barrier.
    async fn dialog_boundary(&mut self) -> Result<Option<VirtualTime>, ChromiumError> {
        let Some(stop) = self.event_stop.as_ref().filter(|s| s.event == ProgressEvent::Dialog) else { return Ok(None); };
        let after_seq = stop.after_seq;
        self.drain_bindings(Duration::ZERO).await?;
        self.drain_native_dialog_receipts().await?;
        self.commit()?;
        if !self.event_hit() { return Ok(None); }
        let t = self.read_time().await?;
        self.now = self.now.max(t);
        let events = BehaviorLog::read(&self.trace_path())?;
        let accepted = events.iter().rev().find(|e| e.seq >= after_seq && matches!(&e.kind,
            EventKind::Dialog(d) if d.kind == DialogKind::Beforeunload && d.answer == "accepted"));
        if let Some(accepted) = accepted
            && !events.iter().any(|e| e.seq > accepted.seq && matches!(e.kind, EventKind::Navigation(_)))
        {
            self.policy(emulation::VirtualTimePolicy::Pause, None).await?;
            self.await_commit(self.documents, self.within_document).await?;
        }
        Ok(Some(t))
    }

    pub(super) async fn wait(&mut self, started: Duration, requests: bool) -> Result<Wait, ChromiumError> {
        self.worker_enrollment_result()?;
        // An interception a serviced page command took while the grant began (`serviced`) stopped the
        // clock at its issue as surely as one still to arrive.
        if requests && !self.queued.is_empty() {
            return Ok(Wait::Request(Box::new(self.queued.remove(0).1)));
        }
        let service = self.page.service_clock();
        let deadline = started.saturating_add(self.watchdog);
        let quiet_at = service.elapsed().saturating_add(STALL_WAIT);
        let changed = self.subs.workers.lock().unwrap().changed.clone();
        loop {
            self.worker_enrollment_result()?;
            let Subs { expired, paused, answers, .. } = &mut self.subs;
            let next = tokio::select! {
                r = expired.next() => { r?; Some(Wait::Expired) }
                Some(a) = answers.recv() => Some(Wait::Answer(a)),
                r = paused.next() => Some(Wait::Request(Box::new(r?))),
                _ = changed.notified() => None,
                _ = service.timeout_at(quiet_at, std::future::pending::<()>()), if requests && quiet_at < deadline => Some(Wait::Quiet),
                _ = service.timeout_at(deadline, std::future::pending::<()>()) => Some(Wait::Watchdog),
            };
            if let Some(next) = next {
                self.worker_enrollment_result()?;
                return Ok(next);
            }
        }
    }

    pub(super) async fn unresponsive<T>(&mut self, goal: VirtualTime) -> Result<T, ChromiumError> {
        self.worker_enrollment_result()?;
        // The page's main thread is not answering; interrupt the script through the debugger, which is
        // enabled only for this. The generation cannot be controlled afterwards.
        let client = self.page.client.clone();
        let session = self.page.id.clone();
        let _ = tokio::time::timeout(Duration::from_secs(2), async move {
            client.call(Some(&session), &debugger::EnableParams::default()).await?;
            client.call(Some(&session), &debugger::PauseParams::default()).await
        })
        .await;
        self.dead = true;
        Err(ChromiumError::Unresponsive { goal_us: goal.0 })
    }

    pub(super) async fn sample_frozen(&mut self) -> Result<(), ChromiumError> {
        if self.watches.any_in(None) && let Some(world) = self.world.clone() {
            self.page.eval("globalThis.__frontcap.sampleFrozen()", Some(world)).await?;
            self.drain().await?;
            self.commit()?;
        }
        for popup in self.open_popups() {
            if !self.watches.any_in(Some(&popup)) { continue; }
            self.enter_context(&popup)?;
            let sampled: Result<(), ChromiumError> = async {
                if let Some(world) = self.world.clone() {
                    self.page.eval("globalThis.__frontcap.sampleFrozen()", Some(world)).await?;
                    self.drain().await?;
                }
                Ok(())
            }.await;
            self.leave_context();
            sampled?;
            self.commit()?;
        }
        Ok(())
    }

    /// Arms or clears the condition stop in the document of the page the watch samples in (its popup's, or
    /// the launch page's) and in every future one; an armed stop is cleared in the page it was armed in.
    pub(super) async fn set_stop(&mut self, name: Option<&WatchName>) -> Result<(), ChromiumError> {
        let armed_in = self.stop_context.take();
        let script = self.stop_script.take();
        if script.is_some() || armed_in.is_some() || name.is_none() {
            self.stop_page_work(armed_in.as_deref(), script, None).await?;
        }
        if let Some(n) = name {
            let context = self.watches.context(n).map(str::to_string);
            let expr = format!("globalThis.__frontcap.stopWhenTrue({})", serde_json::to_string(n.as_str()).unwrap());
            self.stop_script = self.stop_page_work(context.as_deref(), None, Some(&expr)).await?;
            self.stop_context = context;
        }
        Ok(())
    }

    /// In `context`'s page: removes `script`, then arms `arm` (registered for future documents too, its
    /// script returned) or, without it, clears the stop. A closed popup has nothing to clear.
    async fn stop_page_work(&mut self, context: Option<&str>, script: Option<page::ScriptIdentifier>, arm: Option<&str>) -> Result<Option<page::ScriptIdentifier>, ChromiumError> {
        let entered = match self.enter_for(context) {
            Ok(entered) => entered,
            Err(ChromiumError::ContextClosed(_)) if arm.is_none() => return Ok(None),
            Err(e) => return Err(e),
        };
        let done: Result<Option<page::ScriptIdentifier>, ChromiumError> = async {
            if let Some(id) = script {
                self.page.call(&page::RemoveScriptToEvaluateOnNewDocumentParams::new(id)).await?;
            }
            let expr = arm.unwrap_or("globalThis.__frontcap.clearStop()");
            let added = match arm {
                Some(expr) => Some(self.world_script(expr).await?),
                None => None,
            };
            self.drain().await?;
            if let Some(world) = self.world.clone() {
                self.page.eval(expr, Some(world)).await?;
            }
            Ok(added)
        }.await;
        if entered { self.leave_context(); }
        done
    }

    pub async fn until(&mut self, name: &WatchName, max: VirtualDuration) -> Result<ClockOutcome, ChromiumError> {
        self.alive()?;
        let wall = Instant::now();
        let goal = self.now.saturating_add(max);
        self.clock_record(ClockOp::Until, ClockPhase::Start, goal, None, None);
        self.drain().await?;
        let outcome = if self.last_sample(name) == Some(&SampleValue::True) {
            ClockOutcome::ConditionMet { watch: name.clone() }
        } else {
            self.set_stop(Some(name)).await?;
            let end = self.run_cancellable(goal, Some(name)).await;
            // Cleanup is best effort; the run's own error is the one reported.
            let cleared = self.set_stop(None).await;
            let outcome = match end? {
                RunEnd::Stopped => ClockOutcome::ConditionMet { watch: name.clone() },
                RunEnd::Reached => ClockOutcome::ConditionTimedOut { watch: name.clone() },
                RunEnd::Cancelled => ClockOutcome::Cancelled,
            };
            cleared?;
            outcome
        };
        self.clock_record(ClockOp::Until, ClockPhase::End, goal, Some(outcome.clone()), Some(wall));
        self.commit()?;
        Ok(outcome)
    }

    pub async fn settle(&mut self, idle: VirtualDuration, max: VirtualDuration) -> Result<SettleOutcome, ChromiumError> {
        let goal = self.now.saturating_add(max);
        self.settle_to(idle, goal, None, |_| Ok(())).await
    }

    /// A settle that ends by `goal` and continues a quiet window begun at `quiet_since` (a settle resumed
    /// after a fault). `on_step` sees the progress after every step, so a caller can persist it.
    pub async fn settle_to(
        &mut self,
        idle: VirtualDuration,
        goal: VirtualTime,
        quiet_since: Option<VirtualTime>,
        mut on_step: impl FnMut(SettleProgress) -> std::io::Result<()>,
    ) -> Result<SettleOutcome, ChromiumError> {
        self.alive()?;
        if idle.0 < 1000 {
            // Quiescence over an empty window is undefined, and virtual time is read in whole
            // milliseconds, so a shorter window would never move time.
            return Err(ChromiumError::InvalidArgument("the idle window must be at least 1 ms".into()));
        }
        let wall = Instant::now();
        self.clock_record(ClockOp::Settle, ClockPhase::Start, goal, None, None);
        let origin = quiet_since.unwrap_or(self.now).min(self.now);
        let mut q = Quiescence::new(idle, origin);
        for ordinal in self.net.held.keys().chain(self.net.passthrough.values().map(|p| &p.ordinal)) {
            q.begin(&format!("request:{ordinal}"), origin);
        }
        self.quiet = Some(q);
        let result = loop {
            self.drain().await?;
            let animations = match self.world.clone() {
                Some(w) => self.page.eval("globalThis.__frontcap.animationsRunning()", Some(w)).await?.as_u64().unwrap_or(0),
                None => 0,
            };
            let q = self.quiet.as_mut().expect("settling");
            if animations > 0 {
                q.activity(self.now);
            }
            if animations == 0 && q.is_quiescent(self.now) {
                let next_timer_t = match self.world.clone() {
                    Some(w) => self.page.eval("globalThis.__frontcapNextTimer?.() ?? null", Some(w)).await?.as_f64(),
                    None => None,
                };
                break SettleOutcome::Quiescent { t: self.now, next_timer_t: next_timer_t.map(|us| self.now.saturating_add(VirtualDuration::from_micros(us as u64))) };
            }
            if self.now >= goal {
                let mut still_active = q.still_active();
                if animations > 0 {
                    still_active.push("animation".into());
                }
                break SettleOutcome::BudgetExhausted { t: self.now, still_active };
            }
            let step = self.now.saturating_add(idle).min(goal);
            match self.run_cancellable(step, None).await {
                Ok(RunEnd::Cancelled) => break SettleOutcome::Cancelled { t: self.now },
                Ok(_) => {}
                Err(e) => {
                    self.quiet = None;
                    return Err(e);
                }
            }
            let quiet_since = self.quiet.as_ref().expect("settling").last_activity();
            on_step(SettleProgress { goal, quiet_since })?;
        };
        self.quiet = None;
        let outcome = match &result {
            SettleOutcome::Quiescent { .. } => ClockOutcome::Quiescent,
            SettleOutcome::BudgetExhausted { still_active, .. } => ClockOutcome::BudgetExhausted { still_active: still_active.clone() },
            SettleOutcome::Cancelled { .. } => ClockOutcome::Cancelled,
            SettleOutcome::NotRequested => ClockOutcome::Reached,
        };
        self.clock_record(ClockOp::Settle, ClockPhase::End, goal, Some(outcome), Some(wall));
        self.commit()?;
        Ok(result)
    }
}
