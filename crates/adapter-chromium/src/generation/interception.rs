use super::*;
use super::worker_lineage::Issuer;
use crate::bindings::{Binding, BindingEntry, Decision, Resolution};

/// The absolute instant a mocked answer is due: its request's issue plus its latency.
/// A pause at the response stage (only recorded requests adopted on reconnect are continued so).
pub(super) fn is_response_stage(ev: &fetch::RequestPausedEvent) -> bool {
    ev.response_status_code.is_some() || ev.response_error_reason.is_some()
}

fn due_of(t: VirtualTime, spec: &MockSpec) -> u64 {
    t.saturating_add(VirtualDuration::from_ms(spec.latency_ms)).0
}

/// Browser-side rule selection shared by live admission and pre-renderer recovery. No CDP
/// command or renderer observation belongs here: callers persist the decision before delivery.
pub(super) struct SelectedRequest {
    pub rule: RuleId,
    pub entry: Option<u64>,
    pub decision: Decision,
    pub taint: Option<TaintRecord>,
}

pub(super) fn select_request_decision(net: &mut Network, ev: &fetch::RequestPausedEvent,
    t: VirtualTime, preflight: bool, progressing: bool, ordinal: &str) -> Result<SelectedRequest, ChromiumError>
{
    let is_document = ev.resource_type == cnet::ResourceType::Document;
    let headers = super::adoption::paused_headers(ev);
    let header = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
    let body = request_body(&ev.request);
    let rule = request_rule(&net.rules, ev, preflight);
    let key = body.clone().map(|b| frontcap_core::cassette::Key::of(&ev.request.method, &ev.request.url, b.as_deref()));
    // A replay chooses its cassette line here, and the request's record names it: what was consumed
    // survives a fault even if the answer never comes.
    let pick = match (&rule.action, &key) {
        (Action::Replay { latency_ms, after_last, .. }, Some(key)) => {
            // A preflight the recording did not see is answered as the platform answers others.
            let after_last = if preflight { frontcap_core::rules::AfterLast::Fail } else { *after_last };
            net.replay(&rule.id, key, after_last, *latency_ms)
        }
        _ => None,
    };
    let entry = pick.as_ref().map(|(i, _, _)| *i as u64);
    let mut taint = None;
    let decision = 'select: {
        if let Action::Record { cassette } = &rule.action {
            match key {
                Some(key) => {
                    net.recording.insert(ordinal.to_owned(), (frontcap_core::cassette::recording_name(net.rules_revision, cassette), key, t, !progressing && !is_document));
                }
                None => {
                    let detail = format!("{ordinal}: the request body is not available to the platform, so it is not recorded in {cassette}");
                    taint = Some(TaintRecord { source: TaintSource::CassetteMiss, detail });
                }
            }
            break 'select Decision::Passthrough;
        }
        if let Some((_, spec, requires_progress)) = pick {
            let requires_progress = requires_progress && !is_document;
            // A cassette's latency is the recorded one, except for a document.
            let spec = if is_document { MockSpec { latency_ms: 0, ..spec } } else { spec };
            break 'select Decision::Deliver { due_us: due_of(t, &spec), spec, source: ResponseSource::Replay, requires_progress };
        }
        if preflight {
            break 'select match &rule.action {
                Action::Passthrough => Decision::Passthrough,
                Action::Fail { reason } if rule.id.as_str() == "default-block" => Decision::Fail { reason: *reason },
                _ => {
                    let method = header("access-control-request-method").unwrap_or_else(|| "GET".into());
                    let mut headers = BTreeMap::new();
                    headers.insert("access-control-allow-methods".to_string(), method);
                    if let Some(h) = header("access-control-request-headers") {
                        headers.insert("access-control-allow-headers".to_string(), h);
                    }
                    // The browser must not cache preflights on the host clock.
                    headers.insert("access-control-max-age".to_string(), "0".to_string());
                    let spec = MockSpec { status: 204, headers, body: String::new(), body_base64: None, latency_ms: 0 };
                    Decision::Deliver { spec, source: ResponseSource::Preflight, due_us: t.0, requires_progress: false }
                }
            };
        }
        let spec = match &rule.action {
            Action::Mock(m) => m.clone(),
            Action::Replay { .. } => {
                let detail = match &key {
                    Some(key) => format!("{ordinal}: the cassette of {} has no response left for {} {}", rule.id, key.method, key.url_normalized),
                    None => format!("{ordinal}: the request body is not available to the platform, so no cassette line matches it"),
                };
                taint = Some(TaintRecord { source: TaintSource::CassetteMiss, detail });
                break 'select Decision::Fail { reason: FailReason::Aborted };
            }
            Action::Record { .. } => unreachable!("handled above"),
            Action::StreamMock { .. } => {
                let detail = format!("{ordinal}: {} is a stream_mock rule, which answers EventSource and WebSocket; {} {} is neither", rule.id, ev.request.method, ev.request.url);
                taint = Some(TaintRecord { source: TaintSource::UnsupportedStream, detail });
                break 'select Decision::Fail { reason: FailReason::Aborted };
            }
            Action::Sequence { steps, after_last } => match net.cursors.next(&rule.id, steps, *after_last) {
                Some(s) => s,
                None => break 'select Decision::Fail { reason: FailReason::Aborted },
            },
            Action::Hold if is_document => return Err(ChromiumError::Unsupported(format!("hold on the document request {}", ev.request.url))),
            Action::Hold => break 'select Decision::Hold,
            Action::Passthrough => break 'select Decision::Passthrough,
            Action::Fail { reason } => break 'select Decision::Fail { reason: *reason },
        };
        Decision::Deliver { due_us: due_of(t, &spec), spec, source: ResponseSource::Mock, requires_progress: false }
    };
    Ok(SelectedRequest { rule: rule.id, entry, decision, taint })
}

/// The exact wire response used by both live delivery and pre-renderer recovery.
pub(super) fn fulfillment(fetch_id: fetch::RequestId, spec: &MockSpec, source: ResponseSource,
    origin: Option<&str>, arrival: Option<VirtualTime>, initial_date_ms: u64) -> fetch::FulfillRequestParams
{
    // A recorded header that occurred several times is kept as one value per line (the engine's form).
    let mut headers: Vec<fetch::HeaderEntry> = spec
        .headers
        .iter()
        .filter(|(k, _)| !k.eq_ignore_ascii_case(ARRIVAL_HEADER))
        .flat_map(|(k, v)| v.split('\n').map(move |v| fetch::HeaderEntry { name: k.clone(), value: v.to_string() }))
        .collect();
    if let Some(at) = arrival {
        let unix_us = initial_date_ms.saturating_mul(1000).saturating_add(at.0);
        headers.push(fetch::HeaderEntry { name: ARRIVAL_HEADER.into(), value: unix_us.to_string() });
    }
    // A replayed response is the recorded server's, CORS headers and all.
    if let Some(origin) = origin.filter(|_| source != ResponseSource::Replay)
        && !spec.headers.keys().any(|k| k.eq_ignore_ascii_case("access-control-allow-origin"))
    {
        headers.push(fetch::HeaderEntry { name: "access-control-allow-origin".into(), value: origin.into() });
        headers.push(fetch::HeaderEntry { name: "access-control-allow-credentials".into(), value: "true".into() });
        // The headers the response sets are readable by the page, as the API meant them to be.
        let names: Vec<&str> = spec.headers.keys().map(String::as_str).filter(|k| !k.to_ascii_lowercase().starts_with("access-control-")).collect();
        if !names.is_empty() && !spec.headers.keys().any(|k| k.eq_ignore_ascii_case("access-control-expose-headers")) {
            headers.push(fetch::HeaderEntry { name: "access-control-expose-headers".into(), value: names.join(", ") });
        }
        match headers.iter_mut().find(|h| h.name.eq_ignore_ascii_case("vary")) {
            Some(vary) if !vary.value.trim().is_empty() => vary.value = format!("{}, origin", vary.value),
            Some(vary) => vary.value = "origin".into(),
            None => headers.push(fetch::HeaderEntry { name: "vary".into(), value: "origin".into() }),
        }
    }
    fetch::FulfillRequestParams {
        response_headers: Some(headers),
        body: Some(cdp::Binary(spec.body_base64.clone().unwrap_or_else(|| base64(spec.body.as_bytes())))),
        ..fetch::FulfillRequestParams::new(fetch_id, i64::from(spec.status))
    }
}

impl Generation {
    /// Whether the trace (written or about to be) records a response for `ordinal`.
    pub fn has_response(&self, ordinal: &str) -> Result<bool, ChromiumError> {
        let answered = |k: &EventKind| matches!(k, EventKind::Response(r) if r.id == ordinal);
        if self.pending.iter().any(|(_, k)| k.recorded().is_some_and(answered)) {
            return Ok(true);
        }
        self.read_trace(|events| events.iter().any(|e| answered(&e.kind)))
    }

    /// Held requests, and held streams whose handshake is unanswered.
    pub fn held_ordinals(&self) -> Vec<String> {
        self.net.held.keys().cloned().chain(self.net.held_streams.iter().filter(|(_, h)| !h.open).map(|(o, _)| o.clone())).collect()
    }

    pub(super) fn give_up_interceptions(&mut self) {
        let gone = self.net.interceptions.give_up();
        self.record_given_up(gone);
    }

    pub(super) fn record_given_up(&mut self, gone: Vec<crate::network::GivenUp>) {
        for g in gone {
            let detail = format!("{} issued at {} ms was not intercepted in time", g.what, g.t.0 / 1000);
            self.record(g.t, EventKind::Taint(TaintRecord { source: TaintSource::UninterceptedRequest, detail }));
        }
    }

    pub(super) fn take_sent(&mut self, arrival: cdp::Arrival, ev: cnet::RequestWillBeSentEvent) {
        // `wallTime` follows virtual time (S9), so it is the virtual instant the request was issued.
        // A preflight's `wallTime` is the real clock's; it is issued with the request it precedes.
        let t = if ev.initiator.r#type == cnet::InitiatorType::Preflight {
            self.net.preflights.insert(ev.request_id.0.clone());
            if let Some(actual) = &ev.initiator.request_id {
                self.net.preflight_requests.insert(ev.request_id.0.clone(), actual.0.clone());
            }
            let actual = ev.initiator.request_id.as_ref().and_then(|id| self.net.issued.get(&id.0).copied());
            actual.unwrap_or(self.now)
        } else if ev.loader_id.0.is_empty() && ev.frame_id.is_some() {
            // Issued by the browser (a dedicated worker's script): its `wallTime` is the real clock's. The page
            // holds virtual time until the worker has its script, so its clock, read when the request is
            // intercepted (`issue_time`), is the issue instant; until then, where the platform stands.
            self.net.browser_issued.insert(ev.request_id.0.clone());
            self.net.browser_worker_scripts.insert(ev.request_id.0.clone());
            self.now
        } else {
            // A worker's own renderer requests also have no loader, but no frame either.
            // Browser-issued dedicated-worker scripts carry their ancestor frame, including
            // nested workers. Worker fetches use the virtual clock and keep announcement order.
            ms_epoch_to_virtual(ev.wall_time.0 * 1000.0, self.profile.initial_date_ms)
        };
        let url = &ev.request.url;
        // Child document announcements order observer delivery too. Their issue time is read
        // separately from the stopped top frame, never from the browser's wall-clock timestamp.
        // Top navigation is excluded: its time may require a later observer navigate report.
        let child_document = ev.r#type == Some(cnet::ResourceType::Document)
            && ev.frame_id.as_ref().is_some_and(|frame| frame != self.launch_frame());
        if (url.starts_with("http://") || url.starts_with("https://"))
            && (ev.r#type != Some(cnet::ResourceType::Document) || child_document)
        {
            if self.net.browser_issued.contains(&ev.request_id.0) {
                self.net.interceptions.issued_by_browser(&ev.request_id.0, url, t, arrival);
            } else {
                self.net.interceptions.issued(&ev.request_id.0, url, if child_document { self.now } else { t }, arrival);
            }
            if child_document || url.starts_with(STOP_URL) || stream_anchor(url).is_some() {
                self.net.interceptions.collect_separately(&ev.request_id.0);
            }
            // The launch page's: a popup's stream is refused at its interception.
            if stream_anchor(url).is_some() && let Some(frame) = &ev.frame_id
                && self.context_of_session(self.subs.sent.session(&ev.request_id.0).as_deref()).is_none()
            {
                self.net.anchors.insert(ev.request_id.0.clone(), frame.0.clone());
            }
        }
        // A redirect: the passed-through hop it ends was answered with this response, before the next hop
        // is issued (as a redirect the platform answers itself is recorded when it answers).
        if let Some(r) = &ev.redirect_response
            && let Some(p) = self.net.passthrough.remove(&ev.request_id.0)
        {
            let status = r.status as u16;
            let at = p.answered_at(self.now);
            let hop = p.ordinal;
            self.record(at, EventKind::Response(ResponseRecord { id: hop.clone(), status, source: ResponseSource::Passthrough, rule: Some(p.rule) }));
            if let Some(q) = self.quiet.as_mut() {
                q.end(&format!("request:{hop}"), at);
            }
            self.write_cassette_line(&hop, status, engine_headers(&r.headers), Vec::new(), at);
        }
        self.net.issued.insert(ev.request_id.0, t);
    }

