//! Finish current producer work, announcements and ordinary Fetch collection.
use super::*;

enum InterceptionCollection { Ready, MainThreadWait, PreflightAnswered }

/// The requests one collection found: every one announced through `through`. They are allocated in
/// the declared issuer order, and their answers are delivered only once all are allocated, so a
/// request an answer causes, even at the same instant, belongs to a later round.
pub(super) struct RequestRound {
    pub(super) through: cdp::Arrival,
    pub(super) selected: Vec<(fetch::RequestPausedEvent, crate::bindings::Binding)>,
}

impl Generation {
    pub(super) async fn collect_ordinary_requests(&mut self) -> Result<(), ChromiumError> {
        if self.collecting_requests || self.blocking { return Ok(()) }
        self.collecting_requests = true;
        let result = Box::pin(self.collect_ordinary_requests_inner()).await;
        self.collecting_requests = false;
        result
    }

    /// A serviced query may outlive a popup. Recollect the producer boundary;
    /// a detached reply is never evidence that its renderer stopped voting.
    async fn collection_reply<T>(&mut self, result: Result<T, ChromiumError>) -> Result<Option<T>, ChromiumError> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(ChromiumError::Cdp(cdp::CdpError::SessionDetached)) => {
                if self.shared_time.is_some() {
                    self.retire_shared_pages().await?;
                }
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    async fn collect_ordinary_requests_inner(&mut self) -> Result<(), ChromiumError> {
        let deadline = Instant::now() + Session::CALL_TIMEOUT;
        'collect: loop {
            self.worker_enrollment_result()?;
            if Instant::now() >= deadline { return Err(ChromiumError::CallTimeout("worker request notification frontier")); }
            let shared_frontier = self.shared_request_frontier().await?;
            let mut pages = vec![self.page.id.clone()];
            pages.extend(self.contexts.lock().unwrap().popups.iter()
                .filter(|p| !p.closed && !p.held).map(|p| p.session_id.clone()));
            pages.sort();
            pages.dedup();
            if let Some(frontier) = &shared_frontier { pages.retain(|p| !frontier.sessions.contains(p)); }
            // Finish all page task boundaries before sampling worker waits: a
            // later page's admitted tasks can send a message to an earlier page.
            for page in &pages {
                let session = Session::sharing_clock(self.page.client.clone(), page.clone(), self.page.service_clock());
                if self.shared_arrival_service_active() {
                    let result = self.shared_drain_current_tasks(&session).await;
                    if self.collection_reply(result).await? != Some(true) { continue 'collect; }
                    continue;
                }
                let result = self.serviced(session.call(&emulation::FrontcapDrainCurrentTasksParams::default())).await;
                if self.collection_reply(result).await?.is_none() { continue 'collect; }
            }
            let mut live = BTreeMap::new();
            if let Some(frontier) = &shared_frontier { live.extend(frontier.workers.clone()); }
            for page in pages {
                let session = Session::sharing_clock(self.page.client.clone(), page, self.page.service_clock());
                let result = self.serviced(session.call(&emulation::FrontcapGetWorkersParams::default())).await;
                let Some(snapshot) = self.collection_reply(result).await? else { continue 'collect; };
                live.extend(snapshot.workers.into_iter().map(|w| (w.target_id.clone(), w)));
            }
            self.refresh_worker_lineages().await?;
            for worker in live.values().filter(|w| w.thread_started) {
                self.worker_enrollment_result()?;
                let mut books = vec![self.subs.workers.clone()];
                books.extend(self.contexts.lock().unwrap().popups.iter().map(|p| p.workers.clone()));
                let state = books.iter().find_map(|book| {
                    book.lock().unwrap().sessions.iter().find(|(_, s)| s.target == worker.target_id)
                        .map(|(id, s)| (id.clone(), s.enrollment, s.synchronous_wait.clone()))
                });
                if state.is_some() && let Some(wait) = &worker.synchronous_wait_id
                    && self.recovered_worker_wait(&worker.target_id, wait)? {
                    // This inspector connection cannot initialize the blocked
                    // thread or replay its old wait marker until the agent answers.
                    // Native inventory and its recovered held owner prove that
                    // it cannot issue more requests while other windows progress.
                    continue;
                }
                let Some((session, WorkerEnrollment::Ready, wait)) = state else {
                    // Attach/setup or retirement may race the snapshot. Re-read
                    // native liveness rather than waiting forever for a gone host.
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue 'collect;
                };
                if worker.synchronous_wait_id.as_ref().is_some_and(|id| !wait.as_ref().is_some_and(|(seen, _)| seen == id)) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    continue 'collect;
                }
                if let Some(id) = &worker.synchronous_wait_id {
                    // The native inventory and this session's marker must name
                    // the same wait. Earlier renderer announcements precede it.
                    debug_assert_eq!(wait.as_ref().map(|(seen, _)| seen), Some(id));
                    continue;
                }
                let session = Session::sharing_clock(self.page.client.clone(), session, self.page.service_clock());
                if shared_frontier.as_ref().is_some_and(|f| f.workers.contains_key(&worker.target_id)) {
                    // The main thread is in a native arrival wait. This runnable
                    // worker can drain the process's joined current tasks without
                    // waiting for the network responses this round must select.
                    self.serviced(session.call(&emulation::FrontcapDrainCurrentTasksParams::default())).await?;
                }
                match self.serviced(session.call(&emulation::GetVirtualTimeParams::default())).await {
                    Ok(_) => {}
                    // A worker may retire between snapshot and query. Establish
                    // another native boundary rather than treating a failed query
                    // as evidence that its stream has been flushed.
                    Err(ChromiumError::Cdp(cdp::CdpError::SessionDetached)) => continue 'collect,
                    Err(error) => return Err(error),
                }
            }
            self.drain_request_announcements(Duration::ZERO).await?;
            Box::pin(self.freeze_worker_request_owners()).await?;
            // Browser-owned main-script loads run outside the renderer stream
            // flushed above. Their Network id is the native worker target token,
            // including nested workers (announced on the creator's session).
            // A durable binding supplies the same existence evidence on recovery;
            // its old Network notification will not be replayed.
            if live.values().any(|worker| {
                (worker.script_url.starts_with("http://") || worker.script_url.starts_with("https://"))
                    && !self.net.issued.contains_key(&worker.target_id)
                    && !self.net.recovered_network_requests.contains(&worker.target_id)
            }) {
                self.drain_request_announcements(Duration::from_millis(1)).await?;
                // Recheck native liveness too: cancellation or a failed start
                // must not leave us waiting on a constructor that has retired.
                continue 'collect;
            }
            let collected = Box::pin(self.collect_request_interceptions(deadline)).await?;
            if shared_frontier != self.shared_request_frontier().await? { continue 'collect; }
            match collected {
                InterceptionCollection::Ready | InterceptionCollection::MainThreadWait => return Ok(()),
                // A denied preflight needs service admission to publish the
                // application's terminal failure. Refresh after an allowed
                // preflight too, before certifying its dependent request set.
                InterceptionCollection::PreflightAnswered => continue 'collect,
            }
        }
    }

    /// Where a queued interception was announced, when that makes it a member of a round: a renderer
    /// announcement's place, or a browser-issued worker script's own announcement.
    pub(super) fn round_place(&self, event: &fetch::RequestPausedEvent) -> Option<cdp::Arrival> {
        match self.place_of(event) {
            Place::At(at) => Some(at),
            Place::Now => event.network_id.as_ref()
                .filter(|id| self.net.browser_issued.contains(&id.0))
                .and_then(|id| self.net.interceptions.announced_at(&id.0)),
            Place::Unannounced => None,
        }
    }

    /// Whether the active round still has a member to allocate: queued, or announced and not yet
    /// intercepted.
    pub(super) fn round_outstanding(&self) -> bool {
        let Some(round) = &self.request_round else { return false };
        self.queued.iter().any(|(_, event)| self.round_place(event).is_some_and(|at| at <= round.through))
            || self.net.interceptions.earlier_than(cdp::Arrival(round.through.0.saturating_add(1)))
    }

    /// Delivers the round's selected answers in their allocation order and ends the round.
    pub(super) async fn deliver_round(&mut self) -> Result<(), ChromiumError> {
        let Some(round) = self.request_round.take() else { return Ok(()) };
        let several = round.selected.len() > 1;
        for (event, binding) in round.selected {
            match self.deliver_selected(event, &binding).await {
                Err(error) if is_gone(&error) => self.request_gone(&binding.ordinal),
                delivered => delivered?,
            }
            // Process loss once one answer of a round of several is out (`FRONTCAP_INTERCEPTION_FAULT`).
            if several { super::interception::interception_fault_point("after_round_answer"); }
        }
        Ok(())
    }

    /// Collect ordinary Fetch pauses before selecting any of their responses.
    /// A preflight may need an answer to expose its application request; a
    /// terminal Network event discharges a request that will never reach Fetch.
    async fn collect_request_interceptions(&mut self, deadline: Instant) -> Result<InterceptionCollection, ChromiumError> {
        loop {
            self.worker_enrollment_result()?;
            self.drain_request_announcements(Duration::ZERO).await?;
            Box::pin(self.freeze_worker_request_owners()).await?;
            self.take_paused()?;
            self.drain_network().await?;
            // Main-thread waits retain their separate serviced prefix path.
            // Let handle_queued take that path before collecting another round.
            if self.queued.iter().any(|(_, event)| self.blocks_main_thread(event)) { return Ok(InterceptionCollection::MainThreadWait) }
            if let Some(index) = self.queued.iter().position(|(_, event)| {
                !super::interception::is_response_stage(event) &&
                    (event.network_id.as_ref().is_some_and(|id| self.net.preflights.contains(&id.0)) ||
                    (event.request.method == "OPTIONS" && super::adoption::paused_headers(event).keys()
                        .any(|header| header.eq_ignore_ascii_case("access-control-request-method"))))
            }) {
                let (_, event) = self.queued.remove(index);
                Box::pin(self.handle_in_order(event)).await?;
                return Ok(InterceptionCollection::PreflightAnswered);
            }
            let missing = self.net.interceptions.pending_requests().any(|id| {
                !self.queued.iter().any(|(_, event)| event.network_id.as_ref().is_some_and(|network| network.0 == id))
            });
            if !missing { return Ok(InterceptionCollection::Ready) }
            if Instant::now() >= deadline { return Err(ChromiumError::CallTimeout("ordinary request Fetch collection")); }
            self.wait_for_interceptions().await?;
        }
    }
}
