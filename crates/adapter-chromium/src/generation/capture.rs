use super::*;


/// One native command; dropping the owner also drops its local CDP waiter.
pub(super) struct PendingFrame {
    session: String,
    document: u64,
    suspended_opportunity: Option<String>,
    run: Option<tokio::task::JoinHandle<Result<emulation::RunMainFrameReturns, cdp::CdpError>>>,
}

impl Drop for PendingFrame {
    fn drop(&mut self) { if let Some(run) = &self.run { run.abort(); } }
}

/// Windows already rendered cannot be dispatched again when a stopped batch resumes.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct PendingRender {
    context: Option<String>,
    retained: bool,
    recorded: u64,
    root: Option<emulation::RunMainFrameReturns>,
    popups: Vec<String>,
    popup_index: usize,
    rendered: bool,
}

impl Generation {
    /// Keeps the frames the screencast delivered, acknowledging each; `wait` lets a frame still on its
    /// way arrive.
    pub(super) async fn take_frames(&mut self, wait: Duration) -> Result<(), ChromiumError> {
        while let Some(f) = self.subs.frames.next_timeout(wait).await? {
            self.page.call(&page::ScreencastFrameAckParams::new(f.session_id)).await?;
            self.frames_seen += 1;
            // The compositor's timestamp is when the frame was shown, often after virtual time moved on;
            // arrival order is what ties a frame to a state (`frames_quiet`).
            let arrived = crate::frames::wall_now();
            let frame = RawFrame { number: self.frames_seen, data: f.data.0, arrived };
            if let Some((pending, _)) = self.recording.as_mut() {
                pending.push(RawFrame { number: frame.number, data: frame.data.clone(), arrived });
            }
            self.frame = Some(frame);
        }
        self.store_recorded(false)
    }

    /// While recording, each pause is rendered before virtual time moves on, so every frame is tied to
    /// the state it shows.
    pub(super) async fn recorded_pause(&mut self) -> Result<(), ChromiumError> {
        // On the patched engine a recording keeps renders as they are presented (`render_frame`).
        if self.recording.is_some() && !self.patched {
            self.timeline.stopped(self.now);
            self.frames_quiet().await?;
        }
        Ok(())
    }

    /// Takes frames until none has come for `FRAME_QUIET` (at most `FRAME_QUIET_MAX`): every state
    /// committed so far has then been rendered, if it renders at all.
    pub(super) async fn frames_quiet(&mut self) -> Result<(), ChromiumError> {
        match tokio::time::timeout(FRAME_QUIET_MAX, self.take_frames(FRAME_QUIET)).await {
            Ok(r) => r,
            Err(_) => self.store_recorded(false),
        }
    }

    /// Writes recorded frames whose state is settled (a newer mark exists; or every one, `all`) to
    /// `frames/` and its index `frames/frames.jsonl`.
    pub(super) fn store_recorded(&mut self, all: bool) -> Result<(), ChromiumError> {
        let Some((pending, _)) = self.recording.as_mut() else { return Ok(()) };
        let newest = self.timeline.newest_wall();
        let ready: Vec<RawFrame> = std::mem::take(pending);
        let (ready, later): (Vec<_>, Vec<_>) = ready.into_iter().partition(|f| all || f.arrived < newest);
        if let Some((pending, _)) = self.recording.as_mut() {
            *pending = later;
        }
        for f in ready {
            let Some((mark, _)) = self.timeline.attribute(f.arrived) else { continue };
            let Some(t_us) = mark.t_us else { continue };
            let document = mark.document;
            let png = frontcap_core::encode::unbase64(&f.data).ok_or_else(|| ChromiumError::Shape("a screencast frame that is not base64".into()))?;
            let file = format!("frames/{}-{}.png", t_us.0, (f.arrived * 1e6) as u64);
            std::fs::create_dir_all(self.bundle.frames_dir())?;
            std::fs::write(self.bundle.root().join(&file), &png)?;
            let entry = frontcap_core::observation::RecordedFrame { frame: f.number, t_us, document, file };
            let line = serde_json::to_string(&entry).map_err(|e| ChromiumError::Shape(e.to_string()))? + "\n";
            use std::io::Write as _;
            std::fs::OpenOptions::new().create(true).append(true).open(self.bundle.frames_dir().join("frames.jsonl"))?.write_all(line.as_bytes())?;
            if let Some((_, written)) = self.recording.as_mut() {
                written.push(entry);
            }
        }
        Ok(())
    }