    /// A navigation's `wallTime` is stamped by the browser process on the real clock, and while the
    /// document request is held the renderer answers no command. The observer reports each `navigate`
    /// event with its virtual time before the request leaves the page, so that report is the issue time.
    /// The first document is issued at the start boundary; a redirect keeps its navigation's time.
    pub(super) async fn document_issue_time(&mut self, ev: &fetch::RequestPausedEvent) -> Result<VirtualTime, ChromiumError> {
        if ev.frame_id != *self.launch_frame() {
            // A child frame's navigation sends no `navigate` report to the top frame's observer; the page is
            // not navigating, so it answers, at the instant its clock stopped for the navigation.
            return Ok(self.read_time().await?.max(self.now));
        }
        if self.world.is_none() || self.own_navigation {
            return Ok(self.now);
        }
        if let (Some(id), Some((doc_id, t))) = (&ev.network_id, &self.document_request)
            && &id.0 == doc_id
        {
            return Ok(*t);
        }
        let url = ev.request.url.split('#').next().unwrap_or_default().to_string();
        let deadline = tokio::time::Instant::now() + Session::CALL_TIMEOUT;
        loop {
            self.drain_bindings(Duration::ZERO).await?;
            // The latest report of that URL: one replayed after a fault may follow older ones.
            if let Some(i) = self.navigates.iter().rposition(|(u, _)| *u == url) {
                let (_, t) = self.navigates.remove(i);
                self.navigates.clear();
                return Ok(t.max(self.now));
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(ChromiumError::Unsupported(format!("navigation to {url} without a navigate event")));
            }
            self.drain_bindings(Duration::from_millis(20)).await?;
        }
    }

    /// A top-frame navigation is announced by its `navigate` report. While that report is unread and
    /// an observer report waits behind requests announced before it, the navigation's place is that
    /// waiting report's: those requests are decided first.
    fn top_navigation_place(&self, url: &str) -> Option<cdp::Arrival> {
        if self.navigates.iter().any(|(u, _)| u == url) { return None }
        self.deferred_binding.as_ref().map(|(at, _)| *at)
    }

    /// Whether the top-frame navigation `ev` must wait for requests announced before its `navigate`
    /// report: that report, which gives its issue instant, is read only once they are recorded, and
    /// their records precede the navigation's. Waits for the report as `document_issue_time` does.
    async fn follows_earlier_requests(&mut self, ev: &fetch::RequestPausedEvent) -> Result<bool, ChromiumError> {
        if ev.resource_type != cnet::ResourceType::Document || ev.frame_id != *self.launch_frame()
            || self.world.is_none() || self.own_navigation
        {
            return Ok(false);
        }
        if let (Some(id), Some((doc_id, _))) = (&ev.network_id, &self.document_request)
            && &id.0 == doc_id
        {
            return Ok(false);
        }
        let url = ev.request.url.split('#').next().unwrap_or_default().to_string();
        let deadline = tokio::time::Instant::now() + Session::CALL_TIMEOUT;
        loop {
            self.drain_bindings(Duration::ZERO).await?;
            if self.navigates.iter().any(|(u, _)| *u == url) { return Ok(false) }
            if self.deferred_binding.is_some() { return Ok(true) }
            // No report: `document_issue_time` fails the navigation.
            if tokio::time::Instant::now() >= deadline { return Ok(false) }
            self.drain_bindings(Duration::from_millis(20)).await?;
        }
    }

    pub(super) async fn drain_sent(&mut self, timeout: Duration) -> Result<(), ChromiumError> {
        self.drain_request_announcements(timeout).await?;
        self.take_frame_requests()
    }

    /// Request announcements alone: reading observer reports must not register a pending navigation
    /// before a beforeunload receipt can end the dialog stop that decides that navigation.
    pub(super) async fn drain_request_announcements(&mut self, timeout: Duration) -> Result<(), ChromiumError> {
        while let Some((arrival, ev)) = self.subs.sent.next_timeout_stamped(timeout).await? {
            self.take_sent(arrival, ev);
        }
        Ok(())
    }

