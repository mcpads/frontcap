use super::*;

impl Generation {
    /// A stream the page opened (streams.js): its handshake request, held at the instant it was issued,
    /// decides it by the rules. While the request holds virtual time there, the platform delivers the
    /// handshake's outcome and every event due at that instant, then answers the request; later events go
    /// on the agenda, each delivered at its instant (the delivery barrier).
    pub(super) async fn open_stream(&mut self, ev: fetch::RequestPausedEvent, t: VirtualTime, page_stream: u64, websocket: bool, url: String, last_event_id: Option<String>) -> Result<(), ChromiumError> {
        let network_id = ev.network_id.as_ref().map(|id| id.0.clone());
        // Decided from the page's announcement before this interception arrived: only released now.
        if network_id.as_ref().is_some_and(|id| self.net.abandoned_anchors.contains(id)) {
            return self.fail_anchor(ev.request_id).await;
        }
        if let Some(id) = &network_id {
            self.net.anchors.remove(id);
        }
        // The frame tree as the task that opened the stream left it: that task's context events come
        // before the answer.
        let tree = self.page.call(&page::GetFrameTreeParams::default()).await?.frame_tree;
        // The document that opened it: a navigation committed just before is taken first.
        while let Some(nav) = self.subs.navigated.try_next()? {
            self.take_navigated(nav).await?;
        }
        self.take_contexts()?;
        let context = match self.stream_contexts.get(&ev.frame_id.0).cloned() {
            Some(context) => context,
            None => {
                // Missing instrumentation in a living frame is still an error. An absent frame,
                // however, cannot own a connection and must not stop unrelated sibling streams.
                if frame_in(&tree, &ev.frame_id) {
                    return Err(ChromiumError::Shape(format!("stream owner context missing for {}", ev.frame_id.0)));
                }
                let frame = ev.frame_id.clone();
                return self.abandon_stream(Some(ev.request_id), &frame, network_id, t, page_stream, url).await;
            }
        };
        let selected = match self.net.stream_selections.get(&ev.request_id.0) {
            Some(selected) => selected.clone(),
            None => {
                let ordinal = self.net.ordinal();
                let rule = self.net.rules.first_route_match("GET", &url)
                    .expect("effective rule set ends with default-block");
                let selected = crate::bindings::StreamSelection {
                    target_id: self.target_id.clone(), fetch_id: ev.request_id.0.clone(),
                    ordinal, rules_revision: self.net.rules_revision, rule: rule.id.clone(),
                    owner: context.clone(), page_stream, issued_us: t.0,
                };
                self.bindings.append(&crate::bindings::BindingEntry::StreamSelected(Box::new(selected.clone())))?;
                self.net.stream_selections.insert(ev.request_id.0.clone(), selected.clone());
                selected
            }
        };
        if selected.owner != context || selected.page_stream != page_stream {
            return Err(ChromiumError::Shape("retained stream selection has a different owner".into()));
        }
        let ordinal = selected.ordinal;
        let t = VirtualTime(selected.issued_us);
        let revision = selected.rules_revision;
        let rule = self.net.rules_at(revision)
            .and_then(|rules| rules.rules.iter().find(|rule| rule.id == selected.rule)).cloned()
            .ok_or_else(|| ChromiumError::Shape(format!("{ordinal}: selected stream rule is missing")))?;
        super::events::stream_fault_point("before_stream_binding");
        // The request's record is written once its rule has decided (a replayed stream names its session).
        let request_at = self.pending.len();
        // The engine may have let virtual time pass the issue before the request reached the platform
        // (a document load, a held request's segment): events due by then are delivered now, late.
        let held_at = self.read_time().await?.max(self.now);
        self.now = held_at;
        let mut handshake_at = held_at;
        let ok = if websocket { 101 } else { 200 };
        let mut events: Vec<ScriptEvent> = Vec::new();
        // A recorded session's events are the server's: ordered after the agenda's others at an instant.
        let mut replayed = false;
        let mut recorded_handshake = None;
        let (key, _) = instrument::stream_names(&self.launch_id);
        let bind = format!("window[{}].bind({}, {})", serde_json::to_string(&key).unwrap(), page_stream, serde_json::to_string(&ordinal).unwrap());
        let bound = self.page.eval_unique(&bind, &context).await?;
        if bound != serde_json::Value::Bool(true) && !bound.is_null() {
            return Err(ChromiumError::Shape(format!("{ordinal}: stream owner binding refused")));
        }
        super::events::stream_fault_point("after_stream_binding");
        // A page can close before its anchor or the isolated close report reaches the adapter.
        // The wrapper's absence is definitive; do not open an upstream connection for that stream.
        let closed_early = self.net.closed_early.remove(&(context.clone(), page_stream)) || bound.is_null();
        let status = match &rule.action {
            _ if closed_early => 0,
            Action::StreamMock { handshake, messages, close_at_ms } => {
                let status = if handshake.status == 0 { ok } else { handshake.status };
                if status == ok {
                    events.push((t, StreamEventKind::Open, None, None, None, None));
                    for m in messages {
                        events.push((t.saturating_add(VirtualDuration::from_ms(m.at_ms)), StreamEventKind::Message, Some(m.data.clone()), m.event.clone(), m.id.clone(), None));
                    }
                    if let Some(c) = close_at_ms {
                        events.push((t.saturating_add(VirtualDuration::from_ms(*c)), StreamEventKind::Close, None, None, None, Some(1000)));
                    }
                } else {
                    events.push((t, StreamEventKind::Error, None, None, None, None));
                }
                status
            }
            Action::Passthrough | Action::Record { .. } if self.net.stream_connections.contains_key(&ev.request_id.0) => {
                // No durable opening exists for this retained anchor. The previous recorder may
                // have connected already; a retry could repeat server-side effects.
                let connection = self.net.stream_connections[&ev.request_id.0].clone();
                if let Some((name, key, session)) = connection.record {
                    self.net.stream_entry = Some(session);
                    recorded_handshake = Some((name, frontcap_core::cassette::StreamEvent {
                        key, session, at_us: 0, event: "error".into(), data: None,
                        sse_event: None, sse_id: None, code: None,
                    }));
                }
                self.record(held_at, EventKind::Taint(TaintRecord {
                    source: TaintSource::PassthroughContent,
                    detail: format!("{ordinal}: recorder loss interrupted an upstream stream connection; outcome unknown, connection not retried"),
                }));
                events.push((held_at, StreamEventKind::Error, None, None, None, None));
                0
            }
            Action::Passthrough | Action::Record { .. } if self.upstream.is_some() => {
                // The platform connects to the server itself while the handshake request holds virtual time,
                // for a wall time well inside the watchdog: a server that does not answer fails the stream.
                let record = match &rule.action {
                    Action::Record { cassette } => {
                        let key = frontcap_core::cassette::Key::of("GET", &url, None);
                        let session = self.net.recorded_sessions.entry(key.clone()).or_default();
                        *session += 1;
                        self.net.stream_entry = Some(*session - 1);
                        Some((frontcap_core::cassette::recording_name(revision, cassette), key, *session - 1))
                    }
                    _ => None,
                };
                let connection = crate::bindings::StreamConnection {
                    target_id: self.target_id.clone(), fetch_id: ev.request_id.0.clone(), record: record.clone(),
                };
                self.bindings.append(&crate::bindings::BindingEntry::StreamConnecting(Box::new(connection.clone())))?;
                self.net.stream_connections.insert(ev.request_id.0.clone(), connection);
                super::events::stream_fault_point("before_upstream_connect");
                let up = self.upstream.clone().expect("checked");
                let mut upstream = if websocket { up.open_ws(&url) } else { up.open_sse(&url, last_event_id.as_deref()) };
                let bound = Session::CALL_TIMEOUT.min(self.watchdog / 2);
                let first = tokio::time::timeout(bound, upstream.events.recv()).await;
                super::events::stream_fault_point("after_upstream_connect");
                let opened = matches!(first, Ok(Some(frontcap_proxy::upstream::UpstreamEvent::Open)));
                if let Some((name, key, session)) = &record {
                    let event = if opened { "open" } else { "error" };
                    recorded_handshake = Some((name.clone(), frontcap_core::cassette::StreamEvent { key: key.clone(), session: *session, at_us: 0, event: event.into(), data: None, sse_event: None, sse_id: None, code: None }));
                }
                if opened {
                    events.push((t, StreamEventKind::Open, None, None, None, None));
                    self.record(t, EventKind::Taint(TaintRecord { source: TaintSource::PassthroughContent, detail: ordinal.clone() }));
                    // Slots count from here: what arrives is delivered at the next frame tick.
                    self.last_slot = self.last_slot.max(frame_slot(held_at, VirtualDuration(self.profile.frame_interval_us)));
                    self.pass.insert(ordinal.clone(), PassStream { upstream, next_seq: 1, issued: held_at, record });
                    ok
                } else {
                    let why = match first {
                        Ok(Some(frontcap_proxy::upstream::UpstreamEvent::Failed(e))) => e,
                        Ok(_) => "the server closed it".into(),
                        Err(_) => format!("the server did not answer within {} ms", bound.as_millis()),
                    };
                    let detail = format!("{ordinal}: the stream {url} could not be opened: {why}");
                    self.record(t, EventKind::Taint(TaintRecord { source: TaintSource::PassthroughContent, detail }));
                    events.push((t, StreamEventKind::Error, None, None, None, None));
                    0
                }
            }
            Action::Replay { cassette, after_last, .. } if self.net.stream_replays.contains_key(&ev.request_id.0) || self.net.cassettes.get(&rule.id).is_some_and(|c| !c.streams.is_empty()) => {
                // A recorded stream, played as its recording: each event at its recorded offset.
                let key = frontcap_core::cassette::Key::of("GET", &url, None);
                let replay = match self.net.stream_replays.get(&ev.request_id.0) {
                    Some(replay) => replay.clone(),
                    None => {
                        let session = self.net.replay_session(&rule.id, &key, *after_last).map(|(session, _)| session);
                        let replay = crate::bindings::StreamReplay {
                            target_id: self.target_id.clone(), fetch_id: ev.request_id.0.clone(),
                            rules_revision: revision, rule: rule.id.clone(), key: key.clone(),
                            session, base_us: held_at.0,
                        };
                        self.bindings.append(&crate::bindings::BindingEntry::StreamReplay(Box::new(replay.clone())))?;
                        self.net.stream_replays.insert(ev.request_id.0.clone(), replay.clone());
                        replay
                    }
                };
                super::events::stream_fault_point("after_replay_selection");
                handshake_at = VirtualTime(replay.base_us);
                match replay.session {
                    Some(session) => {
                        let text = std::fs::read_to_string(self.bundle.root().join(cassette))?;
                        let (_, recorded) = frontcap_core::cassette::parse_all(&text)
                            .map_err(|error| ChromiumError::Shape(error.to_string()))?;
                        let recorded: Vec<_> = recorded.into_iter().filter(|event| event.key == replay.key && event.session == session).collect();
                        if recorded.is_empty() { return Err(ChromiumError::Shape(format!("{ordinal}: selected replay session is missing"))); }
                        self.net.stream_entry = Some(session);
                        // The recorded handshake's outcome: a session that failed fails again.
                        let failed = recorded.first().is_some_and(|e| e.event == "error");
                        replayed = true;
                        for e in recorded {
                            let at = handshake_at.saturating_add(VirtualDuration(e.at_us));
                            let kind = match e.event.as_str() {
                                "open" => StreamEventKind::Open,
                                "close" => StreamEventKind::Close,
                                "error" => StreamEventKind::Error,
                                _ => StreamEventKind::Message,
                            };
                            events.push((at, kind, e.data.clone(), e.sse_event.clone(), e.sse_id.clone(), e.code));
                        }
                        if failed { 0 } else { ok }
                    }
                    None => {
                        let detail = format!("{ordinal}: the cassette of {} has no stream left for {url}", rule.id);
                        self.record(t, EventKind::Taint(TaintRecord { source: TaintSource::CassetteMiss, detail }));
                        events.push((t, StreamEventKind::Error, None, None, None, None));
                        0
                    }
                }
            }
            // Held for the agent: the wrapper stays connecting until the agent answers its handshake.
            Action::Hold => ok,
            other => {
                if !matches!(other, Action::Fail { .. }) {
                    let detail = match other {
                        Action::Passthrough | Action::Record { .. } => format!("{ordinal}: {} passes the stream {url} through, but this generation has no connection to servers", rule.id),
                        _ => format!("{ordinal}: {} cannot answer a stream; a stream_mock or hold rule can", rule.id),
                    };
                    self.record(t, EventKind::Taint(TaintRecord { source: TaintSource::UnsupportedStream, detail }));
                }
                events.push((t, StreamEventKind::Error, None, None, None, None));
                0
            }
        };
        // The request's record, ahead of what its decision recorded.
        let entry = self.net.stream_entry.take();
        let request = RequestRecord { rules_revision: revision, id: ordinal.clone(), method: "GET".into(), url: url.clone(), rule: Some(rule.id.clone()), entry, context: self.popup_id(), body: None };
        self.pending.insert(request_at, (t, PendingEvent::Recorded(Box::new(EventKind::Request(request.clone())))));
        if matches!(rule.action, Action::Hold) && !closed_early {
            self.bindings.append(&crate::bindings::BindingEntry::StreamHeld {
                target_id: self.target_id.clone(), ordinal: ordinal.clone(), owner: context.clone(), document: self.documents,
                page_stream, websocket, requested_us: t.0, request: Box::new(request),
            })?;
            self.net.streams.insert(ordinal.clone(), Stream { document: self.documents, page_stream, websocket, context: Some(context), keys: Vec::new(), window: self.popup_id() });
            self.net.held_streams.insert(ordinal, crate::network::HeldStream {
                target_id: self.target_id.clone(), url, rule: rule.id.clone(), rules_revision: revision, websocket, open: false, next_seq: 0,
            });
            self.page.call(&fetch::FailRequestParams::new(ev.request_id.clone(), cnet::ErrorReason::Aborted)).await?;
            self.receipts_ready.push(ev.request_id.0);
            return Ok(());
        }
        // Time order, script order within one instant; each event numbered in the stream.
        events.sort_by_key(|e| e.0);
        let source = if status == 0 { ResponseSource::Fail } else { ResponseSource::Mock };
        let response = ResponseRecord { id: ordinal.clone(), status, source, rule: Some(rule.id.clone()) };
        self.record(handshake_at, EventKind::Response(response.clone()));
        if closed_early {
            let handshake = crate::bindings::StreamHandshake {
                fetch_id: Some(ev.request_id.0.clone()), network_id: ev.network_id.as_ref().map(|id| id.0.clone()),
                requested_us: t.0, request, answered_us: handshake_at.0, response,
            };
            if let Some((closed_at, receipt)) = self.net.early_close_reports.remove(&(context.clone(), page_stream)) {
                self.record_early_stream_close(handshake, closed_at, receipt)?;
            } else {
                self.net.closed_handshakes.insert((context.clone(), page_stream), handshake);
            }
            self.fail_anchor(ev.request_id).await?;
            return Ok(());
        }
        if held_at > t {
            let detail = format!("{ordinal}: {url} opened {} µs after its handshake was issued", held_at.0 - t.0);
            self.record(held_at, EventKind::Taint(TaintRecord { source: TaintSource::LateDelivery, detail }));
        }
        if let Some((sequence, (at, _, _, _, _, _))) = events.iter().enumerate().find(|(_, event)| event.1 == StreamEventKind::Open) {
            self.bindings.append(&crate::bindings::BindingEntry::StreamOpening {
                target_id: self.target_id.clone(), ordinal: ordinal.clone(), owner: context.clone(),
                document: self.documents, page_stream, sequence: sequence as u64, at_us: at.0,
                requested_us: t.0, request: Box::new(request), answered_us: handshake_at.0, response,
            })?;
        }
        // The opening intent precedes cassette publication: recovery can finish a missing line,
        // and a published open can never be mistaken for an undecided connection attempt.
        if let Some((name, line)) = recorded_handshake {
            super::events::stream_fault_point("before_stream_cassette");
            self.write_stream_handshake(&name, &line)?;
            super::events::stream_fault_point("after_stream_cassette");
        }
        self.net.streams.insert(ordinal.clone(), Stream { document: self.documents, page_stream, websocket, context: Some(context), keys: Vec::new(), window: self.popup_id() });
        for (seq, (at, event, data, sse_event, sse_id, code)) in events.into_iter().enumerate() {
            let e = StreamDue { ordinal: ordinal.clone(), seq: seq as u64, event, data, sse_event, sse_id, code, from_server: replayed };
            if at <= held_at && !self.progressing {
                self.deliver_stream(&e, held_at).await?;
            } else {
                // During progress, native finite-response callbacks at this instant run before
                // stream events. Keep the event on the agenda until that delivery barrier ends.
                let key = self.net.agenda.schedule(at, Due::Stream(e));
                if let Some(s) = self.net.streams.get_mut(&ordinal) {
                    s.keys.push(key);
                }
            }
        }
        // The handshake request was only the anchor of the stream's instant; the wrapper ignores how it
        // ends, and a failure ends it without the completion step a response takes (S9).
        self.page.call(&fetch::FailRequestParams::new(ev.request_id.clone(), cnet::ErrorReason::Aborted)).await?;
        // Its outcome is the stream's records: the engine's receipt is not needed.
        self.receipts_ready.push(ev.request_id.0);
        Ok(())
    }