    /// The last rendered frame, stored in the bundle, with the state it shows — the state of the mark it
    /// arrived after, once the screencast has fallen quiet — and changing nothing (S16). `no_frame` before the first
    /// frame; `stale_frame` when it shows an earlier document.
    pub async fn screenshot(&mut self) -> Result<frontcap_core::observation::Captured, ChromiumError> {
        self.alive()?;
        if self.patched {
            return self.screenshot_rendered().await;
        }
        self.timeline.stopped(self.now);
        // The renderer draws committed states in order: once no frame comes for a while, the last one
        // shows the latest state that was rendered.
        self.frames_quiet().await?;
        // The latest state has had no frame yet (a slow first paint), or the last frame may show an
        // earlier state: wait for another. A page that changed nothing visible renders none, and the
        // older frame is answered as such.
        let settled = |g: &mut Self| match g.frame.as_ref().map(|f| f.arrived) {
            Some(arrived) => g.timeline.attribute(arrived).is_some_and(|(m, exact)| exact && g.timeline.is_latest(&m)),
            None => false,
        };
        if !settled(self) {
            let seen = self.frames_seen;
            let deadline = tokio::time::Instant::now() + FRAME_BEHIND_WAIT;
            while self.frames_seen == seen && tokio::time::Instant::now() < deadline {
                self.take_frames(Duration::from_millis(10)).await?;
            }
            if self.frames_seen != seen {
                self.frames_quiet().await?;
            }
        }
        let frame = self.frame.as_ref().ok_or(ChromiumError::NoFrame)?;
        let arrived = frame.arrived;
        let (mark, exact) = match self.timeline.unchanged_since(arrived) {
            Some(latest) => (latest, true),
            None => self.timeline.attribute(arrived).ok_or(ChromiumError::NoFrame)?,
        };
        let t_us = mark.t_us.unwrap_or(self.now);
        if mark.document != self.documents {
            return Err(ChromiumError::StaleFrame);
        }
        let current = exact && self.timeline.is_latest(&mark) && t_us == self.now;
        self.keep_screenshot(t_us, mark.document, exact, current)
    }

    /// The existing native surface, bracketed by activation receipts. A recovered
    /// controller can read it without rendering or relabelling old pixels as current.
    pub(super) async fn screenshot_rendered(&mut self) -> Result<frontcap_core::observation::Captured, ChromiumError> {
        let copy = self.copy_presented().await?;
        self.surface_copies += 1;
        let mut captured = self.keep_png(copy.png, self.surface_copies, copy.t_us, self.documents, copy.exact, copy.current)?;
        captured.painted_at_us = Some(copy.painted_at);
        Ok(captured)
    }

    /// Engine patch 6: a PNG of the page's surface as the display shows it, without a frame of its own.
    pub(super) async fn copy_surface(&mut self) -> Result<Vec<u8>, ChromiumError> {
        let shot = self
            .page
            .call(&page::CaptureScreenshotParams { format: Some(page::CaptureScreenshotParamsFormat::Png), from_surface: Some(true), ..Default::default() })
            .await?;
        frontcap_core::encode::unbase64(&shot.data.0).ok_or_else(|| ChromiumError::Shape("a screenshot that is not base64".into()))
    }

    /// Engine patch 6: keeps a recorded frame at the instant whose state it shows.
    pub(super) fn keep_recorded(&mut self, png: Vec<u8>, t_us: VirtualTime, document: u64) -> Result<(), ChromiumError> {
        self.surface_copies += 1;
        let file = format!("frames/{}-{}.png", t_us.0, self.surface_copies);
        std::fs::create_dir_all(self.bundle.frames_dir())?;
        presentation::write_surface(&self.bundle.root().join(&file), &png)?;
        let entry = frontcap_core::observation::RecordedFrame { frame: self.surface_copies, t_us, document, file };
        let line = serde_json::to_string(&entry).map_err(|e| ChromiumError::Shape(e.to_string()))? + "\n";
        use std::io::Write as _;
        std::fs::OpenOptions::new().create(true).append(true).open(self.bundle.frames_dir().join("frames.jsonl"))?.write_all(line.as_bytes())?;
        if let Some((_, written)) = self.recording.as_mut() {
            written.push(entry);
        }
        Ok(())
    }