    /// The virtual instant a paused request was issued, from its `requestWillBeSent`, which may arrive
    /// after the pause.
    pub(super) async fn issue_time(&mut self, ev: &fetch::RequestPausedEvent) -> Result<VirtualTime, ChromiumError> {
        // Fetch may announce a redirect's next pause before Network announces the previous
        // response. Both hops share a network id: finish the old hop before replacing its binding.
        if let (Some(id), Some(previous)) = (&ev.network_id, &ev.redirected_request_id) {
            let deadline = tokio::time::Instant::now() + Session::CALL_TIMEOUT;
            while self.net.passthrough.get(&id.0).is_some_and(|p| p.fetch_id == previous.0) {
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                match self.subs.sent.next_timeout_stamped(left).await? {
                    Some((arrival, sent)) => self.take_sent(arrival, sent),
                    None => return Err(ChromiumError::CallTimeout("Network.requestWillBeSent redirect")),
                }
            }
        }
        if ev.resource_type == cnet::ResourceType::Document {
            return self.document_issue_time(ev).await;
        }
        // A request with no network id (a worker's whose Network domain could not be enabled): it was issued
        // at the page's clock now, read.
        let Some(id) = ev.network_id.as_ref() else {
            return Ok(self.read_time().await?.max(self.now));
        };
        self.drain_sent(Duration::ZERO).await?;
        let deadline = tokio::time::Instant::now() + Session::CALL_TIMEOUT;
        loop {
            if let Some(actual) = self.net.preflight_requests.get(&id.0) {
                if let Some(t) = self.net.issued.get(actual).copied() {
                    self.net.issued.insert(id.0.clone(), t);
                    return Ok(t);
                }
                // Browser and renderer notifications can arrive in either order.
                // The earlier adapter boundary is not an issue-time observation.
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                match self.subs.sent.next_timeout_stamped(left).await? {
                    Some((arrival, sent)) => self.take_sent(arrival, sent),
                    None => return Err(ChromiumError::CallTimeout("preflight application request issue time")),
                }
                continue;
            }
            if self.net.browser_issued.remove(&id.0) {
                self.refresh_worker_lineages().await?;
                let observed = self.read_time().await?.max(self.now);
                let t = if ev.redirected_request_id.is_none() {
                    self.net.worker_request_issues.get(&id.0).copied().unwrap_or(observed)
                } else { observed };
                // The browser can announce the script while Worker's constructor is still
                // returning in the renderer. The clock round trip brings its construction
                // report; publish it before this script, regardless of browser-event arrival.
                self.drain_bindings(Duration::ZERO).await?;
                self.net.issued.insert(id.0.clone(), t);
                return Ok(t);
            }
            if let Some(t) = self.net.issued.get(&id.0).copied() {
                return Ok(t);
            }
            // Issued before this session existed (while no recorder ran), its `requestWillBeSent` never
            // reaches this session; the engine's stamp is the same instant (engine patch 24).
            if self.patched
                && let Some(us) = ev.frontcap_issue_time_us.as_deref().and_then(|s| s.parse::<u64>().ok())
            {
                let t = VirtualTime(us.saturating_sub(self.profile.initial_date_ms.saturating_mul(1000)));
                self.net.issued.insert(id.0.clone(), t);
                return Ok(t);
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.subs.sent.next_timeout_stamped(left).await? {
                Some((arrival, sent)) => self.take_sent(arrival, sent),
                None => return Err(ChromiumError::CallTimeout("Network.requestWillBeSent")),
            }
        }
    }

    /// The main thread cannot reply until this request ends. Network flushes the
    /// request's announcement and earlier notifications before entering the wait.
    pub(super) fn blocks_main_thread(&self, ev: &fetch::RequestPausedEvent) -> bool {
        self.patched && ev.frontcap_blocks_main_thread == Some(true) && !is_response_stage(ev)
    }

    pub(super) async fn serve_blocking(&mut self, ev: fetch::RequestPausedEvent) -> Result<(), ChromiumError> {
        let previous = std::mem::replace(&mut self.blocking, true);
        let handled = Box::pin(self.select_and_serve_blocking(ev)).await;
        self.blocking = previous;
        handled
    }

    async fn select_and_serve_blocking(&mut self, ev: fetch::RequestPausedEvent) -> Result<(), ChromiumError> {
        self.select_blocking_prefix(&ev).await?;
        let stamp = ev.frontcap_issue_time_us.as_deref().and_then(|s| s.parse::<u64>().ok());
        let t = stamp.map_or(self.now, |us| VirtualTime(us.saturating_sub(self.profile.initial_date_ms.saturating_mul(1000)))).max(self.now);
        if let Some(id) = &ev.network_id {
            self.net.interceptions.handled(&id.0);
            self.net.issued.insert(id.0.clone(), t);
        }
        self.handle_at(ev, t).await
    }

    /// Preserve ordinary requests' same-session issue order without asking the
    /// blocked renderer to run. Only their durable decisions are made here.
    async fn select_blocking_prefix(&mut self, blocking: &fetch::RequestPausedEvent) -> Result<(), ChromiumError> {
        let Some(id) = &blocking.network_id else { return Ok(()) };
        let headers = super::adoption::paused_headers(blocking);
        let preflight = blocking.request.method == "OPTIONS"
            && headers.keys().any(|h| h.eq_ignore_ascii_case("access-control-request-method"));
        // Preflights are browser mechanisms, not another application sequence step;
        // redirects retain the first hop's already allocated place.
        if preflight || self.net.interceptions.known(&id.0) { return Ok(()) }
        let deadline = Instant::now() + Session::CALL_TIMEOUT;
        loop {
            self.drain_request_announcements(Duration::ZERO).await?;
            if self.net.interceptions.place(&id.0).is_some() { break; }
            let left = deadline.saturating_duration_since(Instant::now());
            let Some((arrival, event)) = self.subs.sent.next_timeout_stamped(left).await? else {
                return Err(ChromiumError::CallTimeout("synchronous request announcement"));
            };
            self.take_sent(arrival, event);
        }
        // A later task cannot consume sequence positions reserved by an earlier collection.
        if self.request_round.as_ref().is_some_and(|round|
            self.net.interceptions.place(&id.0).is_some_and(|at| at > round.through)) {
            self.select_interrupted_round().await?;
        }
        let owner = self.subs.sent.session(&id.0);
        let mut separate = std::collections::BTreeSet::new();
        loop {
            self.take_paused()?;
            self.drain_network().await?;
            // An earlier cross-origin request may need its preflight answered
            // before its own Fetch interception can be published.
            if let Some(index) = self.queued.iter().position(|(_, e)| e.request.method == "OPTIONS"
                && super::adoption::paused_headers(e).keys().any(|h| h.eq_ignore_ascii_case("access-control-request-method"))) {
                let (_, event) = self.queued.remove(index);
                if let Some(id) = &event.network_id { self.net.interceptions.handled(&id.0); }
                Box::pin(self.handle_at(event, self.now)).await?;
                continue;
            }
            let next = self.net.interceptions.before(&id.0).into_iter()
                .find(|(earlier, _)| !separate.contains(earlier) && self.subs.sent.session(earlier) == owner);
            let Some((earlier, _)) = next else { break };
            if let Some(index) = self.queued.iter().position(|(_, e)| e.network_id.as_ref().is_some_and(|n| n.0 == earlier)) {
                let event = &self.queued[index].1;
                // Navigation and stream/control admission have their own lifecycle;
                // they are not ordinary sequence/replay allocations.
                if event.resource_type == cnet::ResourceType::Document || event.request.url.starts_with(STOP_URL)
                    || stream_anchor(&event.request.url).is_some() {
                    separate.insert(earlier);
                    continue;
                }
                let (_, event) = self.queued.remove(index);
                let issued = self.net.issued.get(&earlier).copied().ok_or_else(|| ChromiumError::Shape("announced prefix request has no issue instant".into()))?;
                self.net.interceptions.handled(&earlier);
                // It is not itself synchronous: retain an armed interception stop,
                // but do not perform mark_seen or any response delivery yet.
                self.blocking = false;
                let selected = self.select_request(&event, issued).await;
                self.blocking = true;
                if let Some(binding) = selected? { self.deferred_requests.push((event, binding)); }
                continue;
            }
            if Instant::now() >= deadline {
                return Err(ChromiumError::Shape(format!("earlier synchronous-prefix interception {earlier} did not arrive before blocker {}", id.0)));
            }
            self.wait_for_interceptions().await?;
        }
        Ok(())
    }

    /// Finish an earlier round's decisions without issuing commands to the blocked renderer.
    /// Delivery waits until the synchronous task returns, as for its ordinary same-session prefix.
    async fn select_interrupted_round(&mut self) -> Result<(), ChromiumError> {
        let deadline = Instant::now() + Session::CALL_TIMEOUT;
        while self.round_outstanding() {
            self.take_paused()?;
            self.drain_request_announcements(Duration::ZERO).await?;
            self.drain_network().await?;
            if !self.round_outstanding() { break; }
            match self.next_in_order_with(true) {
                Next::Handle(index) => {
                    let (_, event) = self.queued.remove(index);
                    if event.resource_type == cnet::ResourceType::Document || event.request.url.starts_with(STOP_URL)
                        || stream_anchor(&event.request.url).is_some() || is_response_stage(&event) {
                        return Err(ChromiumError::Unsupported("a synchronous request interrupted non-ordinary round admission".into()));
                    }
                    let id = event.network_id.as_ref().ok_or_else(|| ChromiumError::Shape("round member has no network identity".into()))?;
                    let issued = self.net.worker_request_issues.get(&id.0).or_else(|| self.net.issued.get(&id.0))
                        .copied().ok_or_else(|| ChromiumError::Shape("round member has no issue instant".into()))?;
                    self.net.interceptions.handled(&id.0);
                    self.blocking = false;
                    let selected = self.select_request(&event, issued).await;
                    self.blocking = true;
                    if let Some(binding) = selected? {
                        self.request_round.as_mut().expect("active round").selected.push((event, binding));
                    }
                }
                _ => {
                    if Instant::now() >= deadline { return Err(ChromiumError::CallTimeout("interrupted request round")); }
                    self.wait_for_interceptions().await?;
                }
            }
        }
        if let Some(round) = self.request_round.take() {
            self.deferred_requests.extend(round.selected);
        }
        Ok(())
    }

    async fn deliver_deferred_requests(&mut self) -> Result<(), ChromiumError> {
        let blocking = self.blocking;
        if self.shared_arrival_service_active() { self.blocking = false; }
        let result = async {
            while !self.deferred_requests.is_empty() {
                let (event, binding) = self.deferred_requests.remove(0);
                self.mark_seen(&event).await;
                self.deliver_selected(event, &binding).await?;
            }
            Ok(())
        }.await;
        self.blocking = blocking;
        result
    }

    /// Awaits a page command while answering the requests its page script makes the main thread wait for:
    /// the page answers the command only after they end. Other interceptions are queued, as `take_paused`
    /// queues them.
    pub(super) async fn serviced<T>(&mut self, call: impl std::future::Future<Output = Result<T, ChromiumError>>) -> Result<T, ChromiumError> {
        tokio::pin!(call);
        // A worker that could not be enrolled leaves work the command may wait for unable to finish:
        // that failure, reported as it happens, is the outcome rather than the command's timeout.
        let workers_changed = self.subs.workers.lock().unwrap().changed.clone();
        let result = loop {
            let paused = tokio::select! {
                biased;
                r = &mut call => break r,
                Some(a) = self.subs.answers.recv() => { self.answer_blocking(a).await; continue }
                ev = self.subs.paused.next() => ev?,
                _ = workers_changed.notified() => match self.worker_enrollment_result() {
                    Err(failure) => break Err(failure),
                    Ok(()) => continue,
                },
            };
            if self.blocks_main_thread(&paused) {
                self.serve_blocking(paused).await?;
            } else {
                self.queued.push((Instant::now(), paused));
            }
        };
        if result.is_ok() {
            self.after_arrival().await?;
            Box::pin(self.deliver_deferred_requests()).await?;
        }
        result
    }

    /// Once the page runs again after its clock moved to a waited-for response's arrival, the grant kept in
    /// the observer world covers that instant, so a relaunched recorder does not take it for a runaway.
    pub(super) async fn after_arrival(&mut self) -> Result<(), ChromiumError> {
        // The grant is kept in the launch page's observer world; a popup's command leaves it for later.
        if self.shared_arrival_service_active() { return Ok(()); }
        if self.popup_id().is_some() || !std::mem::take(&mut self.arrived) {
            return Ok(());
        }
        self.take_contexts()?;
        while let Some(world) = self.world_unique.clone() {
            let expr = format!("(() => {{ const state = globalThis.__frontcap; const at = BigInt('{}'); const arrived = BigInt(state.synchronousArrivalUs ?? '0'); state.synchronousArrivalUs = String(arrived > at ? arrived : at); const granted = BigInt(state.grantedUs ?? '0'); state.grantedUs = String(granted > at ? granted : at); }})()", self.now.0);
            // A sibling window can start its first synchronous request on this
            // renderer immediately after the previous arrival. Keep servicing
            // routed interceptions while awaiting the observer write, as for a
            // grant. Keep actual completed arrivals separate from future
            // grants: a delayed policy write can retain this proven instant
            // without retaining an older, superseded future grant. Integer max
            // prevents either completed-arrival watermark from moving backwards.
            let page = self.page.clone();
            match Box::pin(self.serviced(page.eval_unique(&expr, &world))).await {
                Ok(_) => break,
                Err(error) => self.retire_missing_observer(&world, error)?,
            }
        }
        Ok(())
    }

    /// Handles intercepted requests in the page's issue order (`Interceptions`): `ev`, and those that arrive
    /// meanwhile, each once every request issued before it has been handled. Waiting on an earlier
    /// request's interception ends after `INTERCEPTION_WAIT` of wall time: that one is recorded as taint
    /// and handled whenever it arrives.
    pub(super) async fn handle_request(&mut self, ev: fetch::RequestPausedEvent) -> Result<(), ChromiumError> {
        self.queued.push((Instant::now(), ev));
        self.handle_queued().await
    }

    /// Queues the interceptions that have arrived.
    pub(super) fn take_paused(&mut self) -> Result<(), ChromiumError> {
        while let Some(ev) = self.subs.paused.try_next()? {
            self.queued.push((Instant::now(), ev));
        }
        Ok(())
    }

    /// Handles the queued interceptions (`handle_request`), and stream anchors that ended without one, until
    /// none is left. On the patched engine, ordinary requests are collected first and handled as one
    /// round (`RequestRound`).
    pub(super) async fn handle_queued(&mut self) -> Result<(), ChromiumError> {
        while !self.queued.is_empty() || self.net.interceptions.next_ended().is_some() || !self.deferred_requests.is_empty()
            || self.request_round.is_some()
        {
            // A request the page's main thread waits for needs delivery first.
            // Allocate its announced prefix before answering it without renderer calls.
            if let Some(i) = self.queued.iter().position(|(_, ev)| self.blocks_main_thread(ev)) {
                let (_, ev) = self.queued.remove(i);
                self.serve_blocking(ev).await?;
                continue;
            }
            if !self.deferred_requests.is_empty() {
                if self.shared_arrival_service_active() {
                    Box::pin(self.deliver_deferred_requests()).await?;
                    continue;
                }
                let page = self.page.clone();
                self.serviced(page.call(&emulation::GetVirtualTimeParams::default())).await?;
                continue;
            }
            self.drain_sent(Duration::ZERO).await?;
            self.take_paused()?;
            if self.queued.iter().any(|(_, ev)| self.blocks_main_thread(ev)) {
                continue;
            }
            if let Some(i) = self.queued.iter().position(|(_, ev)| is_response_stage(ev)) {
                let (_, ev) = self.queued.remove(i);
                self.finish_response_stage(ev).await?;
                continue;
            }
            if let Some((id, url, t, at)) = self.ended_due() {
                self.abandon_unpaused_anchor(&id, &url, t, at).await?;
                continue;
            }
            // Every request of the round is allocated: its answers go out together.
            if self.request_round.is_some() && !self.round_outstanding() {
                Box::pin(self.deliver_round()).await?;
                continue;
            }
            if self.queued.is_empty() && self.request_round.is_none() {
                break;
            }
            if self.patched && self.request_round.is_none() && !self.collecting_requests && self.queued.iter().any(|(_, ev)|
                ev.resource_type != cnet::ResourceType::Document && !ev.request.url.starts_with(STOP_URL)
                    && stream_anchor(&ev.request.url).is_none()) {
                Box::pin(self.collect_ordinary_requests()).await?;
                self.take_paused()?;
                if self.queued.is_empty() { continue; }
                if self.queued.iter().any(|(_, ev)| self.blocks_main_thread(ev)) { continue; }
                self.request_round = self.net.interceptions.latest()
                    .map(|through| super::request_collection::RequestRound { through, selected: Vec::new() });
                // Nothing announced is left to allocate (a redirect's next hop, an unannounced one).
                if !self.round_outstanding() { self.request_round = None; }
            }
            match self.next_in_order() {
                Next::Handle(i) => {
                    if self.request_round.is_none() && self.queued[i].1.network_id.as_ref().is_some_and(|id| self.net.browser_issued.contains(&id.0)) {
                        // A browser script announcement may race the still-running constructor.
                        // Finish its renderer task before choosing: earlier renderer requests and
                        // the constructor report must not be overtaken by this browser-side event.
                        self.read_time().await?;
                        self.drain_sent(Duration::ZERO).await?;
                        self.drain_bindings(Duration::ZERO).await?;
                        self.take_paused()?;
                        if !matches!(self.next_in_order(), Next::Handle(next) if next == i) { continue; }
                    }
                    let (_, ev) = self.queued.remove(i);
                    self.handle_gone_or(ev).await?;
                }
                Next::WaitFor(at) => {
                    // Each waited for no longer than `INTERCEPTION_WAIT` from when it was announced.
                    let intercepted: std::collections::BTreeSet<String> = self.queued.iter()
                        .filter_map(|(_, ev)| ev.network_id.as_ref().map(|id| id.0.clone())).collect();
                    let gone = match Instant::now().checked_sub(INTERCEPTION_WAIT) {
                        Some(by) => self.net.interceptions.give_up_unintercepted_before(at, Some(by), &intercepted),
                        None => Vec::new(),
                    };
                    if gone.is_empty() {
                        self.wait_for_interceptions().await?;
                    }
                    self.record_given_up(gone);
                }
                // Its `requestWillBeSent` is still to come; past the wait, `issue_time` waits for it.
                Next::Unannounced(i) if self.queued[i].0.elapsed() >= INTERCEPTION_WAIT => {
                    let (_, ev) = self.queued.remove(i);
                    self.handle_gone_or(ev).await?;
                }
                Next::Unannounced(_) => self.wait_for_interceptions().await?,
            }
        }
        Ok(())
    }

    /// Waits a moment for an interception, taking those that arrive. Requests that end without one (a
    /// memory-cache hit, a blocked one) stop being waited for.
    pub(super) async fn wait_for_interceptions(&mut self) -> Result<(), ChromiumError> {
        self.drain_network().await?;
        if let Some(ev) = self.subs.paused.next_timeout(Duration::from_millis(2)).await? {
            self.queued.push((Instant::now(), ev));
        }
        Ok(())
    }

    /// Handles one interception. One the engine no longer knows (the page aborted the request, or its
    /// frame's navigation was replaced, while it waited) is recorded as failed, as a hold that ends so.
    pub(super) async fn handle_gone_or(&mut self, ev: fetch::RequestPausedEvent) -> Result<(), ChromiumError> {
        let before = self.net.last_ordinal();
        // Shared-policy admission can service this path inside an active advance.
        // Keep the large request future off that nested polling stack.
        match Box::pin(self.handle_in_order(ev)).await {
            Err(e) if is_gone(&e) => {
                let ordinal = self.net.last_ordinal();
                if ordinal != before
                    && let Some(ordinal) = ordinal
                {
                    self.request_gone(&ordinal);
                }
                Ok(())
            }
            r => r,
        }
    }

    /// A request the platform was answering is gone: no answer reaches the page.
    pub(super) fn request_gone(&mut self, ordinal: &str) {
        let passed: Vec<String> = self.net.passthrough.iter().filter(|(_, p)| p.ordinal == ordinal).map(|(id, _)| id.clone()).collect();
        for id in passed {
            self.net.passthrough.remove(&id);
            self.net.loading_documents.remove(&id);
        }
        self.net.recording.remove(ordinal);
        if self.net.held.contains_key(ordinal) {
            self.end_hold(ordinal);
            return;
        }
        self.record(self.now, EventKind::Response(ResponseRecord { id: ordinal.to_string(), status: 0, source: ResponseSource::Fail, rule: None }));
        if let Some(q) = self.quiet.as_mut() {
            q.end(&format!("request:{ordinal}"), self.now);
        }
    }

    /// The anchor that ended without an interception, once no queued interception comes before it.
    fn ended_due(&self) -> Option<(String, String, VirtualTime, cdp::Arrival)> {
        let (id, url, t, at) = self.net.interceptions.next_ended()?;
        let before = self.queued.iter().any(|(_, ev)| match self.place_of(ev) {
            Place::Now => !ev.network_id.as_ref().is_some_and(|id| self.net.browser_issued.contains(&id.0)),
            Place::At(place) => place < at,
            Place::Unannounced => false,
        });
        (!before).then_some((id, url, t, at))
    }

    /// The queued interception to handle next: one with no place in the issue order at once, otherwise the
    /// first in the declared order (its issuer, then its announcement), once nothing before it is still to
    /// be intercepted. While a round is being allocated, only its members are taken.
    pub(super) fn next_in_order(&self) -> Next {
        self.next_in_order_with(false)
    }

    fn next_in_order_with(&self, members_only: bool) -> Next {
        let through = self.request_round.as_ref().map(|round| round.through);
        let mut earliest: Option<((Issuer, cdp::Arrival), usize)> = None;
        let mut unannounced = None;
        let mut browser_issued = None;
        for (i, (_, ev)) in self.queued.iter().enumerate() {
            // Preflights, response stages and later control work cannot become members of a
            // sealed round merely because a synchronous wait interrupted its allocation.
            if members_only && self.round_place(ev).is_none_or(|at| through.is_none_or(|end| at > end)) {
                continue;
            }
            let browser = ev.network_id.as_ref().is_some_and(|id| self.net.browser_issued.contains(&id.0));
            let at = match self.place_of(ev) {
                // A browser-issued worker script has no place among renderer announcements;
                // a round orders it by its worker.
                Place::Now if browser => match through.and_then(|_| self.round_place(ev)) {
                    Some(at) => at,
                    None => {
                        browser_issued.get_or_insert(i);
                        continue;
                    }
                },
                Place::Now => return Next::Handle(i),
                Place::At(at) => at,
                Place::Unannounced => {
                    unannounced.get_or_insert(i);
                    continue;
                }
            };
            // A later round waits for this one's answers.
            if through.is_some_and(|through| at > through) {
                continue;
            }
            // Outside a round, the issue order alone.
            let issuer = if through.is_some() { self.issuer_of_paused(ev) } else { Issuer::MainThread };
            let key = (issuer, at);
            if earliest.as_ref().is_none_or(|(e, _)| key < *e) {
                earliest = Some((key, i));
            }
        }
        if let Some((key, i)) = earliest {
            // Announced before it in the declared order and not intercepted yet: wait for it. A
            // main-thread request waits for that thread's earlier announcements, a worker's for the
            // round's main-thread announcements and earlier workers'.
            let intercepted: std::collections::BTreeSet<&str> = self.queued.iter()
                .filter_map(|(_, ev)| ev.network_id.as_ref().map(|id| id.0.as_str())).collect();
            let blocked = self.net.interceptions.outstanding().any(|(id, at)| {
                if id.is_some_and(|id| intercepted.contains(id)) || through.is_some_and(|through| at > through) {
                    return false;
                }
                let issuer = match (id, through) {
                    (Some(id), Some(_)) => self.issuer_of(id),
                    _ => Issuer::MainThread,
                };
                (issuer, at) < key
            });
            let (issuer, at) = key;
            return match (blocked, issuer, through) {
                (false, ..) => Next::Handle(i),
                (true, Issuer::MainThread, _) | (true, _, None) => Next::WaitFor(at),
                (true, _, Some(through)) => Next::WaitFor(cdp::Arrival(through.0.saturating_add(1))),
            };
        }
        // The round's remaining members are announced and still to be intercepted.
        if let Some(through) = through.map(|t| cdp::Arrival(t.0.saturating_add(1)))
            && self.net.interceptions.earlier_than(through)
        {
            return Next::WaitFor(through);
        }
        match unannounced {
            Some(i) => Next::Unannounced(i),
            None => match self.net.interceptions.first_request() {
                // The browser script is queued, but a renderer request announced by the completed
                // task has not reached Fetch yet. Wait for it, including its own timeout boundary.
                Some(at) => Next::WaitFor(cdp::Arrival(at.0.saturating_add(1))),
                None => Next::Handle(browser_issued.expect("the queue is not empty")),
            },
        }
    }

    /// The issuer of a paused request, by its Network-domain id.
    /// The issuer of a paused request: as `issuer_of` its network id, or an unresolved worker when
    /// the engine names a worker (`frontcapWorkerId`) no frozen owner resolves.
    pub(super) fn issuer_of_paused(&self, ev: &fetch::RequestPausedEvent) -> Issuer {
        let issuer = ev.network_id.as_ref().map_or(Issuer::MainThread, |id| self.issuer_of(&id.0));
        if issuer == Issuer::MainThread && ev.frontcap_worker_id.is_some() { Issuer::UnresolvedWorker } else { issuer }
    }

    /// Where an intercepted request stands in the page's issue order. The page's own document, a request
    /// the platform already handled once (a redirect's next hop) or gave up on, a preflight (its request is
    /// intercepted only after it), and requests the page's thread did not announce (the browser's, a
    /// worker's without a network id) take none.
    pub(super) fn place_of(&self, ev: &fetch::RequestPausedEvent) -> Place {
        if ev.request.url.starts_with(STOP_URL) {
            return Place::Now;
        }
        if ev.resource_type == cnet::ResourceType::Document {
            // A redirect's next hop, or a navigation the platform started, is announced by no page event.
            if ev.network_id.as_ref().is_some_and(|id| self.net.interceptions.known(&id.0)) {
                return Place::Now;
            }
            if ev.frame_id == *self.launch_frame()
                && let Some(at) = self.top_navigation_place(ev.request.url.split('#').next().unwrap_or_default())
            {
                return Place::At(at);
            }
            return self.net.interceptions.navigation_place(&ev.frame_id.0).map_or(Place::Now, Place::At);
        }
        let Some(id) = ev.network_id.as_ref() else { return Place::Now };
        let i = &self.net.interceptions;
        if i.known(&id.0) || self.net.preflights.contains(&id.0) || self.net.browser_issued.contains(&id.0) {
            return Place::Now;
        }
        i.place(&id.0).map_or(Place::Unannounced, Place::At)
    }

    pub(super) async fn handle_in_order(&mut self, ev: fetch::RequestPausedEvent) -> Result<(), ChromiumError> {
        if self.follows_earlier_requests(&ev).await? {
            self.queued.insert(0, (Instant::now(), ev));
            return Ok(());
        }
        if let Some(at) = ev.network_id.as_ref().and_then(|id| self.net.interceptions.place(&id.0)) {
            self.drain_bindings_before(at).await?;
        }
        let mut issued = None;
        if ev.resource_type == cnet::ResourceType::Document {
            if ev.frame_id != *self.launch_frame() {
                // A child frame's issue time is the renderer's answer, which comes after the task that
                // issued this navigation: every navigation that task requested has been announced by then.
                issued = Some(self.issue_time(&ev).await?);
                self.drain_sent(Duration::ZERO).await?;
                // One the page replaced in that task (or later, before its interception) reached
                // interception by the browser's timing only: it is not the page's request.
                if self.net.interceptions.replaced_navigation(&ev.frame_id.0, &ev.request.url) {
                    if let Some(id) = &ev.network_id {
                        self.net.interceptions.handled(&id.0);
                    }
                    return self.fail_anchor(ev.request_id).await;
                }
            }
            // Its frame's navigation is intercepted (a later one, replacing it, stays pending).
            if self.net.interceptions.navigation_ended_at(&ev.frame_id.0, &ev.request.url) {
                if let Some(id) = &ev.network_id {
                    self.net.interceptions.handled(&id.0);
                }
                // Given up on: its record follows what was written meanwhile.
                let t = match issued {
                    Some(t) => t,
                    None => self.issue_time(&ev).await?,
                };
                return self.handle_at(ev, t.max(self.log.last_time())).await;
            }
        }
        let overdue = ev.network_id.as_ref().is_some_and(|id| self.net.interceptions.handled(&id.0));
        let t = match issued {
            Some(t) => t,
            None => self.issue_time(&ev).await?,
        };
        // Given up on: events after its issue time are already written, so its record follows them.
        let t = if overdue { t.max(self.log.last_time()) } else { t };
        self.handle_at(ev, t).await
    }

    /// Binds the paused request `ev` to `ordinal` and its decision, durably, before the platform releases
    /// or answers it: a relaunched recorder adopts it instead of deciding it again.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn bind(&mut self, ev: &fetch::RequestPausedEvent, ordinal: &str, t: VirtualTime, rule: &RuleId, entry: Option<u64>, origin: Option<String>, decision: Decision) -> Result<Binding, ChromiumError> {
        let document_request = ev.resource_type == cnet::ResourceType::Document && ev.frame_id == *self.launch_frame();
        let binding = Binding {
            blocks_main_thread: ev.frontcap_blocks_main_thread == Some(true),
            frame_id: Some(ev.frame_id.0.clone()),
            recording: None, continuation: None,
            selection_taint: None,
            rules_revision: self.net.rules_revision,
            ordinal: ordinal.to_string(),
            target_id: self.target_of_fetch(&ev.request_id),
            fetch_id: ev.request_id.0.clone(),
            network_id: ev.network_id.as_ref().map(|i| i.0.clone()),
            worker_lineage: self.worker_lineage(ev)?,
            document: self.documents,
            issued_us: t.0,
            issued_during_progress: self.progressing,
            rule: rule.clone(),
            entry,
            body: RequestBody::of(ev.request.post_data.as_deref(), ev.request.has_post_data),
            method: ev.request.method.clone(),
            url: ev.request.url.clone(),
            origin,
            decision,
            document_request,
            navigation: (document_request && self.own_navigation).then(|| self.navigation_request.clone()).flatten(),
        };
        self.bindings.append(&BindingEntry::Bound(Box::new(binding.clone())))?;
        Ok(binding)
    }