    /// A stream anchor whose load ended before an interception reached the platform, at its place in the
    /// issue order. Its frame went in the task that opened the stream (that task's events come before the
    /// frame tree's answer), so it is abandoned as if its interception had arrived: the trace does not
    /// depend on when, or whether, the browser reports it. A living frame's anchor is waited for no longer.
    pub(super) async fn abandon_unpaused_anchor(&mut self, id: &str, url: &str, t: VirtualTime, at: cdp::Arrival) -> Result<(), ChromiumError> {
        let frame = self.net.anchors.remove(id);
        let (Some(frame), Some((page_stream, _, url, _))) = (frame, stream_anchor(url)) else {
            self.net.interceptions.ended(id);
            return Ok(());
        };
        self.drain_bindings_before(at).await?;
        let frame = page::FrameId(frame);
        if frame_in(&self.page.call(&page::GetFrameTreeParams::default()).await?.frame_tree, &frame) {
            self.net.interceptions.ended(id);
            return Ok(());
        }
        self.net.interceptions.handled(id);
        self.net.abandoned_anchors.insert(id.to_string());
        self.abandon_stream(None, &frame, Some(id.to_string()), t, page_stream, url).await
    }

    /// Records an abandoned stream: its handshake failed at its issue, when its frame went.
    async fn abandon_stream(&mut self, fetch_id: Option<fetch::RequestId>, frame: &page::FrameId, network_id: Option<String>, t: VirtualTime, page_stream: u64, url: String) -> Result<(), ChromiumError> {
        let selected = fetch_id.as_ref().and_then(|id| self.net.stream_selections.get(&id.0));
        let (ordinal, revision, rule) = match selected {
            Some(selected) => (selected.ordinal.clone(), selected.rules_revision, selected.rule.clone()),
            None => {
                let ordinal = self.net.ordinal();
                let rule = self.net.rules.first_route_match("GET", &url).expect("effective default-block").id.clone();
                (ordinal, self.net.rules_revision, rule)
            }
        };
        let entry = fetch_id.as_ref().and_then(|id| self.net.stream_replays.get(&id.0).and_then(|replay| replay.session)
            .or_else(|| self.net.stream_connections.get(&id.0)
                .and_then(|connection| connection.record.as_ref().map(|(_, _, session)| *session))));
        let request = RequestRecord { rules_revision: revision, id: ordinal.clone(), method: "GET".into(), url,
            rule: Some(rule.clone()), entry, context: self.popup_id(), body: None };
        let response = ResponseRecord { id: ordinal, status: 0, source: ResponseSource::Fail, rule: Some(rule) };
        let handshake = crate::bindings::StreamHandshake { fetch_id: fetch_id.as_ref().map(|id| id.0.clone()), network_id,
            requested_us: t.0, request: request.clone(), answered_us: t.0, response: response.clone() };
        let mut closed = None;
        if self.patched {
            for receipt in self.page.call(&page::FrontcapGetStreamReportsParams::default()).await?.receipts {
                if receipt.frame_id != *frame { continue; }
                let value: serde_json::Value = serde_json::from_str(&receipt.payload)
                    .map_err(|error| ChromiumError::Shape(error.to_string()))?;
                if value["id"].as_u64() != Some(page_stream) || value["op"] != "close" { continue; }
                let epoch = receipt.unix_time_us.parse::<i128>().map_err(|_| ChromiumError::Shape("invalid stream receipt time".into()))?;
                let at = u64::try_from(epoch - i128::from(self.profile.initial_date_ms) * 1000)
                    .map_err(|_| ChromiumError::Shape("stream receipt outside generation timeline".into()))?;
                closed = Some((VirtualTime(at), StreamReceipt { id: receipt.receipt_id,
                    frame: receipt.frame_id.0, document: receipt.document_id }));
                break;
            }
        }
        // Its frame went at its issue: the anchor held the clock there until then.
        self.record(t, EventKind::Request(request));
        self.record(t, EventKind::Response(response));
        if let Some((at, receipt)) = closed {
            self.record_early_stream_close(handshake, at, Some(receipt))?;
        } else {
            self.bindings.append(&crate::bindings::BindingEntry::StreamAbandoned {
                target_id: self.target_id.clone(), handshake: Box::new(handshake),
            })?;
        }
        super::events::stream_fault_point("after_stream_abandoned");
        match fetch_id {
            Some(id) => self.fail_anchor(id).await,
            None => Ok(()),
        }
    }