    pub(super) fn keep_screenshot(&mut self, t_us: VirtualTime, document: u64, exact: bool, current: bool) -> Result<frontcap_core::observation::Captured, ChromiumError> {
        let frame = self.frame.as_ref().ok_or(ChromiumError::NoFrame)?;
        let png = frontcap_core::encode::unbase64(&frame.data).ok_or_else(|| ChromiumError::Shape("a screencast frame that is not base64".into()))?;
        let number = frame.number;
        self.keep_png(png, number, t_us, document, exact, current)
    }

    pub(super) fn keep_png(&mut self, png: Vec<u8>, number: u64, t_us: VirtualTime, document: u64, exact: bool, current: bool) -> Result<frontcap_core::observation::Captured, ChromiumError> {
        let dir = self.bundle.screenshots_dir();
        std::fs::create_dir_all(&dir)?;
        let name = format!("{}-{number}.png", t_us.0);
        let path = dir.join(&name);
        if self.patched { presentation::write_surface(&path, &png)?; } else { std::fs::write(&path, &png)?; }
        let captured = frontcap_core::observation::Captured { path, frame: number, t_us, document, exact, current, painted_at_us: None };
        let record = frontcap_core::event::ScreenshotRecord {
            path: format!("screenshots/{name}"),
            sha256: frontcap_core::canonical::sha256_bytes_hex(&png),
            source_frame: number,
            frame_t_us: t_us,
            document,
        };
        let after = self.log.next_seq().saturating_sub(1);
        self.acquisition.append(after, self.now, AcquisitionKind::Screenshot(record))?;
        Ok(captured)
    }

    /// Keeps every frame from now on, under `frames/` with the index `frames/frames.jsonl`. The recording
    /// begins with the last frame once the screencast has fallen quiet, when it shows the state at the
    /// start for certain.
    pub async fn record_start(&mut self) -> Result<(), ChromiumError> {
        self.alive()?;
        if self.recording.is_some() {
            return Ok(());
        }
        if self.patched {
            self.recording_missed.clear();
            let start = match self.copy_presented().await {
                Ok(copy) if copy.exact => Some(copy),
                Ok(_) | Err(ChromiumError::NoFrame | ChromiumError::StaleFrame) => None,
                Err(e) => return Err(e),
            };
            self.recording = Some((Vec::new(), Vec::new()));
            if let Some(copy) = start {
                self.keep_recorded(copy.png, copy.t_us, self.documents)?;
            }
            return Ok(());
        }
        self.timeline.stopped(self.now);
        self.frames_quiet().await?;
        let start = self.frame.clone().filter(|f| self.timeline.attribute(f.arrived).is_some_and(|(m, exact)| exact && self.timeline.is_latest(&m)));
        self.recording = Some((start.into_iter().collect(), Vec::new()));
        Ok(())
    }

    /// Stops keeping frames and returns those kept since `record_start`.
    pub async fn record_stop(&mut self) -> Result<frontcap_core::observation::Recording, ChromiumError> {
        self.alive()?;
        if self.patched {
            // Each presented render was kept as it was drawn (`render_frame`).
            let missed = std::mem::take(&mut self.recording_missed);
            return Ok(frontcap_core::observation::Recording { frames: self.recording.take().map(|(_, w)| w).unwrap_or_default(), missed });
        }
        self.timeline.stopped(self.now);
        self.frames_quiet().await?;
        self.store_recorded(true)?;
        Ok(frontcap_core::observation::Recording { frames: self.recording.take().map(|(_, w)| w).unwrap_or_default(), missed: Vec::new() })
    }

    /// The re-execution inputs the run lacks, as its host recorded them; without this the run is not
    /// replayable (no command log).
    pub fn set_missing_inputs(&mut self, missing: Vec<String>) {
        self.missing_inputs = Some(missing);
    }

    pub fn is_recording(&self) -> bool {
        self.recording.is_some()
    }