    /// Records the resolution about to be sent for the bound pause `fetch_id`, with its exact payload.
    pub(super) fn dispatching(&mut self, ordinal: &str, fetch_id: &fetch::RequestId, resolution: Resolution) -> Result<(), ChromiumError> {
        self.bindings.append(&BindingEntry::Dispatched { ordinal: ordinal.to_string(), fetch_id: fetch_id.0.clone(), resolution })
    }

    /// A recorded request paused at its response (see `adopting`): its cassette line is written from the
    /// paused response, then it continues to the page unchanged.
    pub(super) async fn finish_response_stage(&mut self, ev: fetch::RequestPausedEvent) -> Result<(), ChromiumError> {
        let ordinal = self.net.response_stage.remove(&ev.request_id.0);
        if let (Some(ordinal), Some(status)) = (ordinal.filter(|o| self.net.recording.contains_key(o)), ev.response_status_code) {
            let status = status as u16;
            let body = match self.fetch_session(&ev.request_id).call(&fetch::GetResponseBodyParams::new(ev.request_id.clone())).await {
                Ok(b) if b.base64_encoded => frontcap_core::encode::unbase64(&b.body),
                Ok(b) => Some(b.body.into_bytes()),
                Err(_) if matches!(status, 204 | 304) => Some(Vec::new()),
                Err(_) => None,
            };
            let mut headers: BTreeMap<String, String> = BTreeMap::new();
            for h in ev.response_headers.iter().flatten() {
                headers.entry(h.name.clone()).and_modify(|v| { v.push('\n'); v.push_str(&h.value) }).or_insert_with(|| h.value.clone());
            }
            match body {
                Some(body) => self.write_cassette_line(&ordinal, status, headers, body, self.now),
                None => {
                    self.net.recording.remove(&ordinal);
                    let detail = format!("{ordinal}: the engine kept no body to record");
                    self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::CassetteMiss, detail }));
                }
            }
        }
        match self.fetch_session(&ev.request_id).call(&fetch::ContinueRequestParams::new(ev.request_id)).await {
            Err(e) if is_gone(&e) => Ok(()),
            r => r.map(|_| ()),
        }
    }

    pub(super) async fn interception_receipts(&mut self) -> Result<BTreeMap<String, (String, fetch::FrontcapReceipt)>, ChromiumError> {
        let mut receipts = BTreeMap::new();
        for (target, page) in self.interception_pages() {
            for receipt in page.call(&fetch::FrontcapReceiptsParams::default()).await?.receipts {
                let id = receipt.request_id.0.clone();
                if receipts.insert(id.clone(), (target.clone(), receipt)).is_some() {
                    return Err(ChromiumError::Shape("native receipt identity belongs to more than one target".into()));
                }
                self.subs.paused.remember_session(id, &page.id);
            }
        }
        Ok(receipts)
    }

    /// The receipts the engine keeps now (engine patch 24), after forgetting those already recorded.
    #[cfg(feature = "test-hooks")]
    pub async fn engine_receipts(&mut self) -> Result<Vec<String>, ChromiumError> {
        self.forget_receipts().await;
        Ok(self.interception_receipts().await?.into_keys().collect())
    }

    /// Lets the engine forget receipts the trace no longer needs (engine patch 24). Best effort: a receipt
    /// left behind is recognized as already recorded by the next adoption.
    pub(super) async fn forget_receipts(&mut self) {
        if !self.patched || self.receipts_ready.is_empty() {
            return;
        }
        let ids: Vec<String> = std::mem::take(&mut self.receipts_ready);
        let mut by_session: BTreeMap<String, (Session, Vec<String>)> = BTreeMap::new();
        for id in ids {
            let page = self.fetch_session(&fetch::RequestId(id.clone()));
            by_session.entry(page.id.clone()).or_insert_with(|| (page, Vec::new())).1.push(id);
        }
        for (_, (page, ids)) in by_session {
            let forget = fetch::FrontcapForgetReceiptsParams::new(ids.iter().cloned().map(fetch::RequestId).collect());
            // Kept for the next drain when the call did not reach the engine; a rejection means nothing is left.
            if let Err(e) = page.call(&forget).await && !is_rejection(&e) {
                self.receipts_ready.extend(ids);
            }
        }
    }

    pub(super) async fn handle_at(&mut self, ev: fetch::RequestPausedEvent, t: VirtualTime) -> Result<(), ChromiumError> {
        self.mark_seen(&ev).await;
        if let Some(name) = ev.request.url.strip_prefix(STOP_URL) {
            // The observer's condition stop: virtual time stopped at the request's issue.
            // This is platform control, not an application load. Remove its pause vote while
            // explicitly frozen so aborting it cannot add Blink's 10 ms load-completion step
            // to the next grant and move a nearby application timer past its deadline.
            if self.patched {
                self.policy(emulation::VirtualTimePolicy::Pause, None).await?;
                if let Some(id) = &ev.network_id {
                    self.set_request_pauses(id.0.clone(), false).await?;
                }
            }
            let name = name.replace("%2D", "-");
            self.fail_anchor(ev.request_id).await?;
            self.stop_hit = WatchName::new(name).ok();
            return Ok(());
        }
        if let Some((page_stream, websocket, url, last_event_id)) = stream_anchor(&ev.request.url) {
            // A popup's stream opens in its window, which its deliveries then address.
            let window = self.context_of_fetch(&ev.request_id);
            let previous = self.popup_id();
            if let Some(window) = window.as_deref().filter(|w| previous.as_deref() != Some(*w)) {
                match self.enter_context(window) {
                    Ok(()) => {}
                    // Gone before its anchor was handled: nothing to deliver to.
                    Err(ChromiumError::ContextClosed(_)) => return self.fail_anchor(ev.request_id).await,
                    Err(e) => return Err(e),
                }
            }
            let opened = self.open_stream(ev, t, page_stream, websocket, url, last_event_id).await;
            self.reenter(previous)?;
            return opened;
        }
        if let Some(binding) = self.select_request(&ev, t).await? {
            #[cfg(feature = "test-hooks")]
            if self.blocking && self.interrupt_synchronous_binding.as_deref() == Some(ev.request.url.as_str()) {
                self.interrupt_synchronous_binding = None;
                return Err(ChromiumError::Shape("test interrupted synchronous binding before delivery".into()));
            }
            // A round's answers wait until each of its requests is allocated. A document keeps its own
            // lifecycle, a preflight only exposes its request, and a synchronous one cannot wait.
            let preflight = matches!(binding.decision, Decision::Deliver { source: ResponseSource::Preflight, .. });
            match self.request_round.as_mut() {
                Some(round) if !self.blocking && !preflight && ev.resource_type != cnet::ResourceType::Document => {
                    round.selected.push((ev, binding));
                    #[cfg(feature = "test-hooks")]
                    if let Some(expression) = self.after_round_selection.take() {
                        let page = self.page.clone();
                        self.serviced(page.call(&runtime::EvaluateParams::new(expression))).await?;
                    }
                }
                _ => self.deliver_selected(ev, &binding).await?,
            }
        }
        Ok(())
    }

    /// Allocate a request's ordinal, rule decision and durable binding without delivering it.
    /// Control and stream anchors are handled separately by `handle_at`.
    async fn select_request(&mut self, ev: &fetch::RequestPausedEvent, t: VirtualTime) -> Result<Option<Binding>, ChromiumError> {
        let is_document = ev.resource_type == cnet::ResourceType::Document;
        if is_document && ev.frame_id == *self.launch_frame() {
            self.document_request = Some((ev.network_id.as_ref().map(|i| i.0.clone()).unwrap_or_default(), t));
            if self.event_stop.as_ref().is_some_and(|s| matches!(s.event, ProgressEvent::Navigation | ProgressEvent::Interception | ProgressEvent::Dialog)) {
                // A document stop has paused before delivery. Its observer report can arrive after
                // the interception, so use the resolved issue instant for the stop and rule deadline.
                // A zero-latency document must not wait for another grant while we await its commit.
                self.now = self.now.max(t);
            }
        }
        let headers = super::adoption::paused_headers(ev);
        let header = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
        let origin = header("origin");
        // Announced as a preflight (`requestWillBeSent`), or recognizable as one without that event (adopted
        // after a fault): an OPTIONS carrying `Access-Control-Request-Method`, a header page script cannot set.
        let announced = ev.network_id.as_ref().is_some_and(|id| self.net.preflights.remove(&id.0));
        let preflight = announced || (ev.request.method == "OPTIONS" && header("access-control-request-method").is_some());
        // A preflight is a browser mechanism, not an application request. It is decided by the method and
        // URL of the request it asks about (it carries neither that request's headers nor its body) and
        // never takes a sequence step.
        let ordinal = self.net.ordinal();
        interception_fault_point(&format!("before_request_selection:{}", ev.request.url));
        if self.blocking { interception_fault_point("blocking_before_selection"); }
        let selected = select_request_decision(&mut self.net, ev, t, preflight, self.progressing, &ordinal)?;
        let context = self.context_of_fetch(&ev.request_id);
        if context.is_some() {
            // A popup's opening precedes its requests.
            self.record_contexts();
        }
        self.record(t, EventKind::Request(RequestRecord { rules_revision: self.net.rules_revision, id: ordinal.clone(), method: ev.request.method.clone(), url: ev.request.url.clone(), rule: Some(selected.rule.clone()), entry: selected.entry, context, body: RequestBody::of(ev.request.post_data.as_deref(), ev.request.has_post_data) }));
        if let Some(q) = self.quiet.as_mut() {
            q.begin(&format!("request:{ordinal}"), t);
        }
        if let Some(taint) = selected.taint { self.record(t, EventKind::Taint(taint)); }
        self.bind_request(ev, &ordinal, t, &selected.rule, selected.entry, origin, selected.decision).await
    }

    /// Apply a selected decision without matching rules or consuming a sequence/replay position again.
    pub(super) async fn deliver_selected(&mut self, ev: fetch::RequestPausedEvent, binding: &Binding) -> Result<(), ChromiumError> {
        let t = VirtualTime(binding.issued_us);
        match &binding.decision {
            Decision::Deliver { spec, source: ResponseSource::Preflight, .. } => {
                self.fulfill(&binding.ordinal, ev.request_id, &binding.rule, spec, ResponseSource::Preflight, binding.origin.as_deref()).await
            }
            Decision::Deliver { spec, source, requires_progress, .. } => {
                let is_document = ev.resource_type == cnet::ResourceType::Document;
                let origin = if *source == ResponseSource::Replay { None } else { binding.origin.clone() };
                self.deliver_mock(ev, binding.ordinal.clone(), &binding.rule, binding.rules_revision,
                    spec.clone(), *source, t, is_document, origin, *requires_progress).await
            }
            Decision::Hold => {
                if !self.blocking { self.retain_shared_prefix_hold(&ev); }
                let network_id = ev.network_id.as_ref().map(|i| i.0.clone());
                self.net.held.insert(binding.ordinal.clone(), Held {
                    fetch_id: ev.request_id, rule: binding.rule.clone(), rules_revision: binding.rules_revision,
                    latency_us: None, released: false, document: binding.document, network_id,
                    attempted: None, origin: binding.origin.clone(), method: binding.method.clone(), url: binding.url.clone(),
                    body: ev.request.post_data.clone(), body_unavailable: ev.request.post_data.is_none() && ev.request.has_post_data == Some(true), blocks: self.blocking,
                });
                // A genuinely synchronous hold suspends its running operation.
                if self.blocking {
                    self.subs.holds.set(Some(BlockingHold {
                        ordinal: binding.ordinal.clone(), method: binding.method.clone(), url: binding.url.clone(), rule: binding.rule.clone(),
                        rules_revision: binding.rules_revision, t_us: t,
                    }));
                } else {
                    self.release_held(&binding.ordinal).await;
                }
                Ok(())
            }
            Decision::Passthrough => {
                let headers = super::adoption::paused_headers(&ev);
                let is_document = ev.resource_type == cnet::ResourceType::Document;
                self.pass_through(ev, binding.ordinal.clone(), &binding.rule, &headers, is_document, t).await
            }
            Decision::Fail { reason } => self.fail(&binding.ordinal, ev.request_id, &binding.rule, *reason).await,
            Decision::Intercept { .. } => self.retain_interception(ev, binding).await,
        }
    }

    /// Ends a paused request whose outcome needs no record (a stream's anchor, recorded already; a
    /// navigation the page replaced): one the engine no longer knows (its document left, or the browser
    /// cancelled it, meanwhile) needs nothing more.
    pub(super) async fn fail_anchor(&mut self, id: fetch::RequestId) -> Result<(), ChromiumError> {
        match self.fetch_session(&id).call(&fetch::FailRequestParams::new(id.clone(), cnet::ErrorReason::Aborted)).await {
            Err(e) if is_gone(&e) => Ok(()),
            r => {
                r?;
                self.receipts_ready.push(id.0);
                Ok(())
            }
        }
    }

    /// Continues a request to the network, through the proxy, named by its ordinal so the proxy's
    /// exchange is this request's.
    pub(super) async fn pass_through(&mut self, ev: fetch::RequestPausedEvent, ordinal: String, rule: &RuleId, headers: &BTreeMap<String, String>, is_document: bool, t: VirtualTime) -> Result<(), ChromiumError> {
        self.dispatching(&ordinal, &ev.request_id, Resolution::Continue)?;
        let intercept_response = (self.adopting && self.net.recording.contains_key(&ordinal)).then_some(true);
        if intercept_response.is_some() {
            self.net.response_stage.insert(ev.request_id.0.clone(), ordinal.clone());
        }
        let headers = self.proxy.as_ref().map(|_| {
            let mut h: Vec<fetch::HeaderEntry> = headers.clone().into_iter().map(|(name, value)| fetch::HeaderEntry { name, value }).collect();
            h.retain(|e| !e.name.eq_ignore_ascii_case(frontcap_proxy_header()));
            h.push(fetch::HeaderEntry { name: frontcap_proxy_header().into(), value: ordinal.clone() });
            h
        });
        let launch_document = is_document && ev.frame_id == *self.launch_frame();
        if let Some(net_id) = &ev.network_id {
            // Continued now: one issued during an `advance` segment and handled at its end reaches the page
            // no earlier than here.
            self.net.passthrough.insert(net_id.0.clone(), crate::network::PassedThrough { ordinal, fetch_id: ev.request_id.0.clone(), rule: rule.clone(), issued: t.max(self.now), moved: false });
            if launch_document {
                self.net.loading_documents.insert(net_id.0.clone());
            }
        }
        self.fetch_session(&ev.request_id).call(&fetch::ContinueRequestParams { headers, intercept_response, ..fetch::ContinueRequestParams::new(ev.request_id.clone()) }).await?;
        if launch_document { document_answered_fault_point(&ev.request.url); }
        Ok(())
    }

    /// Answers a request the platform makes the response for, at its issue time `t` plus its latency.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn deliver_mock(&mut self, ev: fetch::RequestPausedEvent, ordinal: String, rule: &RuleId, rules_revision: u64, spec: MockSpec, source: ResponseSource, t: VirtualTime, is_document: bool, origin: Option<String>, requires_progress: bool) -> Result<(), ChromiumError> {
        let due = t.saturating_add(VirtualDuration::from_ms(spec.latency_ms));
        if spec.latency_ms > 0 && is_document {
            return Err(ChromiumError::Unsupported(format!("latency on the document request {}", ev.request.url)));
        }
        if self.blocking {
            return self.deliver_arrival(ev, ordinal, rule, spec, source, origin, due).await;
        }
        if due <= self.now && (!requires_progress || self.progressing) {
            let answered = self.fulfill(&ordinal, ev.request_id, rule, &spec, source, origin.as_deref()).await;
            if let (Ok(()), Some(id)) = (&answered, &ev.network_id) {
                self.net.answered.insert(id.0.clone());
            }
            // The redirect chain is this navigation's handling: its next hop keeps the page's place.
            if answered.is_ok() && is_document && ev.frame_id != *self.launch_frame() && follows_redirect(&spec) {
                self.net.interceptions.navigation_redirected(&ev.frame_id.0);
            }
            if answered.is_ok() && is_document && ev.frame_id == *self.launch_frame() { document_answered_fault_point(&ev.request.url); }
            answered
        } else {
            self.net.agenda.schedule(due, Due::Response(Delivery { ordinal: ordinal.clone(), spec: spec.clone(), source, requires_progress }));
            let network_id = ev.network_id.as_ref().map(|i| i.0.clone());
            let latency_us = Some(spec.latency_ms.saturating_mul(1000));
            self.retain_shared_prefix_hold(&ev);
            self.net.held.insert(ordinal.clone(), Held { fetch_id: ev.request_id, rule: rule.clone(), rules_revision, latency_us, released: false, document: self.documents, network_id, attempted: None, origin, method: ev.request.method.clone(), url: ev.request.url.clone(), body: ev.request.post_data.clone(), body_unavailable: ev.request.post_data.is_none() && ev.request.has_post_data == Some(true), blocks: false });
            self.release_held(&ordinal).await;
            Ok(())
        }
    }

    /// Answers a request the page's main thread waits for, due at `due`. The page cannot run before it
    /// arrives, so it is fulfilled now with its arrival instant, which the engine applies before the page
    /// sees the response (engine patch 53); the clock moves there, with nothing else delivered between.
    #[allow(clippy::too_many_arguments)]
    async fn deliver_arrival(&mut self, ev: fetch::RequestPausedEvent, ordinal: String, rule: &RuleId, spec: MockSpec, source: ResponseSource, origin: Option<String>, due: VirtualTime) -> Result<(), ChromiumError> {
        let due = due.max(self.now);
        let waits = due > self.now;
        if waits && self.shared_time.is_some() {
            self.shared_arrival(&ev.request_id, due).await?;
        }
        if waits {
            self.clock_record(ClockOp::SynchronousRequest, ClockPhase::Start, due, None, None);
            let shared = self.shared_time.is_some();
            if !shared { self.now = due; }
        }
        let answered = self.fulfill_at(&ordinal, ev.request_id, rule, &spec, source, origin.as_deref(), waits.then_some(due)).await;
        let shared = self.shared_time.is_some();
        if waits && !shared {
            self.clock_record(ClockOp::SynchronousRequest, ClockPhase::End, due, Some(ClockOutcome::Reached), None);
            self.arrived = true;
        }
        if let (Ok(()), Some(id)) = (&answered, &ev.network_id) {
            self.net.answered.insert(id.0.clone());
        }
        answered
    }

    /// Answers a request with `spec`. For a cross-origin request (`origin`), a response without CORS
    /// headers gets those of a server that allows that origin, credentials included.
    pub(super) async fn fulfill(&mut self, ordinal: &str, fetch_id: fetch::RequestId, rule: &RuleId, spec: &MockSpec, source: ResponseSource, origin: Option<&str>) -> Result<(), ChromiumError> {
        self.fulfill_at(ordinal, fetch_id, rule, spec, source, origin, None).await
    }

    /// `fulfill`, naming the response's arrival for a request the main thread waits for.
    #[allow(clippy::too_many_arguments)]
    async fn fulfill_at(&mut self, ordinal: &str, fetch_id: fetch::RequestId, rule: &RuleId, spec: &MockSpec, source: ResponseSource, origin: Option<&str>, arrival: Option<VirtualTime>) -> Result<(), ChromiumError> {
        self.dispatching(ordinal, &fetch_id, Resolution::Fulfill { status: spec.status, body_sha256: crate::bindings::body_sha256(spec) })?;
        self.fetch_session(&fetch_id)
            .call(&fulfillment(fetch_id.clone(), spec, source, origin, arrival, self.profile.initial_date_ms))
            .await?;
        if arrival.is_some() && self.shared_time.is_some() {
            return self.finish_shared_arrival(fetch_id, ResponseRecord {
                id: ordinal.to_string(), status: spec.status, source, rule: Some(rule.clone()),
            }).await;
        }
        self.record(self.now, EventKind::Response(ResponseRecord { id: ordinal.to_string(), status: spec.status, source, rule: Some(rule.clone()) }));
        self.receipts_awaiting.push((ordinal.to_string(), fetch_id.0.clone()));
        if let Some(q) = self.quiet.as_mut() {
            q.end(&format!("request:{ordinal}"), self.now);
        }
        Ok(())
    }

    pub(super) async fn fail(&mut self, ordinal: &str, fetch_id: fetch::RequestId, rule: &RuleId, reason: FailReason) -> Result<(), ChromiumError> {
        self.dispatching(ordinal, &fetch_id, Resolution::Fail { reason })?;
        self.fetch_session(&fetch_id).call(&fetch::FailRequestParams::new(fetch_id.clone(), error_reason(reason))).await?;
        self.record(self.now, EventKind::Response(ResponseRecord { id: ordinal.to_string(), status: 0, source: ResponseSource::Fail, rule: Some(rule.clone()) }));
        self.receipts_awaiting.push((ordinal.to_string(), fetch_id.0));
        if let Some(q) = self.quiet.as_mut() {
            q.end(&format!("request:{ordinal}"), self.now);
        }
        Ok(())
    }

    pub(super) async fn deliver_due(&mut self) -> Result<(), ChromiumError> {
        // At one instant, responses first, then scripted stream events, then the server's (replayed) ones:
        // the order a recording run delivers them in (`run_until`).
        let mut popped = self.net.agenda.pop_due(self.now);
        popped.sort_by_key(|(t, d)| (*t, match d {
            Due::Response(_) => 0,
            Due::Stream(s) if !s.from_server => 1,
            Due::Stream(_) => 2,
        }));
        for (due, d) in popped {
            let d = match d {
                Due::Stream(e) => {
                    if self.progressing && (!self.net.answered.is_empty() || !self.net.passthrough.is_empty()) {
                        self.net.agenda.schedule(due, Due::Stream(e));
                    } else {
                        self.deliver_stream(&e, self.now).await?;
                    }
                    continue;
                }
                Due::Response(d) => d,
            };
            if d.requires_progress && !self.progressing {
                self.net.agenda.schedule(due, Due::Response(d));
                continue;
            }
            if due < self.now {
                let detail = format!("{} was due at {} ms and delivered at {} ms", d.ordinal, due.0 / 1000, self.now.0 / 1000);
                self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::LateDelivery, detail }));
            }
            // A closed window can lose its worker's loadingFailed notification.
            // Its detached Fetch session cannot receive the scheduled response.
            // An ambiguous prior dispatch still requires receipt recovery.
            let closed = self.net.held.get(&d.ordinal).is_some_and(|held| {
                held.attempted.is_none()
                    && self.context_of_fetch(&held.fetch_id).is_some_and(|context|
                        self.contexts.lock().unwrap().popups.iter()
                            .any(|popup| popup.id == context && popup.closed))
            });
            if closed {
                self.end_hold(&d.ordinal);
                continue;
            }
            if self.net.held.contains_key(&d.ordinal)
                && let Err(e) = self.answer_held(&d.ordinal, Answer::Fulfill(&d.spec, d.source)).await
            {
                // The engine no longer knows the interception: the request is gone (aborted, or its
                // document replaced, before that was seen) — the hold ends.
                if is_rejection(&e) && self.net.held.get(&d.ordinal).is_some_and(|h| h.attempted.is_none()) {
                    self.end_hold(&d.ordinal);
                    continue;
                }
                return Err(e);
            }
        }
        Ok(())
    }

    /// Answers a held request. The hold ends only when the answer is known to be applied: on success, on
    /// a rejected retry of an answer whose first call was ambiguous, or (`drain_network`) when the
    /// request completes after such a call.
    pub(super) async fn answer_held(&mut self, ordinal: &str, answer: Answer<'_>) -> Result<(), ChromiumError> {
        let commits_document = matches!(&answer, Answer::Fulfill(..)) && self.interception_stops.get(ordinal)
            .is_some_and(|(ev, b)| b.document_request && ev.frame_id == *self.launch_frame());
        let (documents, within) = (self.documents, self.within_document);
        // One the main thread waits for was never released, and the page could not be asked.
        let blocks = self.net.held.get(ordinal).is_some_and(|h| h.blocks);
        if !blocks {
            self.reengage_held(ordinal).await?;
        }
        let h = self.net.held.get(ordinal).ok_or_else(|| ChromiumError::NoSuchHold(ordinal.into()))?;
        let (fetch_id, rule, earlier, origin) = (h.fetch_id.clone(), h.rule.clone(), h.attempted.clone(), h.origin.clone());
        let (result, record) = match answer {
            Answer::Fulfill(spec, source) => (
                self.fulfill(ordinal, fetch_id, &rule, spec, source, origin.as_deref()).await,
                ResponseRecord { id: ordinal.to_string(), status: spec.status, source, rule: Some(rule.clone()) },
            ),
            Answer::Fail(reason) => (
                self.fail(ordinal, fetch_id, &rule, reason).await,
                ResponseRecord { id: ordinal.to_string(), status: 0, source: ResponseSource::Fail, rule: Some(rule.clone()) },
            ),
        };
        match result {
            Ok(()) => {
                if let Some(id) = self.net.held.remove(ordinal).and_then(|h| h.network_id) {
                    self.net.answered.insert(id);
                }
                self.interception_stops.remove(ordinal);
                if commits_document {
                    self.await_commit(documents, within).await?;
                    self.drain().await?;
                }
                Ok(())
            }
            Err(e) if is_rejection(&e) && earlier.is_some() => {
                // The engine no longer knows the interception: the earlier, ambiguous answer took effect.
                self.resolve_attempted(ordinal);
                Ok(())
            }
            Err(e) => {
                if !is_rejection(&e)
                    && let Some(h) = self.net.held.get_mut(ordinal)
                {
                    h.attempted = Some(record);
                }
                // Rejected, it stays held: released again, it holds no virtual time.
                if is_rejection(&e) && !blocks {
                    self.release_held(ordinal).await;
                }
                Err(e)
            }
        }
    }

    /// Engine patch 1: a request the platform holds stops counting toward `pauseIfNetworkFetchesPending`,
    /// so virtual time runs under that policy while it is held and a new fetch still stops it at its issue.
    /// A worker's request is released on the worker's session (`set_request_pauses`). One the engine
    /// cannot release (without the patch, or a worker's whose session does not answer) keeps the
    /// `advance` segments.
    pub(super) async fn release_held(&mut self, ordinal: &str) {
        if self.net.held.get(ordinal).is_some_and(|h| self.shared_prefix_never_released(&h.fetch_id)) { return; }
        let Some(id) = self.net.held.get(ordinal).and_then(|h| h.network_id.clone()).filter(|_| self.patched) else { return };
        let released = self.set_request_pauses(id, false).await.is_ok();
        if let Some(h) = self.net.held.get_mut(ordinal) {
            h.released = released;
        }
    }

    /// Engine patch 9: a worker blocked in a synchronous load holds virtual time until the platform has seen
    /// each of its other intercepted loads (one may be intercepted after the load it waits for). Any call on a
    /// load says so; re-engaging one that was not released changes nothing. Only while workers exist.
    pub(super) async fn mark_seen(&mut self, ev: &fetch::RequestPausedEvent) {
        // The page cannot answer while its main thread waits; it has no worker load to hold for it then.
        if self.blocking { return }
        if self.shared_request_owner_waiting(&ev.request_id)
            && !ev.network_id.as_ref().is_some_and(|id| self.worker_request_session(&id.0).is_some()) { return; }
        let Some(id) = ev.network_id.as_ref().filter(|_| self.patched) else { return };
        if self.subs.workers.lock().unwrap().sessions.is_empty() {
            return;
        }
        let _ = self.set_request_pauses(id.0.clone(), true).await;
    }

    pub(super) fn worker_request_session(&self, network: &str) -> Option<String> {
        let mut books = vec![self.subs.workers.clone()];
        books.extend(self.contexts.lock().unwrap().popups.iter().filter(|p| !p.closed).map(|p| p.workers.clone()));
        books.iter().find_map(|book| book.lock().unwrap().requests.get(network).cloned())
    }

    /// `Network.setRequestPausesVirtualTime` where the load is: the page's, or else a worker's (its own
    /// request, which the page's interception holds). The page's rejection when no session has it.
    pub(super) async fn set_request_pauses(&self, id: String, pauses: bool) -> Result<(), ChromiumError> {
        let params = cnet::SetRequestPausesVirtualTimeParams::new(cnet::RequestId(id), pauses);
        // Reconnect may retain Fetch ownership without replaying the worker's
        // original Network announcement. Its page owns the synchronous-load
        // registry even while the worker cannot answer inspector commands.
        let session = self.net.held.values().find(|h| h.network_id.as_deref() == Some(&params.request_id.0))
            .map(|h| self.fetch_session(&h.fetch_id)).unwrap_or_else(|| self.network_session(&params.request_id.0));
        // A runnable page can service the process-wide synchronous-worker registry.
        // Only bypass it when a native main-thread arrival makes it unable to reply.
        if self.shared_session_waiting(&session.id)
            && let Some(worker) = self.worker_request_session(&params.request_id.0)
        {
            let mut books = vec![self.subs.workers.clone()];
            books.extend(self.contexts.lock().unwrap().popups.iter().map(|p| p.workers.clone()));
            let waiting = books.iter().any(|book| book.lock().unwrap().sessions.get(&worker)
                .is_some_and(|state| state.synchronous_wait.is_some()));
            if waiting {
                return self.set_shared_worker_request_pauses(&session.id, &params.request_id.0, pauses).await;
            }
            return self.page.client.call(Some(&worker), &params).await.map(|_| ()).map_err(Into::into);
        }
        // A popup's own request on its session.
        let first = match session.call(&params).await {
            Ok(_) => return Ok(()),
            Err(e) if is_rejection(&e) => e,
            Err(e) => return Err(e),
        };
        // The worker that issued it, or else any (its request may not be recorded yet): the page's, a popup's.
        let workers: Vec<String> = {
            let mut all = vec![self.subs.workers.clone()];
            all.extend(self.contexts.lock().unwrap().popups.iter().filter(|p| !p.closed).map(|p| p.workers.clone()));
            let issuer = all.iter().find_map(|w| w.lock().unwrap().requests.get(&params.request_id.0).cloned());
            match issuer {
                Some(s) => vec![s],
                None => all.iter().flat_map(|w| w.lock().unwrap().sessions.keys().cloned().collect::<Vec<_>>()).collect(),
            }
        };
        for w in workers {
            // Bounded: a worker that cannot answer (its thread blocked, its session gone) is not waited on.
            if let Ok(Ok(_)) = tokio::time::timeout(Session::CALL_TIMEOUT, self.page.client.call(Some(&w), &params)).await {
                return Ok(());
            }
        }
        Err(first)
    }

    /// Before the platform answers a held request, it counts again: the response is processed before
    /// virtual time moves on, with the engine's completion step counted from the request's issue. Asked
    /// whenever the engine may have released it (a release whose answer was lost included; the engine
    /// ignores a request it did not release). An answer is sent only once that is known: a request with no
    /// load left (the page aborted it) is rejected, and its answer fails as before.
    pub(super) async fn reengage_held(&mut self, ordinal: &str) -> Result<(), ChromiumError> {
        // Only an explicitly retained, never-dispatched release can omit this
        // renderer command. A failed/ambiguous release is not equivalent evidence.
        if self.net.held.get(ordinal).is_some_and(|h| self.shared_prefix_never_released(&h.fetch_id)) { return Ok(()); }
        let Some(id) = self.net.held.get(ordinal).filter(|_| self.patched).and_then(|h| h.network_id.clone()) else { return Ok(()) };
        match self.set_request_pauses(id, true).await {
            Ok(_) => {}
            Err(e) if is_rejection(&e) => {}
            Err(e) => return Err(e),
        }
        if let Some(h) = self.net.held.get_mut(ordinal) {
            h.released = false;
        }
        Ok(())
    }

    pub(super) fn resolve_attempted(&mut self, ordinal: &str) {
        let Some(h) = self.net.held.remove(ordinal) else { return };
        self.receipts_awaiting.push((ordinal.to_string(), h.fetch_id.0.clone()));
        if let Some(record) = h.attempted {
            self.record(self.now, EventKind::Response(record));
        }
        if let Some(q) = self.quiet.as_mut() {
            q.end(&format!("request:{ordinal}"), self.now);
        }
    }

    pub(super) async fn drain_network(&mut self) -> Result<(), ChromiumError> {
        while let Some(ev) = self.subs.response.next_timeout(Duration::ZERO).await? {
            self.net.status.insert(ev.request_id.0.clone(), ev.response.status);
            // A response a `record` rule keeps, as the page received it.
            if self.net.passthrough.get(&ev.request_id.0).is_some_and(|p| self.net.recording.contains_key(&p.ordinal)) {
                self.net.response_meta.insert(ev.request_id.0.clone(), (ev.response.status as u16, engine_headers(&ev.response.headers)));
            }
            self.check_transport(&ev);
        }
        let mut done = Vec::new();
        while let Some(ev) = self.subs.finished.next_timeout(Duration::ZERO).await? {
            done.push((ev.request_id.0, true));
        }
        while let Some(ev) = self.subs.failed.next_timeout(Duration::ZERO).await? {
            done.push((ev.request_id.0, false));
        }
        if self.blocking || self.collecting_requests {
            // Completed unbound loads no longer wait for an interception. Keep
            // response/body bookkeeping until renderer commands can reply again
            // and until request collection no longer owns the response boundary.
            for (id, _) in &done {
                if self.net.anchors.contains_key(id) { self.net.interceptions.end_waiting(id); }
                else { self.net.interceptions.ended(id); }
            }
            self.deferred_network_completions.extend(done);
            return Ok(());
        }
        let previous = std::mem::take(&mut self.deferred_network_completions);
        for (net_id, ok) in previous.into_iter().chain(done) {
            // A stream anchor ended before its interception keeps its place: it is decided there without one.
            if self.net.anchors.contains_key(&net_id) && self.net.interceptions.end_waiting(&net_id) {
                continue;
            }
            self.net.anchors.remove(&net_id);
            self.net.interceptions.ended(&net_id);
            self.net.answered.remove(&net_id);
            let attempted = self.net.held.iter().find(|(_, h)| h.attempted.is_some() && h.network_id.as_deref() == Some(net_id.as_str())).map(|(o, _)| o.clone());
            if let Some(ordinal) = attempted {
                self.resolve_attempted(&ordinal);
                continue;
            }
            if !ok && self.end_held(&net_id) {
                continue;
            }
            if ok {
                self.record_from_engine(&net_id).await;
            }
            self.end_passthrough(&net_id, ok);
        }
        Ok(())
    }

    /// A response from the network must have come through the proxy; any other remote address is a
    /// transport-boundary defect (network-rules.md). One with no remote address came over no connection:
    /// the platform answered it (a frame's mocked document reports an empty address and port 0).
    pub(super) fn check_transport(&mut self, ev: &cnet::ResponseReceivedEvent) {
        let Some(route) = &self.proxy else { return };
        let (Some(ip), Some(port)) = (ev.response.remote_ip_address.as_deref(), ev.response.remote_port) else { return };
        let ip = ip.trim_start_matches('[').trim_end_matches(']');
        if ip.is_empty() {
            return;
        }
        if ip.parse::<std::net::IpAddr>().ok() == Some(route.addr.ip()) && port == i64::from(route.addr.port()) {
            return;
        }
        let detail = format!("{} came from {ip}:{port}, not the proxy {}", ev.response.url, route.addr);
        self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::TransportBypass, detail }));
    }

    /// Appends the response a `record` rule's request received to its cassette, as the page received
    /// it: status and headers from the engine, the body decoded (`Network.getResponseBody`).
    pub(super) async fn record_from_engine(&mut self, net_id: &str) {
        let Some((ordinal, at)) = self.net.passthrough.get(net_id).map(|p| (p.ordinal.clone(), p.answered_at(self.now))).filter(|(o, _)| self.net.recording.contains_key(o)) else { return };
        let Some((status, headers)) = self.net.response_meta.remove(net_id) else { return };
        let body = match self.network_session(net_id).call(&cnet::GetResponseBodyParams::new(cnet::RequestId(net_id.to_string()))).await {
            Ok(b) if b.base64_encoded => frontcap_core::encode::unbase64(&b.body),
            Ok(b) => Some(b.body.into_bytes()),
            // A response with no body (a preflight's 204, a 304) has none to give.
            Err(_) if matches!(status, 204 | 304) => Some(Vec::new()),
            Err(_) => None,
        };
        match body {
            Some(body) => self.write_cassette_line(&ordinal, status, headers, body, at),
            None => {
                self.net.recording.remove(&ordinal);
                let detail = format!("{ordinal}: the engine kept no body to record");
                self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::CassetteMiss, detail }));
            }
        }
    }

    /// Appends one line to a recorded request's cassette (`cassettes/<name>.jsonl`), synced: a later run
    /// replays from it.
    /// `answered` is the instant the response reached the page: its latency in virtual time is from the
    /// issue to then.
    pub(super) fn write_cassette_line(&mut self, ordinal: &str, status: u16, headers: BTreeMap<String, String>, body: Vec<u8>, answered: VirtualTime) {
        let Some((name, key, issued, issued_while_stopped)) = self.net.recording.remove(ordinal) else { return };
        let line = frontcap_core::cassette::Line {
            key,
            requires_progress: issued_while_stopped && self.progressing,
            response: frontcap_core::cassette::Response { status, headers, body_base64: base64(&body) },
            latency: frontcap_core::cassette::Latency { virtual_ms: answered.since(issued.min(answered)).as_micros() / 1000, wall_ms: 0 },
        };
        let dir = self.bundle.root().join("cassettes");
        let path = dir.join(format!("{name}.jsonl"));
        let written = std::fs::create_dir_all(path.parent().expect("cassette parent")).and_then(|_| {
            use std::io::Write as _;
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
            f.write_all((line.to_line() + "\n").as_bytes())?;
            f.sync_data()
        });
        if let Err(err) = written {
            let detail = format!("{ordinal}: the cassette {name} could not be written: {err}");
            self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::CassetteMiss, detail }));
        }
    }

    /// Records the end of a passed-through request: its response, or a failure.
    pub(super) fn end_passthrough(&mut self, net_id: &str, ok: bool) {
        self.net.loading_documents.remove(net_id);
        let Some(p) = self.net.passthrough.remove(net_id) else { return };
        let at = p.answered_at(self.now);
        let ordinal = p.ordinal;
        self.receipts_awaiting.push((ordinal.clone(), p.fetch_id.clone()));
        if !ok {
            self.net.recording.remove(&ordinal);
        }
        let status = if ok { self.net.status.remove(net_id).unwrap_or(0) as u16 } else { 0 };
        let source = if ok { ResponseSource::Passthrough } else { ResponseSource::Fail };
        self.record(at, EventKind::Response(ResponseRecord { id: ordinal.clone(), status, source, rule: Some(p.rule) }));
        if ok {
            self.record(at, EventKind::Taint(TaintRecord { source: TaintSource::PassthroughContent, detail: ordinal.clone() }));
        }
        for (t, message) in self.net.deferred_console.remove(net_id).unwrap_or_default() {
            self.record(t.max(at), EventKind::Console(message));
        }
        if let Some(q) = self.quiet.as_mut() {
            q.end(&format!("request:{ordinal}"), at);
        }
    }

    /// A held request whose load ended without an answer (the page aborted it, or its document went): the
    /// hold and its due delivery end, recorded as failed. Returns whether `net_id` was held.
    pub(super) fn end_held(&mut self, net_id: &str) -> bool {
        let Some(ordinal) = self.net.held.iter().find(|(_, h)| h.attempted.is_none() && h.network_id.as_deref() == Some(net_id)).map(|(o, _)| o.clone()) else { return false };
        self.end_hold(&ordinal);
        true
    }

    /// Ends a hold whose request is gone without an answer, recorded as failed.
    pub(super) fn end_hold(&mut self, ordinal: &str) {
        self.interception_stops.remove(ordinal);
        let Some(h) = self.net.held.remove(ordinal) else { return };
        let ordinal = ordinal.to_string();
        self.receipts_awaiting.push((ordinal.clone(), h.fetch_id.0.clone()));
        self.net.agenda.cancel_where(|d| matches!(d, Due::Response(r) if r.ordinal == ordinal));
        self.record(self.now, EventKind::Response(ResponseRecord { id: ordinal.clone(), status: 0, source: ResponseSource::Fail, rule: Some(h.rule) }));
        if let Some(q) = self.quiet.as_mut() {
            q.end(&format!("request:{ordinal}"), self.now);
        }
    }

    /// How many held requests the engine has released (engine patch 1).
    pub fn held_released(&self) -> usize {
        self.net.held.values().filter(|h| h.released).count()
    }

    /// The requests held for the agent.
    pub fn held_requests(&self) -> Vec<(String, &crate::network::Held)> {
        self.net.held.iter().filter(|(_, h)| h.latency_us.is_none()).map(|(k, h)| (k.clone(), h)).collect()
    }

    pub fn held(&self) -> Vec<String> {
        self.net.held.iter().filter(|(_, h)| h.latency_us.is_none()).map(|(k, _)| k.clone()).collect()
    }

    /// Answers a held request. The hold is released only once the engine has accepted the response, so
    /// a rejected one leaves it to be answered again.
    pub async fn respond(&mut self, ordinal: &str, spec: MockSpec) -> Result<(), ChromiumError> {
        self.alive()?;
        spec.validate().map_err(ChromiumError::InvalidArgument)?;
        if self.net.held_streams.contains_key(ordinal) {
            if !spec.body.is_empty() || spec.body_base64.is_some() || spec.latency_ms != 0 {
                return Err(ChromiumError::InvalidArgument(format!("{ordinal} is a stream: its handshake takes a status only; send its messages with send")));
            }
            return self.answer_held_stream(ordinal, Some(spec.status)).await;
        }
        // A latency belongs to a rule, which schedules its answer; the agent answers now.
        if spec.latency_ms != 0 {
            return Err(ChromiumError::InvalidArgument("an answer to a held request is given at the current instant: latency_ms must be 0 (advance first for a later answer)".into()));
        }
        self.answer_held(ordinal, Answer::Fulfill(&spec, ResponseSource::Agent)).await?;
        self.drain().await?;
        self.commit()
    }

    pub async fn fail_held(&mut self, ordinal: &str, reason: FailReason) -> Result<(), ChromiumError> {
        self.alive()?;
        if self.net.held_streams.contains_key(ordinal) {
            return self.answer_held_stream(ordinal, None).await;
        }
        self.answer_held(ordinal, Answer::Fail(reason)).await?;
        self.drain().await?;
        self.commit()
    }
}