    /// Delivers one stream event to the wrapper of the document that opened the stream, which
    /// dispatches it before the evaluation returns; recorded at `t` once delivered. A stream the page
    /// closed, or whose document is gone, takes nothing more — and its failure leaves other events due.
    /// Delivers one event of a stream to the page that has it; false when that page is gone.
    pub(super) async fn deliver_stream(&mut self, e: &StreamDue, t: VirtualTime) -> Result<bool, ChromiumError> {
        // A popup's stream is delivered in its window; one whose window closed has nowhere to go.
        let window = self.net.streams.get(&e.ordinal).and_then(|s| s.window.clone());
        let previous = self.popup_id();
        let Some(window) = window.filter(|w| previous.as_deref() != Some(w.as_str())) else {
            return self.deliver_stream_here(e, t).await;
        };
        match self.enter_context(&window) {
            Ok(()) => {}
            Err(ChromiumError::ContextClosed(_)) => {
                self.end_stream(&e.ordinal);
                return Ok(false);
            }
            Err(e) => return Err(e),
        }
        let delivered = self.deliver_stream_here(e, t).await;
        self.reenter(previous)?;
        delivered
    }

    async fn deliver_stream_here(&mut self, e: &StreamDue, t: VirtualTime) -> Result<bool, ChromiumError> {
        let Some(s) = self.net.streams.get(&e.ordinal).cloned() else { return Ok(false) };
        let kind = serde_json::to_value(e.event).expect("serializes");
        let event = serde_json::json!({ "type": kind, "data": e.data, "event": e.sse_event, "id": e.sse_id, "code": e.code });
        let (key, _) = instrument::stream_names(&self.launch_id);
        let expr = format!("(() => {{ const w = window[{}]; return !!w && w.deliver({}, {}, {}); }})()", serde_json::to_string(&key).unwrap(), s.page_stream, e.seq, event);
        if e.event == StreamEventKind::Open {
            super::events::stream_fault_point("before_open_delivery");
        }
        let delivered = match (&s.context, s.document == self.documents) {
            // The page's handlers run in the delivery and may wait on a synchronous request.
            (Some(ctx), true) => {
                let (page, ctx) = (self.page.clone(), ctx.clone());
                self.serviced(page.eval_unique(&expr, &ctx)).await.ok() == Some(serde_json::Value::Bool(true))
            }
            _ => false,
        };
        let receipt = if e.event == StreamEventKind::Open {
            self.opening_receipt(&e.ordinal, s.page_stream, e.seq).await?
        } else { None };
        // Removing the document inside onopen can destroy the evaluation context before CDP
        // returns its value. The browser receipt survives that callback and proves the opening.
        let delivered = delivered || receipt.is_some();
        if !delivered {
            if s.document != self.documents || s.context.as_ref().is_none_or(|owner|
                !self.stream_contexts.values().any(|live| live == owner)) {
                self.end_stream(&e.ordinal);
            }
            return Ok(false);
        }
        if e.event == StreamEventKind::Open {
            super::events::stream_fault_point("after_open_delivery");
        }
        if let Some(receipt) = &receipt { self.seen_stream_receipts.insert(receipt.id.clone()); }
        self.record(t, EventKind::Stream(StreamRecord { context: self.popup_id(),
            stream: e.ordinal.clone(),
            direction: StreamDirection::In,
            event: e.event,
            data: e.data.clone(),
            page_stream: (e.event == StreamEventKind::Open).then_some(s.page_stream),
            document: (e.event == StreamEventKind::Open).then_some(s.document),
            receipt,
        }));
        if let Some(q) = self.quiet.as_mut() {
            q.activity(t);
        }
        self.take_contexts()?;
        // A callback may remove its owner before the inspector evaluation can return.
        let owner_gone = s.context.as_ref().is_some_and(|owner|
            !self.stream_contexts.values().any(|live| live == owner));
        // An error ends a stream; so does a close (an EventSource then reconnects as a new stream).
        if owner_gone || matches!(e.event, StreamEventKind::Close | StreamEventKind::Error) {
            self.end_stream(&e.ordinal);
        }
        Ok(true)
    }