    /// Renders one frame at the current instant (engine patch 6: the engine holds main frames back and lets
    /// exactly one run when the page asked for one, answered once it has run). Requests the frame makes
    /// (a synchronous one blocks it) are handled meanwhile.
    pub(super) async fn render_frame(&mut self, recorded: u64, started: Duration) -> Result<(), ChromiumError> {
        let previous = self.popup_id();
        if let Some(batch) = &self.pending_render { self.reenter(batch.context.clone())?; }
        let rendered = self.render_batch(recorded, started).await;
        let restored = self.reenter(previous);
        restored?;
        rendered
    }

    async fn render_batch(&mut self, recorded: u64, started: Duration) -> Result<(), ChromiumError> {
        if !self.patched { return Ok(()); }
        if self.world.is_none() {
            if self.pending_render.is_some() {
                self.pending_frame.take();
                self.pending_render.take();
                self.dead = true;
                return Err(ChromiumError::Unsupported("an unfinished frame lost its document owner".into()));
            }
            return Ok(());
        }
        let mut batch = self.pending_render.take().unwrap_or(PendingRender {
            context: self.popup_id(), retained: false,
            recorded, root: None, popups: Vec::new(), popup_index: 0, rendered: false,
        });
        if batch.root.is_none() {
            let Some(answer) = self.render_context(started).await? else {
                self.retain_render_batch(batch).await?;
                return Ok(());
            };
            batch.rendered = answer.rendered;
            batch.root = Some(answer);
            batch.popups = self.open_popups();
        }
        while batch.popup_index < batch.popups.len() {
            if self.frame_stop.is_some() {
                self.retain_render_batch(batch).await?;
                return Ok(());
            }
            let id = &batch.popups[batch.popup_index];
            self.enter_context(id)?;
            let slot = frame_slot(self.now, VirtualDuration(self.profile.frame_interval_us.max(1)));
            let sampled = match self.world.clone() {
                Some(world) if self.pending_frame.is_none() && self.watches.any_in(Some(id)) => {
                    let page = self.page.clone();
                    match self.serviced(page.eval(&format!("globalThis.__frontcap.sampleFrame({slot})"), Some(world))).await {
                        Ok(_) => self.drain().await,
                        Err(e) => Err(e),
                    }
                }
                _ => Ok(()),
            };
            let popup = match sampled {
                Ok(()) if self.world.is_some() => self.render_context(started).await,
                Ok(()) => Ok(None),
                Err(e) => Err(e),
            };
            self.leave_context();
            batch.rendered |= popup?.is_some_and(|a| a.rendered);
            if self.pending_frame.is_some() {
                self.retain_render_batch(batch).await?;
                return Ok(());
            }
            batch.popup_index += 1;
        }
        if batch.retained {
            frontcap_core::state::save_atomic(&self.bundle.root().join(STOPPED_RENDER), &Option::<StoppedRender>::None)?;
        }
        self.frames_idle = (!batch.rendered && batch.popups.is_empty()).then(|| FramesIdle {
            recorded: batch.recorded,
            documents: self.documents,
            wake: batch.root.as_ref().and_then(|answer| answer.next_wake_up)
                .map(|ms| self.now.saturating_add(VirtualDuration((ms * 1000.0).round() as u64))),
        });
        Ok(())
    }

    /// Native frame requests made since this adapter instance was created.
    #[cfg(feature = "test-hooks")]
    pub fn frame_requests(&self) -> u64 {
        self.frame_requests
    }