impl Generation {
    #[allow(clippy::too_many_arguments)]
    async fn bind_request(&mut self, ev: &fetch::RequestPausedEvent, ordinal: &str, t: VirtualTime,
        rule: &RuleId, entry: Option<u64>, origin: Option<String>, decision: Decision) -> Result<Option<Binding>, ChromiumError>
    {
        // A request the main thread waits for has no serviceable stop before its answer: the page is in the
        // middle of a task.
        // A popup stop holds the window's first document request: the window is open, its document pending.
        let popup_document = ev.resource_type == cnet::ResourceType::Document
            // A window without an opener has no frame to answer meanwhile: it is admitted committed.
            && self.contexts.lock().unwrap().popups.iter().any(|p| p.target_id == ev.frame_id.0 && !p.no_opener);
        if popup_document {
            self.popup_navigations.insert(ev.frame_id.0.clone());
        }
        let stopped = !self.blocking && self.event_stop.as_ref().is_some_and(|s| s.event == ProgressEvent::Interception
            || (s.event == ProgressEvent::Popup && popup_document));
        if stopped { interception_fault_point("before_bind"); }
        let decision = if stopped { Decision::Intercept { then: Box::new(decision) } } else { decision };
        if self.blocking { interception_fault_point("blocking_before_binding"); }
        let binding = self.bind(ev, ordinal, t, rule, entry, origin, decision)?;
        if stopped {
            interception_fault_point("after_bind");
            self.retain_interception(ev.clone(), &binding).await?;
            interception_fault_point("after_retention");
            self.record(t, EventKind::Interception(InterceptionRecord {
                ordinal: ordinal.into(), method: ev.request.method.clone(), url: ev.request.url.clone(),
            }));
        }
        Ok((!stopped).then_some(binding))
    }