    /// The streams of the current document a relaunched recorder takes over, from the trace and the page:
    /// each opened `stream_mock` stream its wrapper still has, with the events after the last one the
    /// wrapper received (those due while no recorder ran are delivered at once, late; the fault is
    /// recorded).
    pub(super) async fn rederive_streams(&mut self, events: &[BehaviorEvent]) -> Result<(), ChromiumError> {
        let mut rules = BTreeMap::new();
        let mut opened = Vec::new();
        let mut ended = std::collections::BTreeSet::new();
        let mut answered = BTreeMap::new();
        for e in events {
            match &e.kind {
                EventKind::Request(r) => {
                    rules.insert(r.id.clone(), (r.rules_revision, r.rule.clone(), e.t_us, r.url.clone(), r.entry));
                }
                EventKind::Response(r) => {
                    answered.insert(r.id.clone(), e.t_us);
                }
                EventKind::Stream(s) if s.direction == StreamDirection::In && s.event == StreamEventKind::Open => {
                    // This page's: the launch page's, or the active popup's.
                    if let (Some(page), Some(doc), true) = (s.page_stream, s.document, s.context == self.popup_id()) {
                        opened.push((s.stream.clone(), page, doc));
                    }
                }
                EventKind::Stream(s) if matches!(s.event, StreamEventKind::Close | StreamEventKind::Error) => {
                    ended.insert(s.stream.clone());
                }
                _ => {}
            }
        }
        let (key, _) = instrument::stream_names(&self.launch_id);
        for (ordinal, page_stream, document) in opened {
            if document != self.documents || ended.contains(&ordinal) {
                continue;
            }
            let Some((revision, Some(rule_id), issued, url, entry)) = rules.get(&ordinal).cloned() else { continue };
            let Some(rule) = self.net.rules_at(revision).and_then(|rules| rules.rules.iter().find(|r| r.id == rule_id)).cloned() else { continue };
            let expr = format!("(() => {{ const w = window[{}]; return w && w.owned ? w.owned({}) : null; }})()",
                serde_json::to_string(&key).unwrap(), serde_json::to_string(&ordinal).unwrap());
            let mut owner = None;
            for context in self.stream_contexts.values() {
                if let Ok(value) = self.page.eval_unique(&expr, context).await
                    && value["page_stream"].as_u64() == Some(page_stream)
                    && let Some(last) = value["last"].as_i64() {
                    if owner.is_some() { return Err(ChromiumError::Shape(format!("{ordinal}: multiple stream owners"))); }
                    owner = Some((context.clone(), last));
                }
            }
            let Some((context, last)) = owner else { continue };
            if matches!(rule.action, Action::Passthrough | Action::Record { .. }) {
                // The platform's connection to the server died with the recorder: the page learns the
                // connection was lost (a WebSocket closes 1006; an EventSource reconnects).
                let websocket = url.starts_with("ws");
                self.net.streams.insert(ordinal.clone(), Stream { document, page_stream, websocket, context: Some(context), keys: Vec::new(), window: self.popup_id() });
                let lost = StreamDue { ordinal: ordinal.clone(), seq: u64::MAX / 2, event: StreamEventKind::Close, data: None, sse_event: None, sse_id: None, code: Some(1006), from_server: true };
                // A recording keeps the loss, so its replay closes the stream at the same offset.
                if let (Action::Record { cassette }, Some(session), Some(open)) = (&rule.action, entry, answered.get(&ordinal)) {
                    let line = frontcap_core::cassette::StreamEvent { key: frontcap_core::cassette::Key::of("GET", &url, None), session, at_us: self.now.0.saturating_sub(open.0), event: "close".into(), data: None, sse_event: None, sse_id: None, code: Some(1006) };
                    self.write_stream_line(&frontcap_core::cassette::recording_name(revision, cassette), &line);
                }
                let k = self.net.agenda.schedule(self.now, Due::Stream(lost));
                if let Some(s) = self.net.streams.get_mut(&ordinal) {
                    s.keys.push(k);
                }
                continue;
            }

            // What the page's wrapper last received: nothing is delivered twice.
            let mut script: Vec<ScriptEvent> = Vec::new();
            let replayed = match &rule.action {
                Action::StreamMock { messages, close_at_ms, .. } => {
                    script.push((issued, StreamEventKind::Open, None, None, None, None));
                    for m in messages {
                        script.push((issued.saturating_add(VirtualDuration::from_ms(m.at_ms)), StreamEventKind::Message, Some(m.data.clone()), m.event.clone(), m.id.clone(), None));
                    }
                    if let Some(c) = close_at_ms {
                        script.push((issued.saturating_add(VirtualDuration::from_ms(*c)), StreamEventKind::Close, None, None, None, Some(1000)));
                    }
                    false
                }
                Action::Replay { cassette, .. } => {
                    let session = entry.ok_or_else(|| ChromiumError::Shape(format!("{ordinal}: replay stream has no selected session")))?;
                    let base = answered.get(&ordinal).ok_or_else(|| ChromiumError::Shape(format!("{ordinal}: replay stream has no handshake response")))?;
                    // Read the immutable cassette belonging to the selected installation, not the
                    // active installation's consumption state. Restoration must consume no session.
                    let text = std::fs::read_to_string(self.bundle.root().join(cassette))?;
                    let (_, streams) = frontcap_core::cassette::parse_all(&text).map_err(|e| ChromiumError::Shape(e.to_string()))?;
                    let request = frontcap_core::cassette::Key::of("GET", &url, None);
                    for event in streams.iter().filter(|e| e.key == request && e.session == session) {
                        let kind = match event.event.as_str() {
                            "open" => StreamEventKind::Open, "close" => StreamEventKind::Close,
                            "error" => StreamEventKind::Error, _ => StreamEventKind::Message,
                        };
                        script.push((base.saturating_add(VirtualDuration(event.at_us)), kind,
                            event.data.clone(), event.sse_event.clone(), event.sse_id.clone(), event.code));
                    }
                    if script.is_empty() { return Err(ChromiumError::Shape(format!("{ordinal}: selected replay stream session is missing"))); }
                    true
                }
                _ => continue,
            };
            script.sort_by_key(|e| e.0);
            let websocket = url.starts_with("ws");
            self.net.streams.insert(ordinal.clone(), Stream { document, page_stream, websocket, context: Some(context), keys: Vec::new(), window: self.popup_id() });
            for (seq, (at, event, data, sse_event, sse_id, code)) in script.into_iter().enumerate() {
                if seq as i64 <= last {
                    continue;
                }
                let e = StreamDue { ordinal: ordinal.clone(), seq: seq as u64, event, data, sse_event, sse_id, code, from_server: replayed };
                let k = self.net.agenda.schedule(at.max(self.now), Due::Stream(e));
                if let Some(s) = self.net.streams.get_mut(&ordinal) {
                    s.keys.push(k);
                }
            }
        }
        self.rederive_held_streams(events, &key, &ended).await
    }

