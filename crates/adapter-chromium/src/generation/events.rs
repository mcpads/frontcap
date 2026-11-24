use super::*;

impl Generation {
    fn retire_context(&mut self, unique: &str) {
        if self.world_unique.as_deref() == Some(unique) {
            self.world = None;
            self.world_unique = None;
        }
        if self.main_context_unique.as_deref() == Some(unique) {
            self.main_context = None;
            self.main_context_unique = None;
        }
    }

    // Navigation can destroy an observer between draining events and evaluating the grant.
    // Only this exact browser response permits retiring it; script and other protocol errors propagate.
    pub(super) fn retire_missing_observer(&mut self, unique: &str, error: ChromiumError) -> Result<(), ChromiumError> {
        if !matches!(&error, ChromiumError::Cdp(cdp::CdpError::Protocol { code: -32602, message, .. })
            if message == "uniqueContextId not found") {
            return Err(error);
        }
        self.retire_context(unique);
        self.take_contexts()
    }

    /// Child frames' navigations the page requested, and those that ended without an interception, in
    /// the order the page's session delivered them.
    pub(super) fn take_frame_requests(&mut self) -> Result<(), ChromiumError> {
        enum Frame {
            Requested(page::FrameRequestedNavigationEvent),
            Ended(String),
        }
        let mut events = Vec::new();
        while let Some((at, ev)) = self.subs.requested.try_next_stamped()? {
            events.push((at, Frame::Requested(ev)));
        }
        while let Some((at, ev)) = self.subs.frame_navigated.try_next_stamped()? {
            if ev.frame.parent_id.is_some() {
                events.push((at, Frame::Ended(ev.frame.id.0)));
            }
        }
        while let Some((at, ev)) = self.subs.detached.try_next_stamped()? {
            events.push((at, Frame::Ended(ev.frame_id.0)));
        }
        // A popup's navigations are read at the launch page's boundaries, after their interceptions: they
        // are not waited for (`contexts`).
        if self.popup_id().is_some() {
            return Ok(());
        }
        events.sort_by_key(|(at, _)| *at);
        for (at, ev) in events {
            match ev {
                // Only network navigations in the frame itself are intercepted for it: not `about:`,
                // `data:` or `javascript:` (which replace one pending), nor one a modified click sends to a
                // new tab or window, or a download (the event names the frame that started it). The page's
                // own navigations count too (a link a click handler beacons before); the platform's do not.
                Frame::Requested(r) if r.disposition == page::ClientNavigationDisposition::CurrentTab && (r.frame_id != self.main_frame || !self.own_navigation) => {
                    if r.url.starts_with("http://") || r.url.starts_with("https://") {
                        self.net.interceptions.navigation_requested(&r.frame_id.0, &r.url, self.now, at);
                    } else {
                        self.net.interceptions.navigation_ended(&r.frame_id.0);
                    }
                }
                Frame::Requested(_) => {}
                Frame::Ended(frame) => {
                    self.net.interceptions.navigation_ended(&frame);
                }
            }
        }
        Ok(())
    }

    /// Makes every binding call issued so far arrive, then buffers all pending page events with the virtual
    /// times at which they happened.
    pub(super) async fn drain(&mut self) -> Result<(), ChromiumError> {
        self.worker_enrollment_result()?;
        // A popup addressed by a command: its own events; the rest wait for the launch page's next drain.
        if self.popup_id().is_some() {
            self.drain_sent(Duration::ZERO).await?;
            self.drain_bindings(Duration::ZERO).await?;
            self.renderer_event_barrier().await;
            self.drain_sent(Duration::ZERO).await?;
            return self.popup_events().await;
        }
        self.timeline.stopped(self.now);
        self.forget_receipts().await;
        self.forget_native_dialog_receipts().await;
        self.forget_stream_receipts().await?;
        self.take_frames(Duration::ZERO).await?;
        self.drain_sent(Duration::ZERO).await?;
        self.drain_bindings(Duration::ZERO).await?;
        self.renderer_event_barrier().await;
        // The round trip also brings every `requestWillBeSent` the page emitted before it.
        self.drain_sent(Duration::ZERO).await?;
        self.drain_bindings(Duration::ZERO).await?;
        self.drain_page_events().await
    }