    pub(super) async fn retain_interception(&mut self, ev: fetch::RequestPausedEvent, b: &Binding) -> Result<(), ChromiumError> {
        // Renderer commands keep addressing the current document while the new one is held (engine patch
        // 33): the launch page's, or a popup's (its window's first document at a popup stop).
        let popup_document = ev.resource_type == cnet::ResourceType::Document
            // A window without an opener has no frame to answer meanwhile: it is admitted committed.
            && self.contexts.lock().unwrap().popups.iter().any(|p| p.target_id == ev.frame_id.0 && !p.no_opener);
        if (b.document_request && ev.frame_id == *self.launch_frame()) || popup_document {
            self.fetch_session(&ev.request_id).call(&fetch::FrontcapInspectNavigationParams::new(ev.request_id.clone())).await?;
        }
        self.net.held.insert(b.ordinal.clone(), Held {
            fetch_id: ev.request_id.clone(), rule: b.rule.clone(), rules_revision: b.rules_revision, latency_us: None,
            released: false, document: b.document, network_id: b.network_id.clone(),
            attempted: None, origin: b.origin.clone(), method: b.method.clone(), url: b.url.clone(),
            body: ev.request.post_data.clone(), body_unavailable: ev.request.post_data.is_none() && ev.request.has_post_data == Some(true),
            blocks: false,
        });
        self.interception_stops.insert(b.ordinal.clone(), (ev, b.clone()));
        Ok(())
    }