    /// The held streams of the current document, from the trace (the agent's answer) and the binding
    /// journal (what it sent). What the page's wrapper did not receive is delivered now: the dead
    /// recorder had decided it at this instant.
    async fn rederive_held_streams(&mut self, events: &[BehaviorEvent], key: &str, ended: &std::collections::BTreeSet<String>) -> Result<(), ChromiumError> {
        use crate::bindings::BindingEntry;
        let journal = crate::bindings::BindingJournal::read(self.bindings.path())?;
        for entry in &journal {
            let BindingEntry::StreamHeld { target_id, ordinal, document, page_stream, websocket, request, .. } = entry else { continue };
            if *target_id != self.target_id || *document != self.documents || ended.contains(ordinal) || self.net.streams.contains_key(ordinal) {
                continue;
            }
            let expr = format!("(() => {{ const w = window[{}]; return w && w.owned ? w.owned({}) : null; }})()",
                serde_json::to_string(key).unwrap(), serde_json::to_string(ordinal).unwrap());
            let mut owner = None;
            for context in self.stream_contexts.values() {
                if let Ok(value) = self.page.eval_unique(&expr, context).await
                    && value["page_stream"].as_u64() == Some(*page_stream)
                    && let Some(last) = value["last"].as_i64() {
                    owner = Some((context.clone(), last));
                }
            }
            let Some((context, last)) = owner else { continue };
            let ok = if *websocket { 101 } else { 200 };
            let answered = events.iter().rev().find_map(|e| match &e.kind {
                EventKind::Response(r) if r.id == *ordinal => Some(r.status),
                _ => None,
            });
            let due = |seq, event| StreamDue { ordinal: ordinal.clone(), seq, event, data: None, sse_event: None, sse_id: None, code: None, from_server: false };
            let mut script = Vec::new();
            match answered {
                Some(status) if status == ok => {
                    script.push(due(0, StreamEventKind::Open));
                    let mut sent: Vec<StreamDue> = journal.iter().filter_map(|b| match b {
                        BindingEntry::StreamSent { ordinal: o, seq, event, data, sse_event, sse_id, code, .. } if o == ordinal => Some(StreamDue {
                            ordinal: ordinal.clone(), seq: *seq, event: *event, data: data.clone(), sse_event: sse_event.clone(),
                            sse_id: sse_id.clone(), code: *code, from_server: false,
                        }),
                        _ => None,
                    }).collect();
                    sent.sort_by_key(|d| d.seq);
                    script.extend(sent);
                }
                Some(_) => script.push(due(0, StreamEventKind::Error)),
                None => {}
            }
            let Some(rule) = request.rule.clone() else { continue };
            self.net.streams.insert(ordinal.clone(), Stream { document: *document, page_stream: *page_stream, websocket: *websocket, context: Some(context), keys: Vec::new(), window: self.popup_id() });
            self.net.held_streams.insert(ordinal.clone(), crate::network::HeldStream {
                target_id: target_id.clone(), url: request.url.clone(), rule, rules_revision: request.rules_revision, websocket: *websocket,
                open: answered == Some(ok), next_seq: script.last().map_or(0, |d| d.seq + 1),
            });
            for e in script {
                if e.seq as i64 > last {
                    self.deliver_stream(&e, self.now).await?;
                }
            }
        }
        Ok(())
    }