    /// An ordered renderer reply brings preceding observer/network notifications.
    /// A controlled navigation may hold Runtime.evaluate until its response is
    /// answered; the native clock query crosses that barrier without page code.
    async fn renderer_event_barrier(&self) {
        let Some(world) = self.world.clone() else { return; };
        if self.patched {
            let _ = self.page.call(&emulation::FrontcapGetClockParams::default()).await;
        } else {
            let _ = self.page.eval("0", Some(world)).await;
        }
    }

    pub(super) fn take_contexts(&mut self) -> Result<(), ChromiumError> {
        self.contexts.lock().unwrap().check_enrollment()?;
        enum Context {
            Created(runtime::ExecutionContextCreatedEvent),
            Destroyed(String),
            Cleared,
        }
        let mut events = Vec::new();
        while let Some((at, ev)) = self.subs.context.try_next_stamped()? { events.push((at, Context::Created(ev))); }
        while let Some((at, ev)) = self.subs.context_destroyed.try_next_stamped()? {
            events.push((at, Context::Destroyed(ev.execution_context_unique_id)));
        }
        while let Some((at, _)) = self.subs.contexts_cleared.try_next_stamped()? { events.push((at, Context::Cleared)); }
        events.sort_by_key(|(at, _)| *at);
        for (at, event) in events {
            let ev = match event {
                Context::Created(ev) => ev,
                Context::Destroyed(unique) => {
                    self.retire_context(&unique);
                    self.stream_contexts.retain(|_, owner| owner != &unique);
                    continue;
                }
                Context::Cleared => {
                    self.world = None;
                    self.world_unique = None;
                    self.main_context = None;
                    self.main_context_unique = None;
                    self.stream_contexts.clear();
                    continue;
                }
            };
            // Numeric context ids may be reused after renderer replacement. Retain the
            // notification order so buffered exceptions keep their original context.
            self.exception_contexts.entry(ev.context.id.0).or_default().push((at, ev.context.unique_id.clone()));
            let frame = ev.context.aux_data.as_ref().and_then(|d| d.get("frameId")).and_then(|f| f.as_str());
            let default = ev.context.aux_data.as_ref().and_then(|d| d.get("isDefault")).and_then(|b| b.as_bool()) == Some(true);
            if let Some(frame) = frame {
                if default { self.stream_contexts.insert(frame.to_string(), ev.context.unique_id.clone()); }
                if ev.context.name == WORLD
                    && let Some(owner) = self.stream_contexts.get(frame) {
                    self.stream_observers.entry(ev.context.id.0).or_default().push((at, owner.clone()));
                }
            }
            if default && frame == Some(self.main_frame.0.as_str()) {
                self.main_context = Some(ev.context.id.clone());
                self.main_context_unique = Some(ev.context.unique_id.clone());
            }
            if ev.context.name == WORLD && frame == Some(self.main_frame.0.as_str()) && self.world_unique.as_ref() != Some(&ev.context.unique_id) {
                // A new document's observer: its reports count from 1, and earlier values are another
                // document's.
                self.world = Some(ev.context.id);
                self.world_unique = Some(ev.context.unique_id);
                self.report_epoch += 1;
                self.dom_base += self.dom_doc;
                self.dom_doc = 0;
                let (cursor, samples) = self.resume_cursor.take().unwrap_or_default();
                self.max_rseq = cursor;
                self.written_rseq = cursor;
                self.last_samples = samples;
            }
        }
        Ok(())
    }

    /// Execution contexts and observer reports, without any command to the page.
    pub(super) async fn drain_bindings(&mut self, first_wait: Duration) -> Result<(), ChromiumError> {
        self.drain_bindings_until(first_wait, None).await
    }

    /// Publish reports announced before a request before recording that request. A Fetch pause and
    /// a clock expiry may both be ready: their selection must not reorder earlier observer evidence.
    pub(super) async fn drain_bindings_before(&mut self, before: cdp::Arrival) -> Result<(), ChromiumError> {
        self.drain_bindings_until(Duration::ZERO, Some(before)).await
    }