    /// Renders the active context's frame (`render_frame`); what the engine answered.
    async fn render_context(&mut self, mut started: Duration) -> Result<Option<emulation::RunMainFrameReturns>, ChromiumError> {
        let mut command = if let Some(mut command) = self.pending_frame.take() {
            if command.session != self.page.id || command.document != self.documents {
                self.dead = true;
                return Err(ChromiumError::Unsupported("an unfinished frame no longer belongs to this document".into()));
            }
            if let Some(opportunity) = command.suspended_opportunity.take() {
                if command.run.is_some() {
                    if let Err(error) = self.page.call(&emulation::FrontcapSetWorkerFramesPausedParams::new(opportunity, false)).await {
                        self.dead = true;
                        return Err(error);
                    }
                } else {
                    static NEXT_RECEIVER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let sequence = NEXT_RECEIVER.try_update(std::sync::atomic::Ordering::Relaxed,
                        std::sync::atomic::Ordering::Relaxed, |n| n.checked_add(1))
                        .map_err(|_| ChromiumError::Shape("worker frame receiver identities exhausted".into()))?;
                    let receiver = format!("{}-{sequence}", self.page.id);
                    let native = self.page.call(&emulation::FrontcapGetWorkersParams::default()).await?;
                    let workers = native.workers.into_iter()
                        .filter(|worker| worker.frame_opportunity.as_deref() == Some(opportunity.as_str()))
                        .map(|worker| worker.target_id).collect::<Vec<_>>();
                    if workers.is_empty() {
                        return Err(ChromiumError::Unsupported("the stopped worker frame has retired".into()));
                    }
                    let client = self.page.client.clone();
                    let session = self.page.id.clone();
                    let claimed_receiver = receiver.clone();
                    command.run = Some(tokio::spawn(async move {
                        client.call(Some(&session), &emulation::RunMainFrameParams {
                            resume_worker_opportunity: Some(opportunity),
                            resume_worker_receiver: Some(claimed_receiver),
                        }).await
                    }));
                    self.wait_for_frame_receiver(&command, &workers, &receiver).await?;
                }
            }
            command
        } else {
            let client = self.page.client.clone();
            let session = self.page.id.clone();
            #[cfg(feature = "test-hooks")]
            { self.frame_requests += 1; }
            PendingFrame {
                session: session.clone(), document: self.documents, suspended_opportunity: None,
                run: Some(tokio::spawn(async move { client.call(Some(&session), &emulation::RunMainFrameParams::default()).await })),
            }
        };
        let service = self.page.service_clock();
        let answer = loop {
            // A refused or already-completed command is resolved before any
            // future synchronous response can advance this frame's clock.
            if let Some(run) = command.run.as_mut()
                && run.is_finished() {
                break run.await.map_err(|e| ChromiumError::Shape(e.to_string()))??;
            }
            // Interceptions a serviced command took meanwhile are this frame's to handle too.
            if !self.queued.is_empty() {
                self.handle_queued().await?;
                started = service.elapsed();
            }
            if self.event_stop.is_some() { self.commit()?; }
            if self.event_hit() && self.frame_stop.is_none() {
                self.frame_stop = Some(RunEnd::Stopped);
            }
            if self.frame_stop.is_some() {
                // A response may have completed this command already. Finish its receipt before
                // publishing a stop; a still-blocked worker keeps the original command for resume.
                let opportunity = match self.worker_frame_waiting_opportunity().await {
                    Ok(opportunity) => opportunity,
                    Err(error) => { self.dead = true; return Err(error); }
                };
                if let Some(opportunity) = opportunity {
                    // Retaining a CDP waiter does not stop its native callbacks. Close the
                    // matching owned ticket before publishing this progress boundary.
                    if let Err(error) = self.page.call(&emulation::FrontcapSetWorkerFramesPausedParams::new(opportunity.clone(), true)).await {
                        self.dead = true;
                        return Err(error);
                    }
                    command.suspended_opportunity = Some(opportunity);
                    self.pending_frame = Some(command);
                    return Ok(None);
                }
            } else if let Some(due) = self.worker_frame_response_deadline().await? {
                let addressed = self.popup_id();
                self.leave_context();
                let serviced = Box::pin(self.progress_before_frame_completion(due)).await;
                let restored = self.reenter(addressed);
                restored?;
                match serviced? {
                    RunEnd::Reached => {},
                    end => self.frame_stop = Some(end),
                }
                started = service.elapsed();
                continue;
            }
            let deadline = started.saturating_add(self.watchdog);
            let run = command.run.as_mut().ok_or_else(|| ChromiumError::Shape("an unfinished frame has no native receiver".into()))?;
            tokio::select! {
                done = run => break done.map_err(|e| ChromiumError::Shape(e.to_string()))??,
                Some(a) = self.subs.answers.recv() => self.answer_blocking(a).await,
                ev = self.subs.paused.next() => {
                    let ev = ev?;
                    self.handle_request(ev).await?;
                    // Handling may wait on the wall clock (a stream's handshake): that is not the engine's.
                    started = service.elapsed();
                }
                _ = service.timeout_at(deadline, std::future::pending::<()>()) => {
                    return self.unresponsive(self.now).await;
                }
            }
        };
        self.after_arrival().await?;
        if answer.workers_cut_off == Some(true) {
            let detail = "a dedicated worker kept working at this frame's instant; the frame ran without what it posts".to_string();
            self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::WorkersCutOff, detail }));
        }
        if answer.presented {
            // The display shows this render now (the engine answered once it was presented).
            let frame = self.presented_frame_after(answer.presented_frame_token.clone()).await?;
            let context = self.popup_id().unwrap_or_else(|| super::PAGE.into());
            if let Some(stop) = self.event_stop.as_mut()
                && stop.event == ProgressEvent::Presentation
            {
                stop.triggers.push(ProgressTrigger::Presentation {
                    t_us: self.now, painted_at_us: frame.painted_at, document: self.documents, context,
                });
            }
            self.presented = Some(presentation::Presentation { frame, changes: self.changes });
            // A recording keeps every presented render, at its instant; one it cannot copy is listed as
            // missed, and the run goes on.
            if self.recording.is_some() {
                match self.copy_presented().await {
                    Ok(copy) if copy.exact => self.keep_recorded(copy.png, copy.t_us, self.documents)?,
                    _ => self.recording_missed.push(self.now),
                }
            }
        } else if answer.rendered
            && let Some(p) = self.presented.as_mut()
        {
            // The render ran after every change so far and drew nothing new: what the display shows is
            // the page as it is.
            p.changes = self.changes;
        }
        Ok(Some(answer))
    }

    /// Worker subscriptions stay with their launch window when page state is swapped.
    fn active_frame_workers(&self) -> Result<Workers, ChromiumError> {
        let Some(context) = self.popup_id() else { return Ok(self.subs.workers.clone()); };
        self.contexts.lock().unwrap().popups.iter()
            .find(|popup| popup.id == context && !popup.closed)
            .map(|popup| popup.workers.clone())
            .ok_or(ChromiumError::ContextClosed(context))
    }

    /// A blocked recovered worker cannot initialize a replacement inspector or
    /// replay the old wait event. Its durable request owner and native ticket
    /// identify the wait without entering the worker or inventing a new request.
    fn worker_frame_request_matches(
        &self, network: &str, worker: &emulation::FrontcapWorker, require_ready: bool,
    ) -> Result<bool, ChromiumError> {
        let Some(wait) = worker.synchronous_wait_id.as_ref() else { return Ok(false) };
        if !worker.thread_started || network.rsplit('.').next() != Some(wait.as_str())
            || !worker.frame_opportunity.as_deref().and_then(|ticket| ticket.parse::<u64>().ok())
                .is_some_and(|ticket| ticket > 0) {
            return Ok(false);
        }
        let current = {
            let book = self.active_frame_workers()?;
            let workers = book.lock().unwrap();
            workers.requests.get(network).and_then(|session| workers.sessions.get(session))
                .is_some_and(|state| state.target == worker.target_id
                    && (!require_ready || state.enrollment == WorkerEnrollment::Ready)
                    && state.synchronous_wait.as_ref().is_some_and(|(id, _)| id == wait))
        };
        if current { return Ok(true); }
        if !self.net.recovered_network_requests.contains(network) { return Ok(false); }
        let Some(lineage) = self.target_lineage(&worker.target_id)? else { return Ok(false) };
        Ok(self.net.worker_request_owners.get(network) == Some(&lineage))
    }

    /// The native inventory is scoped to this window's owned worker subtree.
    async fn worker_frame_waiting_opportunity(&self) -> Result<Option<String>, ChromiumError> {
        let unanswered = self.net.held.values()
            .filter(|held| held.attempted.is_none())
            .filter_map(|held| held.network_id.as_deref()).collect::<Vec<_>>();
        if unanswered.is_empty() { return Ok(None); }
        let native = self.page.call(&emulation::FrontcapGetWorkersParams::default()).await?;
        let mut opportunity = None;
        for worker in &native.workers {
            for network in &unanswered {
                if self.worker_frame_request_matches(network, worker, false)? {
                    opportunity = worker.frame_opportunity.clone();
                    break;
                }
            }
            if opportunity.is_some() { break; }
        }
        let Some(opportunity) = opportunity else { return Ok(None) };
        if native.workers.iter().filter_map(|worker| worker.frame_opportunity.as_deref())
            .any(|ticket| ticket != opportunity) {
            return Err(ChromiumError::Shape("owned workers report multiple unfinished frame opportunities".into()));
        }
        Ok(Some(opportunity))
    }

    /// Only a native synchronous wait inside an active frame ticket can
    /// require a future mocked answer before the in-flight frame can complete.
    /// Application values and waits outside an approved frame grant no time.
    async fn worker_frame_response_deadline(&self) -> Result<Option<VirtualTime>, ChromiumError> {
        let candidates = self.net.agenda.iter().filter_map(|(due, event)| {
            let Due::Response(delivery) = event else { return None };
            let held = self.net.held.get(&delivery.ordinal)?;
            if due <= self.now || !held.released || held.attempted.is_some() { return None; }
            Some((due, held.network_id.as_deref()?))
        }).collect::<Vec<_>>();
        if candidates.is_empty() { return Ok(None); }
        let native = self.page.call(&emulation::FrontcapGetWorkersParams::default()).await?;
        let mut deadline: Option<VirtualTime> = None;
        for (due, network) in candidates {
            for worker in &native.workers {
                if self.worker_frame_request_matches(network, worker, true)? {
                    deadline = Some(deadline.map_or(due, |before| before.min(due)));
                    break;
                }
            }
        }
        Ok(deadline)
    }

    /// At a frame tick not rendered yet, renders it — exactly at the tick only. A stop between ticks renders
    /// nothing, nor does one past a tick the clock skipped in one step (a document load's completion step):
    /// their changes render at the next tick.
    pub(super) async fn render_at_tick(&mut self, recorded: u64, started: Duration) -> Result<(), ChromiumError> {
        if !self.patched {
            return Ok(());
        }
        let frame = VirtualDuration(self.profile.frame_interval_us);
        let slot = frame_slot(self.now, frame);
        if slot > self.last_frame && frontcap_core::clock::frame_tick(slot, frame) == self.now {
            self.last_frame = slot;
            // Watches sample at the ticks the page renders at; a tick skipped as idle changes nothing. The
            // samples are the controller's, not page activity: an idle tick stays idle.
            let mut recorded = recorded;
            if self.watches.any_in(None) && let Some(world) = self.world.clone() {
                let before = self.recorded;
                let page = self.page.clone();
                self.serviced(page.eval(&format!("globalThis.__frontcap.sampleFrame({slot})"), Some(world))).await?;
                self.drain().await?;
                recorded += self.recorded - before;
            }
            self.render_frame(recorded, started).await?;
        }
        Ok(())
    }
}