    /// At a slot (a frame tick, inside an advancing operation), delivers what each passed-through stream's
    /// server sent since the last one, in arrival order (time-model.md, Streams). Never while frozen: only
    /// the run loop calls it, and the run stops at every frame tick while such a stream is open.
    pub(super) async fn deliver_upstream(&mut self) -> Result<(), ChromiumError> {
        let slot = frame_slot(self.now, VirtualDuration(self.profile.frame_interval_us));
        if self.pass.is_empty() || slot <= self.last_slot {
            return Ok(());
        }
        self.last_slot = slot;
        let ordinals: Vec<String> = self.pass.keys().cloned().collect();
        for ordinal in ordinals {
            let mut arrived = Vec::new();
            if let Some(p) = self.pass.get_mut(&ordinal) {
                while let Ok(e) = p.upstream.events.try_recv() {
                    arrived.push(e);
                }
            }
            for e in arrived {
                use frontcap_proxy::upstream::UpstreamEvent as U;
                let Some(p) = self.pass.get_mut(&ordinal) else { break };
                let seq = p.next_seq;
                p.next_seq += 1;
                let (issued, record) = (p.issued, p.record.clone());
                let (event, data, sse_event, sse_id, code) = match e {
                    U::Message { data, event, id } => (StreamEventKind::Message, Some(data), event, id, None),
                    U::Closed { code } => (StreamEventKind::Close, None, None, None, Some(code)),
                    // The connection broke after the handshake: lost, as a browser reports it (a WebSocket
                    // closes 1006; an EventSource reconnects).
                    U::Failed(_) => (StreamEventKind::Close, None, None, None, Some(1006)),
                    U::Open => continue,
                };
                if let Some((name, key, session)) = &record {
                    let kind = match event {
                        StreamEventKind::Message => "message",
                        StreamEventKind::Close => "close",
                        StreamEventKind::Error => "error",
                        StreamEventKind::Open => "open",
                    };
                    let line = frontcap_core::cassette::StreamEvent { key: key.clone(), session: *session, at_us: self.now.0 - issued.0.min(self.now.0), event: kind.into(), data: data.clone(), sse_event: sse_event.clone(), sse_id: sse_id.clone(), code };
                    self.write_stream_line(name, &line);
                }
                let due = StreamDue { ordinal: ordinal.clone(), seq, event, data, sse_event, sse_id, code, from_server: true };
                self.deliver_stream(&due, self.now).await?;
            }
        }
        self.commit()
    }

    fn write_stream_handshake(&self, name: &str, line: &frontcap_core::cassette::StreamEvent) -> Result<(), ChromiumError> {
        persist_stream_handshake(&self.bundle.root().join("cassettes").join(format!("{name}.jsonl")), line)
    }

    /// An opening intent is durable before its cassette line. Complete publication without
    /// replaying the external connection; only handshake outcomes are idempotent this way.
    pub(super) fn recover_stream_handshakes(&self, bindings: &[crate::bindings::BindingEntry]) -> Result<(), ChromiumError> {
        for binding in bindings {
            let crate::bindings::BindingEntry::StreamOpening { target_id, ordinal, .. } = binding else { continue };
            if target_id != &self.target_id { continue; }
            let Some(selected) = self.net.stream_selections.values().find(|selected| &selected.ordinal == ordinal) else { continue };
            let Some(connection) = self.net.stream_connections.get(&selected.fetch_id) else { continue };
            let Some((name, key, session)) = &connection.record else { continue };
            self.write_stream_handshake(name, &frontcap_core::cassette::StreamEvent {
                key: key.clone(), session: *session, at_us: 0, event: "open".into(),
                data: None, sse_event: None, sse_id: None, code: None,
            })?;
            super::events::stream_fault_point("after_stream_cassette");
        }
        Ok(())
    }

    /// Appends one stream event to a `record` rule's cassette (synced).
    pub(super) fn write_stream_line(&mut self, name: &str, line: &frontcap_core::cassette::StreamEvent) {
        let dir = self.bundle.root().join("cassettes");
        let path = dir.join(format!("{name}.jsonl"));
        let written = std::fs::create_dir_all(path.parent().expect("cassette parent")).and_then(|_| {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
            f.write_all((line.to_line() + "\n").as_bytes())?;
            f.sync_data()
        });
        if let Err(err) = written {
            let detail = format!("the cassette {name} could not be written: {err}");
            self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::CassetteMiss, detail }));
        }
    }

    /// Ends a stream's scripted events: the page closed it, or its document is gone.
    /// The streams a `hold` rule keeps for the agent.
    pub fn held_streams(&self) -> Vec<(String, &crate::network::HeldStream)> {
        self.net.held_streams.iter().filter(|(o, _)| self.net.streams.contains_key(*o)).map(|(o, h)| (o.clone(), h)).collect()
    }

    /// Answers a held stream's handshake at the current instant: a success status (200, or 101 for a
    /// WebSocket) opens it, another refuses it as its server would, and none fails it.
    pub(super) async fn answer_held_stream(&mut self, ordinal: &str, status: Option<u16>) -> Result<(), ChromiumError> {
        let Some(held) = self.net.held_streams.get(ordinal).cloned() else {
            return Err(ChromiumError::InvalidArgument(format!("{ordinal} is not held")));
        };
        if held.open {
            return Err(ChromiumError::InvalidArgument(format!("{ordinal} is open: send its messages or close it")));
        }
        let ok = if held.websocket { 101 } else { 200 };
        let opens = status.is_some_and(|s| s == ok || s == 200);
        let source = if status.is_some() { ResponseSource::Agent } else { ResponseSource::Fail };
        let recorded = if opens { ok } else { status.unwrap_or(0) };
        self.record(self.now, EventKind::Response(ResponseRecord { id: ordinal.into(), status: recorded, source, rule: Some(held.rule.clone()) }));
        self.commit()?;
        let event = if opens { StreamEventKind::Open } else { StreamEventKind::Error };
        let due = StreamDue { ordinal: ordinal.into(), seq: 0, event, data: None, sse_event: None, sse_id: None, code: None, from_server: false };
        let delivered = self.deliver_stream(&due, self.now).await?;
        match self.net.held_streams.get_mut(ordinal) {
            Some(h) if opens && delivered => {
                h.open = true;
                h.next_seq = 1;
            }
            _ => self.end_stream(ordinal),
        }
        self.drain().await?;
        self.commit()?;
        if !delivered {
            return Err(ChromiumError::NotActionable(format!("{ordinal} ended: the page that opened it is gone")));
        }
        Ok(())
    }

    /// Delivers, at the current instant, what the agent sends on a held stream it opened: a message, or
    /// the server's close (`code` for a WebSocket, 1000 when none).
    #[allow(clippy::too_many_arguments)]
    pub async fn stream_send(&mut self, request: &RequestId, ordinal: &str, close: bool, data: Option<String>, sse_event: Option<String>, sse_id: Option<String>, code: Option<u16>) -> Result<(), ChromiumError> {
        self.alive()?;
        let Some(held) = self.net.held_streams.get(ordinal).filter(|_| self.net.streams.contains_key(ordinal)).cloned() else {
            return Err(ChromiumError::InvalidArgument(format!("{ordinal} is not a stream held for you; list_held lists them")));
        };
        if !held.open {
            return Err(ChromiumError::InvalidArgument(format!("{ordinal} is connecting: respond with status 200 to open it first")));
        }
        let (event, code) = if close { (StreamEventKind::Close, code.or(held.websocket.then_some(1000))) } else { (StreamEventKind::Message, None) };
        let seq = held.next_seq;
        self.bindings.append(&crate::bindings::BindingEntry::StreamSent {
            target_id: held.target_id.clone(), ordinal: ordinal.into(), seq, event, data: data.clone(),
            sse_event: sse_event.clone(), sse_id: sse_id.clone(), code, request: request.as_str().into(),
        })?;
        super::events::stream_fault_point("after_stream_sent");
        let due = StreamDue { ordinal: ordinal.into(), seq, event, data, sse_event, sse_id, code, from_server: false };
        let delivered = self.deliver_stream(&due, self.now).await?;
        if let Some(h) = self.net.held_streams.get_mut(ordinal) {
            h.next_seq = seq + 1;
        }
        self.drain().await?;
        self.commit()?;
        if !delivered {
            self.end_stream(ordinal);
            return Err(ChromiumError::NotActionable(format!("{ordinal} ended: the page that opened it is gone")));
        }
        Ok(())
    }

    /// Whether the agent's `request` sent an event on a held stream: a relaunched recorder delivers it.
    pub fn stream_sent_by(&self, request: &RequestId) -> Result<bool, ChromiumError> {
        Ok(crate::bindings::BindingJournal::read(self.bindings.path())?.iter()
            .any(|b| matches!(b, crate::bindings::BindingEntry::StreamSent { request: r, .. } if r == request.as_str())))
    }

    pub(super) fn end_stream(&mut self, ordinal: &str) {
        // A passed-through stream's connection to its server ends with it; a held one's hold too.
        self.pass.remove(ordinal);
        self.net.held_streams.remove(ordinal);
        if let Some(s) = self.net.streams.remove(ordinal) {
            for k in s.keys {
                self.net.agenda.cancel(k);
            }
        }
    }
}