    async fn drain_bindings_until(&mut self, first_wait: Duration, before: Option<cdp::Arrival>) -> Result<(), ChromiumError> {
        self.take_contexts()?;
        let mut wait = first_wait;
        loop {
            let next = match self.deferred_binding.take() {
                Some(ev) => Some(ev),
                None => self.subs.binding.next_timeout_stamped(wait).await?,
            };
            let Some((at, ev)) = next else { break };
            // Both subscriptions share the connection's arrival sequence. Read announcements that
            // preceded this report even if a clock expiry made us drain reports before Fetch pauses.
            self.drain_request_announcements(Duration::ZERO).await?;
            let blocked = self.net.interceptions.first_request().is_some_and(|issued| at > issued);
            if blocked || before.is_some_and(|before| at >= before) {
                self.deferred_binding = Some((at, ev));
                break;
            }
            wait = Duration::ZERO;
            if ev.name != instrument::BINDING { continue; }
            let mut v: serde_json::Value = serde_json::from_str(&ev.payload).map_err(|e| ChromiumError::Shape(e.to_string()))?;
            if v["type"] == "stream" {
                if let Some(id) = v["native_receipt"].as_str() {
                    if self.seen_stream_receipts.contains(id) { continue; }
                    let inventory = self.page.call(&page::FrontcapGetStreamReportsParams::default()).await?;
                    let receipt = inventory.receipts.into_iter().find(|receipt| receipt.receipt_id == id)
                        .ok_or_else(|| ChromiumError::Shape("announced stream receipt is not retained".into()))?;
                    v["stream_receipt"] = serde_json::to_value(StreamReceipt {
                        id: receipt.receipt_id, frame: receipt.frame_id.0, document: receipt.document_id,
                    }).map_err(|error| ChromiumError::Shape(error.to_string()))?;
                }
                let owner = self.stream_observers.get(&ev.execution_context_id.0)
                    .and_then(|epochs| epochs.iter().rev().find(|(created, _)| *created <= at)).map(|(_, owner)| owner);
                let Some(owner) = owner else { continue };
                v["stream_owner"] = serde_json::Value::String(owner.clone());
            } else if Some(&ev.execution_context_id) != self.world.as_ref() { continue; }
            self.take_report(&v)?;
        }
        Ok(())
    }