    pub(super) async fn resume_interceptions(&mut self) -> Result<(), ChromiumError> {
        let mut stopped: Vec<_> = std::mem::take(&mut self.interception_stops).into_values().collect();
        stopped.sort_by_key(|(_, b)| (b.issued_us, super::adoption::ordinal_number(&b.ordinal)));
        for (ev, b) in stopped {
            if self.has_response(&b.ordinal)? { continue; }
            self.bindings.append(&BindingEntry::InterceptionReleased {
                target_id: b.target_id.clone(), fetch_id: b.fetch_id.clone(),
            })?;
            self.released_interceptions.insert(b.fetch_id.clone());
            self.net.held.remove(&b.ordinal);
            self.restore_bound(ev, &b).await?;
        }
        Ok(())
    }
}

/// Whether the browser follows `spec` as a redirect to another network request: a redirect status with
/// a `Location` that is relative (the request's own scheme) or names `http`/`https`.
fn follows_redirect(spec: &MockSpec) -> bool {
    if !matches!(spec.status, 301 | 302 | 303 | 307 | 308) {
        return false;
    }
    let Some(location) = spec.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("location")).map(|(_, v)| v.trim()) else {
        return false;
    };
    let scheme = location.split_once(':').map(|(s, _)| s).filter(|s| {
        s.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    });
    !location.is_empty() && scheme.is_none_or(|s| s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https"))
}