impl Generation {
    pub(super) async fn forget_stream_receipts(&mut self) -> Result<(), ChromiumError> {
        if !self.patched || self.stream_receipts_ready.is_empty() { return Ok(()); }
        self.log.sync()?;
        #[cfg(feature = "fault-injection")]
        let fault_after_forget = if std::env::var("FRONTCAP_STREAM_FAULT").is_ok_and(|point| point == "after_forget") {
            self.page.call(&page::FrontcapGetStreamReportsParams::default()).await?.receipts.iter().any(|receipt|
                self.stream_receipts_ready.contains(&receipt.receipt_id)
                && serde_json::from_str::<serde_json::Value>(&receipt.payload).is_ok_and(|value|
                    matches!(value["op"].as_str(), Some("send" | "close"))))
        } else { false };
        self.page.call(&page::FrontcapForgetStreamReportsParams {
            receipt_ids: self.stream_receipts_ready.iter().cloned().collect(),
        }).await?;
        self.stream_receipts_ready.clear();
        #[cfg(feature = "fault-injection")]
        if fault_after_forget { super::events::stream_fault_point("after_forget"); }
        Ok(())
    }

    /// Restore page calls, never external sends: the old recorder owned the dead upstream sockets.
    /// Admission without a durable request/open binding is handled separately and remains retained.
    pub(super) async fn recover_stream_reports(&mut self, events: &[BehaviorEvent]) -> Result<(), ChromiumError> {
        if !self.patched { return Ok(()); }
        let inventory = self.page.call(&page::FrontcapGetStreamReportsParams::default()).await?;
        for receipt in inventory.receipts {
            let report: serde_json::Value = serde_json::from_str(&receipt.payload)
                .map_err(|error| ChromiumError::Shape(error.to_string()))?;
            if report["op"] == "received_open" { continue; }
            let Some(ordinal) = report["ordinal"].as_str() else { continue };
            let request = events.iter().find_map(|event| match &event.kind {
                EventKind::Request(request) if request.id == ordinal => Some(request),
                _ => None,
            });
            let Some(request) = request else { continue };
            let opened = events.iter().any(|event| matches!(&event.kind,
                EventKind::Stream(stream) if stream.stream == ordinal && stream.event == StreamEventKind::Open));
            if !opened { continue; }
            let close = match report["op"].as_str() {
                Some("close") => true, Some("send") => false,
                _ => return Err(ChromiumError::Shape("invalid retained stream operation".into())),
            };
            let epoch = receipt.unix_time_us.parse::<i128>()
                .map_err(|_| ChromiumError::Shape("invalid retained stream time".into()))?;
            let t = VirtualTime(u64::try_from(epoch - i128::from(self.profile.initial_date_ms) * 1000)
                .map_err(|_| ChromiumError::Shape("stream report outside generation timeline".into()))?);
            let outgoing_recorded = events.iter().any(|event| matches!(&event.kind,
                EventKind::Stream(stream) if stream.direction == StreamDirection::Out
                    && stream.receipt.as_ref().is_some_and(|saved| saved.id == receipt.receipt_id)));
            let needs_close = close && request.url.starts_with("ws");
            let close_recorded = events.iter().any(|event| matches!(&event.kind,
                EventKind::Stream(stream) if stream.direction == StreamDirection::In
                    && stream.event == StreamEventKind::Close && stream.stream == ordinal && event.t_us == t));
            if outgoing_recorded && (!needs_close || close_recorded) { continue; }
            // An outgoing close may be durable while its matching incoming close is not. Keep the
            // native receipt until the missing half has also been appended and synced by commit.
            self.stream_receipts_ready.remove(&receipt.receipt_id);
            super::events::stream_fault_point("before_trace");
            self.seen_stream_receipts.insert(receipt.receipt_id.clone());
            let metadata = StreamReceipt { id: receipt.receipt_id, frame: receipt.frame_id.0, document: receipt.document_id };
            if !outgoing_recorded {
                self.record(t, EventKind::Stream(StreamRecord { context: self.popup_id(),
                    stream: ordinal.to_string(), direction: StreamDirection::Out,
                    event: if close { StreamEventKind::Close } else { StreamEventKind::Message },
                    data: if close { None } else { report["data"].as_str().map(str::to_string) },
                    page_stream: None, document: None, receipt: Some(metadata.clone()),
                }));
            }
            if needs_close && !close_recorded {
                self.record(t, EventKind::Stream(StreamRecord { context: self.popup_id(),
                    stream: ordinal.to_string(), direction: StreamDirection::In, event: StreamEventKind::Close,
                    data: None, page_stream: None, document: None, receipt: Some(metadata),
                }));
            }
        }
        Ok(())
    }
}