    /// Handles one observer report, live or read back after a relaunch.
    pub(super) fn take_report(&mut self, v: &serde_json::Value) -> Result<(), ChromiumError> {
        let t = VirtualTime(v["t_us"].as_str()
            .ok_or_else(|| ChromiumError::Shape("observer report has no microsecond timestamp".into()))?
            .parse::<u64>()
            .map_err(|e| ChromiumError::Shape(format!("observer timestamp: {e}")))?);
        match v["type"].as_str() {
            Some("sample") => {
                let rseq = v["rseq"].as_u64().unwrap_or(0);
                if rseq != 0 && rseq <= self.max_rseq {
                    return Ok(());
                }
                self.max_rseq = self.max_rseq.max(rseq);
                let name = WatchName::new(v["name"].as_str().unwrap_or_default()).map_err(|e| ChromiumError::Shape(e.to_string()))?;
                let value: SampleValue = serde_json::from_value(v["value"].clone()).map_err(|e| ChromiumError::Shape(e.to_string()))?;
                let point: PointKind = serde_json::from_value(v["point"].clone()).map_err(|e| ChromiumError::Shape(e.to_string()))?;
                self.last_samples.insert(name.clone(), value);
                self.record(t, EventKind::Watch(WatchSample { name, value, point, rseq, document: self.report_epoch, context: self.popup_id() }));
            }
            Some("message") => {
                let rseq = v["rseq"].as_u64().unwrap_or(0);
                if rseq != 0 && rseq <= self.max_rseq { return Ok(()); }
                self.max_rseq = self.max_rseq.max(rseq);
                self.record(t, EventKind::Message(frontcap_core::event::MessageRecord {
                    origin: v["origin"].as_str().unwrap_or_default().to_string(),
                    data: v["data"].as_str().unwrap_or_default().to_string(),
                    document: self.report_epoch, rseq, context: self.popup_id(),
                }));
                if let Some(q) = self.quiet.as_mut() { q.activity(t); }
            }
            Some("focus") => {
                let rseq = v["rseq"].as_u64().unwrap_or(0);
                if rseq != 0 && rseq <= self.max_rseq { return Ok(()); }
                self.max_rseq = self.max_rseq.max(rseq);
                self.record(t, EventKind::Focus(FocusRecord {
                    target: v["target"].as_str().unwrap_or_default().to_string(),
                    focused: v["focused"].as_bool().unwrap_or(false), document: self.report_epoch, rseq,
                    context: self.popup_id(),
                }));
                if let Some(q) = self.quiet.as_mut() { q.activity(t); }
            }
            Some("mutations") => {
                let total = v["total"].as_u64().unwrap_or(self.dom_doc + v["n"].as_u64().unwrap_or(0));
                self.state_revision = self.state_revision.saturating_add(total.saturating_sub(self.dom_doc));
                self.dom_doc = total;
                if let Some(q) = self.quiet.as_mut() {
                    q.activity(t);
                }
            }
            Some("dialog") => {
                // Announced by the main world's replacements; anything else is not a platform record.
                let Ok(kind) = serde_json::from_value::<DialogKind>(v["kind"].clone()) else { return Ok(()) };
                let message = v["message"].as_str().unwrap_or_default().to_string();
                let answer = v["answer"].as_str().unwrap_or_default().to_string();
                let rseq = v["rseq"].as_u64().unwrap_or(0);
                if rseq != 0 && rseq <= self.max_rseq { return Ok(()) }
                self.max_rseq = self.max_rseq.max(rseq);
                let record = DialogRecord { kind, message, answer, native: None,
                    observer: Some(DialogObserver { document: self.report_epoch, rseq }), context: self.popup_id() };
                if v["native"].as_bool() == Some(true) {
                    self.note_record(&EventKind::Dialog(record.clone()));
                    self.pending.push((t, PendingEvent::NativeDialogOpening(record)));
                } else {
                    self.record(t, EventKind::Dialog(record));
                }
                if let Some(q) = self.quiet.as_mut() {
                    q.activity(t);
                }
            }
            Some("taint") => self.unseeded(t, v["detail"].as_str().unwrap_or_default()),
            Some("new_window") => {
                let detail = format!("window.open({}) was refused: a new window runs outside the platform", v["url"].as_str().unwrap_or_default());
                self.record(t, EventKind::Taint(TaintRecord { source: TaintSource::NewWindow, detail }));
            }
            Some("stream") => {
                let receipt: Option<StreamReceipt> = v.get("stream_receipt")
                    .map(|value| serde_json::from_value(value.clone()))
                    .transpose().map_err(|error| ChromiumError::Shape(error.to_string()))?;
                if receipt.as_ref().is_some_and(|receipt| self.seen_stream_receipts.contains(&receipt.id)) {
                    return Ok(());
                }
                let page_stream = v["id"].as_u64().unwrap_or(0);
                let op = v["op"].as_str().unwrap_or_default();
                if op == "received_open" { return Ok(()); } // The delivery path publishes this receipt.
                let close = op == "close";
                let owner = v["stream_owner"].as_str().map(str::to_string)
                    .or_else(|| self.stream_contexts.get(&self.main_frame.0).cloned());
                let Some(owner) = owner else { return Ok(()) };
                if close && let Some(handshake) = self.net.closed_handshakes.remove(&(owner.clone(), page_stream)) {
                    self.record_early_stream_close(handshake, t, receipt)?;
                    return Ok(());
                }
                let found = self.net.streams.iter().find(|(_, s)| s.page_stream == page_stream
                    && s.context.as_ref() == Some(&owner)).map(|(o, s)| (o.clone(), s.websocket));
                let Some((ordinal, websocket)) = found else {
                    // Closed before the platform saw it open: its handshake, when intercepted, opens nothing.
                    if close {
                        self.net.early_close_reports.insert((owner.clone(), page_stream), (t, receipt));
                        self.net.closed_early.insert((owner, page_stream));
                    }
                    return Ok(());
                };
                if let Some(receipt) = &receipt {
                    stream_fault_point("before_trace");
                    self.seen_stream_receipts.insert(receipt.id.clone());
                }
                let data = if close { None } else { v["data"].as_str().map(str::to_string) };
                if let (Some(p), Some(text)) = (self.pass.get(&ordinal), &data)
                    && let Some(send) = &p.upstream.send
                {
                    let _ = send.send(text.clone());
                }
                self.record(t, EventKind::Stream(StreamRecord { context: self.popup_id(), stream: ordinal.clone(), direction: StreamDirection::Out, event: if close { StreamEventKind::Close } else { StreamEventKind::Message }, data, page_stream: None, document: None, receipt: receipt.clone() }));
                if close && websocket {
                    // The wrapper completes the closing handshake at once and tells the page.
                    self.record(t, EventKind::Stream(StreamRecord { context: self.popup_id(), stream: ordinal.clone(), direction: StreamDirection::In, event: StreamEventKind::Close, data: None, page_stream: None, document: None, receipt }));
                }
                if let Some(q) = self.quiet.as_mut() {
                    q.activity(t);
                }
                if close {
                    self.end_stream(&ordinal);
                }
            }
            Some("navigation_committed") => {
                let rseq = v["rseq"].as_u64().unwrap_or(0);
                if rseq != 0 && rseq <= self.max_rseq { return Ok(()); }
                self.max_rseq = self.max_rseq.max(rseq);
                self.record(t, EventKind::Navigation(NavigationRecord {
                    rseq, url: v["url"].as_str().unwrap_or_default().to_string(),
                    document: self.documents, context: self.popup_id(),
                }));
            }
            // The launch page's navigations only: a popup's document requests are timed like a child frame's.
            Some("navigate") if self.popup_id().is_some() => {}
            Some("navigate") => {
                self.last_navigate = Some(t);
                let url = v["url"].as_str().unwrap_or_default();
                self.navigates.push((url.split('#').next().unwrap_or_default().to_string(), t));
            }
            _ => {}
        }
        Ok(())
    }