/// The launch page's document request `url` has been answered (continued or fulfilled), and nothing of
/// its commit is recorded yet.
fn document_answered_fault_point(url: &str) {
    #[cfg(feature = "fault-injection")]
    interception_fault_point(&format!("after_document_answer:{}", path_and_query(url)));
    #[cfg(not(feature = "fault-injection"))]
    let _ = url;
}

/// A URL without its scheme and authority (`/page.html?query`), so fault points name a page apart from
/// the test server's address.
#[cfg(feature = "fault-injection")]
fn path_and_query(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.find('/').map_or("/", |at| &rest[at..])
}

pub(super) fn interception_fault_point(point: &str) {
    #[cfg(feature = "fault-injection")]
    if std::env::var("FRONTCAP_INTERCEPTION_FAULT").is_ok_and(|wanted| wanted == point) {
        std::process::abort();
    }
    #[cfg(not(feature = "fault-injection"))]
    let _ = point;
}

/// The shared route/body match for live selection and pre-renderer held-request admission.
pub(super) fn request_rule(rules: &RuleSet, ev: &fetch::RequestPausedEvent, preflight: bool) -> frontcap_core::rules::Rule {
    let headers = super::adoption::paused_headers(ev);
    let header = |name: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
    if preflight {
            let method = header("access-control-request-method").unwrap_or_else(|| "GET".into());
            rules.first_route_match(&method, &ev.request.url).cloned()
        } else {
            let json: Option<serde_json::Value> = ev.request.post_data.as_deref().and_then(|b| serde_json::from_str(b).ok());
            let view = RequestView { method: &ev.request.method, url: &ev.request.url, headers: &headers, body: json.as_ref() };
            rules.first_match(&view).cloned()
        }
        .expect("effective rule set ends with default-block")

}