impl Generation {
    pub(super) async fn recover_stream_openings(
        &mut self, bindings: &[crate::bindings::BindingEntry], events: &[BehaviorEvent],
    ) -> Result<Vec<(VirtualTime,StreamRecord)>, ChromiumError> {
        let (key, _) = instrument::stream_names(&self.launch_id);
        let mut recovered = Vec::new();
        let mut opened: std::collections::BTreeSet<String> = events.iter().filter_map(|event| match &event.kind {
            EventKind::Stream(stream) if stream.event == StreamEventKind::Open => Some(stream.stream.clone()),
            _ => None,
        }).collect();
        for binding in bindings {
            let crate::bindings::BindingEntry::StreamOpening {
                target_id, ordinal, owner, document, page_stream, sequence, at_us, ..
            } = binding else { continue };
            if target_id != &self.target_id || *document != self.documents || opened.contains(ordinal) { continue; }
            if !events.iter().any(|event| matches!(&event.kind, EventKind::Request(request) if &request.id == ordinal)) {
                continue; // Earlier admission phases require their own request binding.
            }
            let mut receipt = self.opening_receipt(ordinal, *page_stream, *sequence).await?;
            let mut delivered_at = VirtualTime(*at_us);
            if receipt.is_none() {
                let expression = format!("(() => {{ const w=window[{}]; return w && w.owned ? w.owned({}) : null; }})()",
                    serde_json::to_string(&key).unwrap(),serde_json::to_string(ordinal).unwrap());
                let Ok(value) = self.page.eval_unique(&expression, owner).await else { continue };
                if value["page_stream"].as_u64() != Some(*page_stream) { continue; }
                let Some(last) = value["last"].as_i64() else { continue };
                if last < 0 || (last as u64) < *sequence {
                    if self.now.0 < *at_us { continue; }
                    let deliver = format!("window[{}].deliver({}, {}, {{type:'open'}})",
                        serde_json::to_string(&key).unwrap(),page_stream,sequence);
                    let delivered = self.page.eval_unique(&deliver, owner).await;
                    // The callback may remove its own frame before evaluation returns. As on
                    // live delivery, the browser-owned receipt proves execution across that loss.
                    receipt = self.opening_receipt(ordinal, *page_stream, *sequence).await?;
                    if receipt.is_none() && delivered? != serde_json::Value::Bool(true) {
                        return Err(ChromiumError::Shape("retained stream opening was not delivered".into()));
                    }
                    delivered_at = self.now;
                    self.take_contexts()?;
                }
            }
            opened.insert(ordinal.clone());
            recovered.push((delivered_at,StreamRecord { context: self.popup_id(),
                stream: ordinal.clone(), direction: StreamDirection::In, event: StreamEventKind::Open,
                data: None, page_stream: Some(*page_stream), document: Some(*document), receipt,
            }));
        }
        recovered.sort_by_key(|(at,_)|*at);
        Ok(recovered)
    }
}


impl Generation {
    async fn opening_receipt(&self, ordinal: &str, page_stream: u64, sequence: u64) -> Result<Option<StreamReceipt>, ChromiumError> {
        if !self.patched { return Ok(None); }
        let inventory = self.page.call(&page::FrontcapGetStreamReportsParams::default()).await?;
        for receipt in inventory.receipts {
            let value: serde_json::Value = serde_json::from_str(&receipt.payload)
                .map_err(|error| ChromiumError::Shape(error.to_string()))?;
            if value["op"] == "received_open" && value["ordinal"] == ordinal
                && value["id"].as_u64() == Some(page_stream) && value["sequence"].as_u64() == Some(sequence) {
                return Ok(Some(StreamReceipt { id: receipt.receipt_id, frame: receipt.frame_id.0, document: receipt.document_id }));
            }
        }
        Ok(None)
    }
}


impl Generation {
    pub(super) fn record_early_stream_close(&mut self, handshake: crate::bindings::StreamHandshake,
        at: VirtualTime, receipt: Option<StreamReceipt>) -> Result<(), ChromiumError> {
        let closure = crate::bindings::StreamClosure {
            target_id: self.target_id.clone(), handshake, closed_us: at.0, receipt,
        };
        self.bindings.append(&crate::bindings::BindingEntry::StreamClosed(Box::new(closure.clone())))?;
        if closure.receipt.is_some() { super::events::stream_fault_point("before_trace"); }
        self.restore_early_stream_close(&closure, &[]);
        Ok(())
    }

    pub(super) fn restore_early_stream_close(&mut self, closure: &crate::bindings::StreamClosure, events: &[BehaviorEvent]) {
        let ordinal = &closure.handshake.request.id;
        let t = VirtualTime(closure.closed_us);
        let present = |direction| events.iter().any(|event| matches!(&event.kind,
            EventKind::Stream(stream) if stream.stream == *ordinal && stream.direction == direction
                && stream.event == StreamEventKind::Close && event.t_us == t));
        let outgoing = present(StreamDirection::Out);
        let websocket = closure.handshake.request.url.starts_with("ws");
        let incoming = present(StreamDirection::In);
        if outgoing && (!websocket || incoming) { return; }
        if let Some(receipt) = &closure.receipt { self.stream_receipts_ready.remove(&receipt.id); }
        if !outgoing {
            self.record(t, EventKind::Stream(StreamRecord { context: self.popup_id(),
                stream: ordinal.clone(), direction: StreamDirection::Out, event: StreamEventKind::Close,
                data: None, page_stream: None, document: None, receipt: closure.receipt.clone(),
            }));
        }
        if websocket && !incoming {
            self.record(t, EventKind::Stream(StreamRecord { context: self.popup_id(),
                stream: ordinal.clone(), direction: StreamDirection::In, event: StreamEventKind::Close,
                data: None, page_stream: None, document: None, receipt: closure.receipt.clone(),
            }));
        }
        if let Some(quiet) = self.quiet.as_mut() { quiet.activity(t); }
    }
}


/// A session has one handshake outcome. Do not use this for messages, which may repeat exactly.
fn persist_stream_handshake(path: &std::path::Path, line: &frontcap_core::cassette::StreamEvent) -> Result<(), ChromiumError> {
    use std::io::Write;
    std::fs::create_dir_all(path.parent().expect("cassette parent"))?;
    let mut opened = frontcap_core::lines::open_append(path)
        .map_err(|error| ChromiumError::Shape(error.to_string()))?;
    let (_, streams) = frontcap_core::cassette::parse_all(&(opened.lines.join("\n") + "\n"))
        .map_err(|error| ChromiumError::Shape(error.to_string()))?;
    if let Some(existing) = streams.iter().find(|event| event.key == line.key && event.session == line.session
        && matches!(event.event.as_str(), "open" | "error")) {
        if existing == line { return Ok(()); }
        return Err(ChromiumError::Shape("conflicting recorded stream handshake".into()));
    }
    opened.file.write_all((line.to_line() + "\n").as_bytes())?;
    opened.file.sync_data()?;
    Ok(())
}


#[cfg(test)]
#[path = "streams_tests.rs"]
mod handshake_tests;