    /// Takes a committed document once the browser has finished its commit. The renderer reports the
    /// commit before the browser may have processed it, and until then the target's browser commands
    /// address a frame that is not yet current. The browser needs nothing more from the platform to
    /// finish it.
    pub(super) async fn take_navigated(&mut self, ev: page::FrameNavigatedEvent) -> Result<(), ChromiumError> {
        if ev.frame.parent_id.is_none() && self.popup_id().is_none() && self.committed_loader.as_deref() == Some(ev.frame.loader_id.0.as_str()) {
            return Ok(());
        }
        if ev.frame.parent_id.is_none() && self.patched {
            match self.page.call(&page::FrontcapAwaitCommitParams::new(ev.frame.loader_id.clone())).await {
                Ok(_) => {}
                // A window closed meanwhile has no frame left to address.
                Err(ChromiumError::Cdp(e)) if contexts::session_gone(&e) => {}
                Err(e) => return Err(e),
            }
        }
        self.take_committed(ev);
        Ok(())
    }

    fn take_committed(&mut self, ev: page::FrameNavigatedEvent) {
        // A popup's new document: its own numbering; the network state is the launch page's.
        if ev.frame.parent_id.is_none() && let Some(context) = self.popup_id() {
            self.documents += 1;
            let t = self.now.max(self.log.last_time());
            self.record(t, EventKind::Navigation(NavigationRecord { rseq: 0, url: ev.frame.url, document: self.documents, context: Some(context) }));
            if let Some(q) = self.quiet.as_mut() {
                q.activity(t);
            }
            return;
        }
        if ev.frame.parent_id.is_none() {
            // The streams of the document left are gone with it.
            let left: Vec<String> = self.net.streams.keys().cloned().collect();
            for o in left {
                self.end_stream(&o);
            }
            // Earlier documents still loading were replaced: the engine sends them no completion. One whose
            // response arrived ended with it; the others failed.
            let current = self.document_request.as_ref().map(|(id, _)| id.clone());
            let replaced: Vec<String> = self.net.loading_documents.iter().filter(|id| Some(*id) != current.as_ref()).cloned().collect();
            for id in replaced {
                let ok = self.net.status.contains_key(&id);
                self.end_passthrough(&id, ok);
            }
            // Only this window's replaced document lost its interceptions. Popup
            // requests use the same network book and may still await delivery.
            let gone: Vec<String> = self.net.held.iter().filter(|(_, h)|
                h.document <= self.documents && h.attempted.is_none()
                    && self.context_of_fetch(&h.fetch_id).is_none()
            ).map(|(o, _)| o.clone()).collect();
            for o in gone {
                self.end_hold(&o);
            }
            self.documents += 1;
            self.committed_loader = Some(ev.frame.loader_id.0.clone());
            // Fetches of the replaced document end with it, reported or not.
            self.net.answered.clear();
            let t = self.document_request.as_ref().map_or(self.now, |(_, t)| *t);
            self.record(t, EventKind::Navigation(NavigationRecord { rseq: 0, url: ev.frame.url, document: self.documents, context: None }));
            if let Some(q) = self.quiet.as_mut() {
                q.activity(t);
            }
        }
    }