const STOPPED_RENDER: &str = "stopped-render.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct RenderOwner {
    context: Option<String>,
    target: String,
    document: u64,
    documents: Vec<(String, String)>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoppedFrame {
    owner: usize,
    opportunity: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct StoppedRender {
    at_us: VirtualTime,
    clock: Option<String>,
    owners: Vec<RenderOwner>,
    frame: Option<StoppedFrame>,
    batch: PendingRender,
}

impl Generation {
    async fn wait_for_frame_receiver(&self, command: &PendingFrame, workers: &[String], receiver: &str) -> Result<(), ChromiumError> {
        let waiting = async {
            loop {
                if command.run.as_ref().is_some_and(|run| run.is_finished()) { return Ok(()); }
                let native = self.page.call(&emulation::FrontcapGetWorkersParams::default()).await?;
                let mut accepted = true;
                for target in workers {
                    let Some(worker) = native.workers.iter().find(|worker| &worker.target_id == target) else {
                        return Err(ChromiumError::Unsupported("a worker retired during frame adoption".into()));
                    };
                    accepted &= worker.frame_receiver.as_deref() == Some(receiver);
                }
                if accepted { return Ok(()); }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        self.page.service_clock().timeout(self.watchdog, waiting).await
            .map_err(|_| ChromiumError::CallTimeout("worker frame receiver adoption"))?
    }

    async fn render_document_scope(&self) -> Result<Vec<(String, String)>, ChromiumError> {
        let tree = self.page.call(&page::GetFrameTreeParams::default()).await?;
        let mut remaining = vec![tree.frame_tree];
        let mut documents = Vec::new();
        while let Some(node) = remaining.pop() {
            let document = node.frame.frontcap_document_id.ok_or_else(||
                ChromiumError::Shape("a retained render requires native document identity".into()))?;
            documents.push((node.frame.id.0, document));
            remaining.extend(node.child_frames.into_iter().flatten());
        }
        documents.sort();
        Ok(documents)
    }

    /// Publish the stopped batch only after native callback permission is closed.
    /// Completed windows and the unfinished native ticket share one atomic record.
    async fn retain_render_batch(&mut self, mut batch: PendingRender) -> Result<(), ChromiumError> {
        let previous = self.popup_id();
        let mut contexts = vec![batch.context.clone()];
        contexts.extend(batch.popups.iter().cloned().map(Some));
        let mut owners = Vec::new();
        let mut frame = None;
        let captured: Result<(), ChromiumError> = async {
            for context in contexts {
                self.reenter(context.clone())?;
                if let Some(pending) = &self.pending_frame
                    && pending.session == self.page.id {
                    let opportunity = pending.suspended_opportunity.clone().ok_or_else(||
                        ChromiumError::Shape("an unfinished frame was not suspended at its stop".into()))?;
                    if frame.is_some() {
                        return Err(ChromiumError::Shape("a frame has multiple stopped owners".into()));
                    }
                    frame = Some(StoppedFrame { owner: owners.len(), opportunity });
                }
                owners.push(RenderOwner {
                    context, target: self.target_id.clone(), document: self.documents,
                    documents: self.render_document_scope().await?,
                });
            }
            if self.pending_frame.is_some() && frame.is_none() {
                return Err(ChromiumError::Shape("the unfinished frame is outside its render batch".into()));
            }
            Ok(())
        }.await;
        let restored = self.reenter(previous);
        restored?;
        captured?;
        batch.retained = true;
        let stopped = StoppedRender {
            at_us: self.now,
            clock: self.shared_time.as_ref().map(|clock| clock.clock_id().to_owned()),
            owners, frame, batch,
        };
        frontcap_core::state::save_atomic(&self.bundle.root().join(STOPPED_RENDER), &Some(&stopped))?;
        self.pending_render = Some(stopped.batch);
        Ok(())
    }

    /// Reconstruction only reads native identities. Adopting and resuming its
    /// callback cursor belongs to the next explicit progress operation.
    pub(super) async fn recover_stopped_render(&mut self) -> Result<(), ChromiumError> {
        let stopped = frontcap_core::state::load::<Option<StoppedRender>>(
            &self.bundle.root().join(STOPPED_RENDER))?.flatten();
        let Some(stopped) = stopped else { return Ok(()) };
        if stopped.at_us != self.now
            || stopped.clock.as_deref() != self.shared_time.as_ref().map(|clock| clock.clock_id()) {
            return Err(ChromiumError::Unsupported("a stopped render no longer matches its native clock boundary".into()));
        }
        let previous = self.popup_id();
        let mut pending_frame = None;
        let recovered: Result<(), ChromiumError> = async {
            for (index, owner) in stopped.owners.iter().enumerate() {
                self.reenter(owner.context.clone())?;
                if self.target_id != owner.target || self.documents != owner.document
                    || self.render_document_scope().await? != owner.documents {
                    return Err(ChromiumError::Unsupported("a stopped render lost its original document scope".into()));
                }
                if let Some(frame) = &stopped.frame
                    && frame.owner == index {
                    let native = self.page.call(&emulation::FrontcapGetWorkersParams::default()).await?;
                    let opportunities = native.workers.iter().filter_map(|w| w.frame_opportunity.as_deref())
                        .collect::<Vec<_>>();
                    if opportunities.is_empty() || opportunities.iter().any(|ticket| *ticket != frame.opportunity) {
                        return Err(ChromiumError::Unsupported("a stopped worker frame has no matching native opportunity".into()));
                    }
                    pending_frame = Some(PendingFrame {
                        session: self.page.id.clone(), document: self.documents,
                        suspended_opportunity: Some(frame.opportunity.clone()), run: None,
                    });
                }
            }
            if stopped.frame.is_some() && pending_frame.is_none() {
                return Err(ChromiumError::Shape("a stopped frame names no retained owner".into()));
            }
            Ok(())
        }.await;
        let restored = self.reenter(previous);
        restored?;
        recovered?;
        self.pending_frame = pending_frame;
        self.pending_render = Some(stopped.batch);
        Ok(())
    }
}