    /// A navigation within the current document (a fragment, `pushState`, a same-document history
    /// entry): recorded with the current document's number, at the observer's `navigate` report.
    pub(super) fn take_within(&mut self, ev: page::NavigatedWithinDocumentEvent) {
        if ev.frame_id != self.main_frame {
            return;
        }
        self.within_document += 1;
        // The observer retains committed same-document changes across recorder loss. CDP still
        // advances the live navigation barrier, but must not publish a second copy of that receipt.
        if self.world.is_some() { return; }
        let navigated = if self.popup_id().is_some() { None } else { self.last_navigate };
        let t = navigated.unwrap_or(self.now).max(self.log.last_time());
        self.record(t, EventKind::Navigation(NavigationRecord { rseq: 0, url: ev.url, document: self.documents, context: self.popup_id() }));
    }

    pub(super) async fn drain_page_events(&mut self) -> Result<(), ChromiumError> {
        if self.popup_id().is_some() {
            return self.popup_events().await;
        }
        self.take_contexts()?;
        // Before navigations: a dialog the engine raised is queued before it is answered, so it precedes
        // the navigation it lets through. A `beforeunload` opens in the task that started the navigation;
        // other engine dialogs (frames of another origin) are stamped at the step's current time.
        // Every window a page opens is announced; only the refusing policy refuses them all.
        let closed = std::mem::take(&mut *self.closed_windows.lock().unwrap());
        if self.profile.popups.is_block() {
            for url in closed {
                let detail = format!("a new window ({url}) was not created: a new window runs outside the platform");
                self.record(self.now.max(self.log.last_time()), EventKind::Taint(TaintRecord { source: TaintSource::NewWindow, detail }));
            }
        }
        self.record_contexts();
        self.drain_native_dialog_receipts().await?;
        let answered = std::mem::take(&mut *self.native_dialogs.lock().unwrap());
        for d in answered {
            let t = match (d.kind, self.last_navigate) {
                // The launch page's navigation; a popup's prompt is not that navigation's.
                (DialogKind::Beforeunload, Some(t)) if d.context.is_none() => t,
                _ => self.now,
            };
            self.record(t.max(self.log.last_time()), EventKind::Dialog(d));
        }
        while let Some(ev) = self.subs.navigated.next_timeout(Duration::ZERO).await? {
            self.take_navigated(ev).await?;
        }
        while let Some(ev) = self.subs.within.try_next()? {
            self.take_within(ev);
        }
        while let Some(ev) = self.subs.attached.try_next()? {
            let kind = ev.target_info.r#type.as_str();
            if kind.contains("worker") {
                // A document that reaches the top's observer announced the worker when it constructed
                // it (seed.js), before its target could exist: that report, at the page's instant, comes
                // first. The target names only the others (another origin's frame, a worker's worker),
                // as of when the browser announced it.
                self.drain_bindings(Duration::ZERO).await?;
                self.unseeded(self.now, &format!("{kind} {}", ev.target_info.url));
            }
        }
        self.take_exception_receipts().await?;
        while let Some(tagged) = self.subs.exceptions.try_next_tagged()? {
            let (at, ev) = (tagged.arrival, tagged.event);
            let t = ms_epoch_to_virtual(ev.timestamp.0, self.profile.initial_date_ms);
            let d = &ev.exception_details;
            // A popup's, as the engine reports it (`contexts`).
            if let Some(context) = self.context_of_session(tagged.session.as_deref()) {
                let detail = d.exception.as_ref().and_then(|e| e.description.clone()).unwrap_or_default();
                let text = match detail.lines().next() {
                    Some(first) if !first.is_empty() => format!("{} {first}", d.text),
                    _ => d.text.clone(),
                };
                self.record(t, EventKind::Exception(ExceptionRecord { text, native: None, receipt: None, context: Some(context) }));
                if let Some(q) = self.quiet.as_mut() {
                    q.activity(t);
                }
                continue;
            }
            // Patched engines report browser-owned receipts, including after renderer loss.
            if self.patched { continue; }
            let context = d.execution_context_id.as_ref()
                .and_then(|id| self.exception_contexts.get(&id.0))
                .and_then(|contexts| contexts.iter().rev().find(|(created, _)| *created <= at))
                .map(|(_, unique)| unique)
                .ok_or_else(|| ChromiumError::Unsupported("exception execution context identity is unavailable".into()))?;
            let native = NativeException { context: context.clone(), id: d.exception_id };
            if !self.recorded_exceptions.insert(native.clone()) { continue; }
            let detail = d.exception.as_ref().and_then(|e| e.description.clone()).unwrap_or_default();
            let text = match detail.lines().next() {
                Some(first) if !first.is_empty() => format!("{} {first}", d.text),
                _ => d.text.clone(),
            };
            self.record(t, EventKind::Exception(ExceptionRecord { text, native: Some(native), receipt: None, context: None }));
            if let Some(q) = self.quiet.as_mut() {
                q.activity(t);
            }
        }
        while let Some(tagged) = self.subs.console.try_next_tagged()? {
            let context = self.context_of_session(tagged.session.as_deref());
            let ev = tagged.event;
            let t = ms_epoch_to_virtual(ev.timestamp.0, self.profile.initial_date_ms);
            let level = serde_json::to_value(ev.r#type).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
            let text = ev
                .args
                .iter()
                .map(|a| {
                    a.value
                        .as_ref()
                        .map(|v| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))
                        .or_else(|| a.description.clone())
                        .unwrap_or_default()
                })
                .collect::<Vec<_>>()
                .join(" ");
            self.record(t, EventKind::Console(ConsoleRecord { level, text, context, source: None }));
            if let Some(q) = self.quiet.as_mut() {
                q.activity(t);
            }
        }
        // The browser's own messages (a frame it refused to show, a request it blocked or that failed
        // CORS, a deprecation): the console a developer sees, with the source that reported each.
        while let Some(tagged) = self.subs.log.try_next_tagged()? {
            let context = self.context_of_session(tagged.session.as_deref());
            let entry = tagged.event.entry;
            let t = ms_epoch_to_virtual(entry.timestamp.0, self.profile.initial_date_ms);
            let name = |v: serde_json::Result<serde_json::Value>| v.ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default();
            let (level, source) = (name(serde_json::to_value(entry.level)), name(serde_json::to_value(entry.source)));
            let record = ConsoleRecord { level, text: entry.text, context, source: Some(source) };
            match entry.network_request_id.map(|id| id.0).filter(|id| self.net.passthrough.contains_key(id)) {
                Some(id) => self.net.deferred_console.entry(id).or_default().push((t, record)),
                None => self.record(t, EventKind::Console(record)),
            }
            if let Some(q) = self.quiet.as_mut() {
                q.activity(t);
            }
        }
        self.drain_network().await?;
        // Each open popup's own events, at the same boundary.
        self.drain_popups().await
    }

    /// The dialog rules in force.
    pub fn dialog_rules(&self) -> frontcap_core::dialog::DialogRules {
        self.dialog_rules.lock().unwrap().clone()
    }

    /// Puts `rules` in force for every dialog from now on: new documents get them from their start, the
    /// current document's shims at once (through the observer, on the launch's rules event), and native
    /// dialogs from the engine's answering task.
    pub async fn set_dialog_rules(&mut self, rules: frontcap_core::dialog::DialogRules) -> Result<(), ChromiumError> {
        self.alive()?;
        if !self.patched && rules.0.iter().any(|r| r.answer == frontcap_core::dialog::DialogAnswer::Hold) {
            return Err(ChromiumError::Unsupported("native dialog holds require the patched engine".into()));
        }
        self.page.call(&page::RemoveScriptToEvaluateOnNewDocumentParams::new(self.dialogs_script.clone())).await?;
        self.dialogs_script = self.page.call(&page::AddScriptToEvaluateOnNewDocumentParams::new(instrument::dialogs_script(&self.launch_id, &rules, self.profile.popups))).await?.identifier;
        let (_, rules_event) = instrument::dialog_names(&self.launch_id);
        let detail = serde_json::to_string(&serde_json::to_string(&rules).expect("rules serialize")).expect("a string serializes");
        // Context-created notifications may still be queued even though the initial wrapper exists.
        // Drain before deciding whether current-document rules need updating.
        self.drain().await?;
        if self.world.is_some() {
            self.world_eval(&format!("document.dispatchEvent(new CustomEvent({}, {{ detail: {detail} }})), true", serde_json::to_string(&rules_event).unwrap())).await?;
        }
        *self.dialog_rules.lock().unwrap() = rules;
        Ok(())
    }

    /// The browser's page targets: the generation's page, and any window the page opened and the platform
    /// has not closed yet.
    /// Crashes the page's renderer, as an engine fault would.
    #[cfg(feature = "test-hooks")]
    pub fn crash_renderer(&self) -> Result<(), ChromiumError> {
        self.page.client.send(Some(&self.page.id), &page::CrashParams::default())?;
        Ok(())
    }

    /// Run one real page task after the next round member is bound, before the remaining members.
    #[cfg(feature = "test-hooks")]
    pub fn evaluate_after_round_selection(&mut self, expression: String) {
        self.after_round_selection = Some(expression);
    }

    /// Hands over interceptions whose request contains `contains` `by` late, as a browser slow to report
    /// them would.
    #[cfg(feature = "test-hooks")]
    pub fn delay_interceptions(&self, contains: &str, by: Duration) {
        self.page.client.delay_events("Fetch.requestPaused", contains, by);
    }

    /// Delay matching Network announcements independently of their Fetch pauses.
    #[cfg(feature = "test-hooks")]
    pub fn delay_request_announcements(&self, contains: &str, by: Duration) {
        self.page.client.delay_events("Network.requestWillBeSent", contains, by);
    }

    /// Delay matching `Page.frameRequestedNavigation` events: the page's navigation is then placed by
    /// its report alone, as when its pause reaches the platform first.
    #[cfg(feature = "test-hooks")]
    pub fn delay_navigation_requests(&self, contains: &str, by: Duration) {
        self.page.client.delay_events("Page.frameRequestedNavigation", contains, by);
    }

    /// Exercise a popup closing before the browser replies to its mouse release.
    #[cfg(feature = "test-hooks")]
    pub fn omit_mouse_release_answer(&self) {
        self.page.client.omit_next_answer("Input.dispatchMouseEvent", "mouseReleased");
    }

    /// Drop the next worker's Network.enable reply after launch, before it may run.
    #[cfg(feature = "test-hooks")]
    pub fn omit_worker_network_enable_answer(&self) {
        self.page.client.omit_next_answer("Network.enable", "");
    }

    #[cfg(feature = "test-hooks")]
    pub async fn page_targets(&self) -> Result<usize, ChromiumError> {
        let targets = self.page.client.call(None, &target::GetTargetsParams::default()).await?.target_infos;
        Ok(targets.iter().filter(|t| t.r#type == "page").count())
    }
}


impl Generation {
    async fn take_exception_receipts(&mut self) -> Result<(), ChromiumError> {
        if !self.patched { return Ok(()) }
        if !self.exception_receipts_ready.is_empty() {
            self.log.sync()?;
            let ids: Vec<_> = self.exception_receipts_ready.iter().cloned().collect();
            self.page.call(&page::FrontcapForgetExceptionsParams { exception_ids: ids }).await?;
            self.exception_receipts_ready.clear();
            exception_fault_point("after_forget");
        }
        let inventory = self.page.call(&page::FrontcapGetExceptionsParams::default()).await?;
        if !inventory.receipts.is_empty() { exception_fault_point("before_trace"); }
        for receipt in inventory.receipts {
            if !self.seen_exception_receipts.insert(receipt.exception_id.clone()) { continue; }
            let epoch = receipt.unix_time_us.parse::<i128>()
                .map_err(|_| ChromiumError::Shape("native exception has an invalid timestamp".into()))?;
            let elapsed = epoch - i128::from(self.profile.initial_date_ms) * 1000;
            let t = VirtualTime(u64::try_from(elapsed)
                .map_err(|_| ChromiumError::Shape("native exception lies outside the generation timeline".into()))?);
            let text = receipt.text.lines().next().unwrap_or_default().to_string();
            self.record(t, EventKind::Exception(ExceptionRecord {
                text, native: None,
                receipt: Some(ExceptionReceipt { id: receipt.exception_id, frame: receipt.frame_id.0 }),
                context: None,
            }));
            if let Some(q) = self.quiet.as_mut() { q.activity(t); }
        }
        Ok(())
    }
}

pub(super) fn exception_fault_point(point: &str) {
    #[cfg(feature = "fault-injection")]
    if std::env::var("FRONTCAP_EXCEPTION_FAULT").is_ok_and(|wanted| wanted == point) {
        std::process::abort();
    }
    #[cfg(not(feature = "fault-injection"))]
    let _ = point;
}


pub(super) fn stream_fault_point(point: &str) {
    #[cfg(feature = "fault-injection")]
    if std::env::var("FRONTCAP_STREAM_FAULT").is_ok_and(|wanted| wanted == point) {
        std::process::abort();
    }
    #[cfg(not(feature = "fault-injection"))]
    let _ = point;
}
