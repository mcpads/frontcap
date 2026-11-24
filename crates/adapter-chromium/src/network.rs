use cdp::Arrival;
use frontcap_core::agenda::Agenda;
use frontcap_core::rules::{MockSpec, RuleSet, SequenceCursors};
use frontcap_core::time::{VirtualDuration, VirtualTime};
use std::collections::{BTreeMap, BTreeSet};

/// A request held at the `Fetch` request stage.
pub struct Held {
    pub fetch_id: cdp::protocol::fetch::RequestId,
    pub rule: frontcap_core::ids::RuleId,
    pub rules_revision: u64,
    /// `Some` while waiting for mocked latency; `None` while waiting for the agent.
    pub latency_us: Option<u64>,
    /// The engine no longer counts it toward `pauseIfNetworkFetchesPending` (engine patch 1): virtual
    /// time runs under that policy while it is held.
    pub released: bool,
    /// The document that issued it: a hold ends with its document.
    pub document: u64,
    /// The Network-domain id, by which the request's completion is seen.
    pub network_id: Option<String>,
    /// An answer whose call failed without a definite rejection (a timeout or a transport error): it may
    /// have been applied, which the request's completion or a rejected retry shows.
    pub attempted: Option<frontcap_core::event::ResponseRecord>,
    /// The request's `Origin`: a response the platform makes for it carries the CORS headers a server
    /// configured for that origin would send.
    pub origin: Option<String>,
    /// What the request asks for, as the agent sees it in `list_held`.
    pub method: String,
    pub url: String,
    /// The request body as the engine showed it, and whether it has one the engine did not show.
    pub body: Option<String>,
    pub body_unavailable: bool,
    /// The page's main thread waits for it (engine patch 53): the page answers no command meanwhile.
    pub blocks: bool,
}

/// A mocked response scheduled on the agenda.
#[derive(Clone)]
pub struct Delivery {
    pub ordinal: String,
    pub spec: MockSpec,
    pub source: frontcap_core::event::ResponseSource,
    pub requires_progress: bool,
}

/// What the agenda delivers at an instant: a response, or a stream event.
#[derive(Clone)]
pub enum Due {
    Response(Delivery),
    Stream(StreamDue),
}

/// One event the platform delivers on a stream (stream_mock): `open`, a `message`, `error`, `close`.
#[derive(Clone, Debug, PartialEq)]
pub struct StreamDue {
    pub ordinal: String,
    /// Its place in the stream's events: the wrapper ignores one it already had (after a relaunch).
    pub seq: u64,
    pub event: frontcap_core::event::StreamEventKind,
    pub data: Option<String>,
    /// Server-Sent Events: the event's name and id.
    pub sse_event: Option<String>,
    pub sse_id: Option<String>,
    pub code: Option<u16>,
    /// The server's event (passed through, or replayed from a recording): delivered after the other
    /// events of its instant.
    pub from_server: bool,
}

/// A stream the page opened, by the ordinal of its handshake request.
#[derive(Clone, Debug)]
pub struct Stream {
    pub document: u64,
    /// The page's id of it (streams.js).
    pub page_stream: u64,
    pub websocket: bool,
    /// The main world of the document that opened it: deliveries go there or nowhere.
    pub context: Option<String>,
    /// Its events still on the agenda.
    pub keys: Vec<frontcap_core::agenda::AgendaKey>,
    /// The popup whose page has it; none for the launch page.
    pub window: Option<String>,
}

/// A stream held for the agent: its handshake waits for an answer; once open, the agent sends its
/// messages and closes it.
#[derive(Debug, Clone)]
pub struct HeldStream {
    /// The page target of the window it is in.
    pub target_id: String,
    pub url: String,
    pub rule: frontcap_core::ids::RuleId,
    pub rules_revision: u64,
    pub websocket: bool,
    pub open: bool,
    /// The stream's next event number (its opening is 0).
    pub next_seq: u64,
}

#[derive(Default)]
pub struct Network {
    pub rules_revision: u64,
    /// Retained immutable sets needed by requests and streams selected before replacement.
    pub previous_rules: BTreeMap<u64, RuleSet>,
    pub rules: RuleSet,
    pub cursors: SequenceCursors,
    pub next_ordinal: u64,
    pub held: BTreeMap<String, Held>,
    pub agenda: Agenda<Due>,
    /// Streams open in the page, by handshake ordinal.
    pub streams: BTreeMap<String, Stream>,
    /// Streams a `hold` rule keeps for the agent, which answers the handshake and sends the rest.
    pub held_streams: BTreeMap<String, HeldStream>,
    /// Streams the page closed before their handshake was intercepted: (browser-unique context, page stream id).
    pub closed_early: BTreeSet<(String, u64)>,
    /// Reports received before admission, or failed admissions waiting for their ordered report.
    pub early_close_reports: BTreeMap<(String, u64), (VirtualTime, Option<frontcap_core::event::StreamReceipt>)>,
    pub stream_replays: BTreeMap<String, crate::bindings::StreamReplay>,
    pub stream_connections: BTreeMap<String, crate::bindings::StreamConnection>,
    pub stream_selections: BTreeMap<String, crate::bindings::StreamSelection>,
    pub closed_handshakes: BTreeMap<(String, u64), crate::bindings::StreamHandshake>,
    /// Stream anchors announced and not yet decided: Network-domain id → the frame that issued it.
    pub anchors: BTreeMap<String, String>,
    /// Stream anchors decided from the page's announcement before any interception: an interception that
    /// still arrives for one is released without another record.
    pub abandoned_anchors: BTreeSet<String>,
    /// Streams a `record` rule kept, by key: the next session's number.
    pub recorded_sessions: BTreeMap<frontcap_core::cassette::Key, u64>,
    /// The session a replayed stream's handshake chose, for its request's record.
    pub stream_entry: Option<u64>,
    /// Network-domain request id → the passed-through request, for its completion.
    pub passthrough: BTreeMap<String, PassedThrough>,
    /// The browser's console messages about a passed-through request (by network id), recorded after its
    /// response: one the platform makes is recorded before the page receives it, and a replay answers
    /// from a cassette what the recording passed through.
    pub deferred_console: BTreeMap<String, Vec<(frontcap_core::time::VirtualTime, frontcap_core::event::ConsoleRecord)>>,
    pub status: BTreeMap<String, i64>,
    /// Network-domain request id → virtual issue time (`requestWillBeSent.wallTime` follows virtual time).
    pub issued: BTreeMap<String, VirtualTime>,
    pub interceptions: Interceptions,
    /// Main-frame document requests passed through and still loading, by Network-domain id. A document
    /// left before it finished gets no completion event (S14 follow-up), so the next commit ends it.
    pub loading_documents: BTreeSet<String>,
    /// Network-domain ids of CORS preflights (`requestWillBeSent` with a preflight initiator; their
    /// interception reports them as ordinary subresources).
    pub preflights: BTreeSet<String>,
    /// Preflight id → application request id, whose renderer stamp supplies its issue time.
    pub preflight_requests: BTreeMap<String, String>,
    /// Requests the browser issued (a dedicated worker's script): their `wallTime` is the real clock's.
    pub browser_issued: BTreeSet<String>,
    /// Worker main-script identities remain known after issue-time admission and redirects.
    pub browser_worker_scripts: BTreeSet<String>,
    /// Durable bindings prove these requests already existed before reconnection.
    /// Their historical Network announcements are not replayed by the browser.
    pub recovered_network_requests: BTreeSet<String>,
    /// Request ownership frozen durably before selection, independent of live routes.
    pub worker_request_owners: BTreeMap<String, crate::bindings::WorkerLineage>,
    /// First-hop issue instants observed with those owners, before any decision.
    pub worker_request_issues: BTreeMap<String, VirtualTime>,
    /// Where each owner's creator document stood then: its place among same-instant issuers.
    pub worker_request_places: BTreeMap<String, crate::bindings::DocumentPlace>,
    /// Network-domain ids of requests the platform answered whose loading has not finished in the page.
    pub answered: BTreeSet<String>,
    /// Cassettes of `replay` rules, with the lines already answered.
    pub cassettes: BTreeMap<frontcap_core::ids::RuleId, ReplayCassette>,
    /// Requests a `record` rule passes through, by ordinal: the cassette name, the request's key, and its
    /// issue time, and whether a non-document request was issued while stopped.
    pub recording: BTreeMap<String, (String, frontcap_core::cassette::Key, VirtualTime, bool)>,
    /// Recorded requests continued with response interception, by native pause id: the ordinal whose
    /// cassette line is written from the paused response (Fetch.getResponseBody).
    pub response_stage: BTreeMap<String, String>,
    /// Status and headers of responses a `record` rule keeps, by network id, until the body has arrived.
    pub response_meta: BTreeMap<String, (u16, BTreeMap<String, String>)>,
}

#[derive(Debug, Default, Clone)]
pub struct ReplayCassette {
    pub lines: Vec<frontcap_core::cassette::Line>,
    pub served: BTreeSet<usize>,
    /// Recorded streams, and the sessions already played.
    pub streams: Vec<frontcap_core::cassette::StreamEvent>,
    pub sessions_served: BTreeSet<(frontcap_core::cassette::Key, u64)>,
}

/// A request passed through to the network, outstanding. Virtual time does not flow while it is (it
/// counts toward `pauseIfNetworkFetchesPending`), so its response reaches the page at its issue instant;
/// unless the platform ran `advance` segments meanwhile (`moved`), which let virtual time flow regardless,
/// and then when the platform saw it end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PassedThrough {
    pub ordinal: String,
    /// The native pause it was continued from: its engine receipt is forgotten once its response is recorded.
    pub fetch_id: String,
    pub rule: frontcap_core::ids::RuleId,
    pub issued: VirtualTime,
    pub moved: bool,
}

impl PassedThrough {
    /// The instant its response reached the page, when the platform sees it at `now`.
    pub fn answered_at(&self, now: VirtualTime) -> VirtualTime {
        if self.moved { now } else { self.issued }
    }
}

/// Response headers a replay does not repeat: they described the recorded transfer, not the response.
const TRANSFER_HEADERS: &[&str] = &["content-length", "transfer-encoding", "connection", "keep-alive", "content-encoding"];

impl Network {
    /// The selected installation, regardless of later rule replacement.
    pub fn rules_at(&self, revision: u64) -> Option<&RuleSet> {
        if revision == self.rules_revision { Some(&self.rules) } else { self.previous_rules.get(&revision) }
    }

    /// The next recorded session of a stream with `key` under a `replay` rule, and its events in order:
    /// the lowest not yet played, or past the last, the last again (`repeat_last`).
    pub fn replay_session(&mut self, rule: &frontcap_core::ids::RuleId, key: &frontcap_core::cassette::Key, after_last: frontcap_core::rules::AfterLast) -> Option<(u64, Vec<frontcap_core::cassette::StreamEvent>)> {
        let c = self.cassettes.get_mut(rule)?;
        let mut sessions: Vec<u64> = c.streams.iter().filter(|e| &e.key == key).map(|e| e.session).collect();
        sessions.sort_unstable();
        sessions.dedup();
        let session = sessions.iter().copied().find(|s| !c.sessions_served.contains(&(key.clone(), *s))).or_else(|| (after_last == frontcap_core::rules::AfterLast::RepeatLast).then(|| sessions.last().copied()).flatten())?;
        c.sessions_served.insert((key.clone(), session));
        let events = c.streams.iter().filter(|e| &e.key == key && e.session == session).cloned().collect();
        Some((session, events))
    }

    /// The next recorded answer of a `replay` rule for `key`, and the response the platform makes of it.
    pub fn replay(&mut self, rule: &frontcap_core::ids::RuleId, key: &frontcap_core::cassette::Key, after_last: frontcap_core::rules::AfterLast, latency_ms: Option<u64>) -> Option<(usize, MockSpec, bool)> {
        let c = self.cassettes.get_mut(rule)?;
        let i = frontcap_core::cassette::next(&c.lines, key, &c.served, after_last == frontcap_core::rules::AfterLast::RepeatLast)?;
        c.served.insert(i);
        let line = &c.lines[i];
        let headers = line.response.headers.iter().filter(|(k, _)| !TRANSFER_HEADERS.contains(&k.to_ascii_lowercase().as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
        let spec = MockSpec { status: line.response.status, headers, body: String::new(), body_base64: Some(line.response.body_base64.clone()), latency_ms: latency_ms.unwrap_or(line.latency.virtual_ms) };
        Some((i, spec, line.requires_progress))
    }
}

/// Two URLs of one document: equal but for their fragments.
fn same_document(a: &str, b: &str) -> bool {
    a.split('#').next() == b.split('#').next()
}

/// Requests seen issued whose interception has not been handled yet, each with its place in the page's
/// issue order: where the renderer's event announcing it came in the session's stream
/// (`Network.requestWillBeSent` for a subresource, `Page.frameRequestedNavigation` for a child frame's
/// document). The page's one thread issues both kinds and its session delivers their events in that
/// order, while their interceptions take different paths through the browser and arrive in either order;
/// a request is handled once every request issued before it has been, so ordinals, sequence steps and
/// cassette lines follow the page's order. The trace cannot move past the earliest issue time among them,
/// since its request record, stamped at the issue time, is still to come. Network ids already handled are
/// remembered so a late `requestWillBeSent` does not make them wait again.
#[derive(Default)]
pub struct Interceptions {
    waiting: BTreeMap<String, Issued>,
    /// The latest announcement taken: a collected round holds every request announced through it.
    latest: Option<Arrival>,
    /// Child frames' navigations not intercepted yet, by frame.
    navigations: BTreeMap<String, RequestedNavigation>,
    /// Child frames' navigations whose interception was handled, by frame: a redirect the platform
    /// answers puts its next hop back at that place.
    intercepted: BTreeMap<String, RequestedNavigation>,
    handled: BTreeSet<String>,
    overdue: BTreeSet<String>,
    overdue_frames: BTreeSet<String>,
    /// Child frames' navigations the page replaced with another before their interception, by frame
    /// (the latest few): an interception that still arrives for one is the browser's timing, not the page's.
    replaced: BTreeMap<String, Vec<String>>,
}

/// A subresource the page issued: its URL, issue time, place in the page's issue order, and when the
/// platform saw it announced (wall time). `ended`: the page's load ended before any interception (a stream
/// anchor whose frame went in the opening task), so it is decided at its place without one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Issued {
    url: String,
    t: VirtualTime,
    at: Arrival,
    seen: std::time::Instant,
    ended: bool,
    /// Browser-issued worker scripts have no position among renderer reports.
    ordered: bool,
    /// Documents and stream/control admission have separate collection lifecycles.
    collected_separately: bool,
}

/// A child frame's navigation the page requested: its URL, the platform's time when it was seen (its
/// issue is no earlier), its place in the page's issue order, and when the platform saw it (wall time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestedNavigation {
    pub url: String,
    pub t: VirtualTime,
    pub at: Arrival,
    seen: std::time::Instant,
    /// A redirect's next hop: its URL is the redirect's, whatever the page requested.
    redirect: bool,
}

/// One the platform stopped waiting for: what it was, for the record, and when it was issued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GivenUp {
    pub what: String,
    pub t: VirtualTime,
}

impl Interceptions {
    pub fn issued(&mut self, id: &str, url: &str, t: VirtualTime, at: Arrival) {
        self.latest = self.latest.max(Some(at));
        if !self.handled.contains(id) {
            self.waiting.insert(id.to_string(), Issued { url: url.to_string(), t, at, seen: std::time::Instant::now(), ended: false, ordered: true, collected_separately: false });
        }
    }

    pub fn issued_by_browser(&mut self, id: &str, url: &str, t: VirtualTime, at: Arrival) {
        self.issued(id, url, t, at);
        if let Some(request) = self.waiting.get_mut(id) { request.ordered = false; }
    }

    pub fn collect_separately(&mut self, id: &str) {
        if let Some(request) = self.waiting.get_mut(id) { request.collected_separately = true; }
    }

    /// Ordinary announced requests still requiring an interception or terminal event.
    /// Browser-issued main scripts participate even though they have no renderer order.
    pub fn pending_requests(&self) -> impl Iterator<Item = &str> {
        self.waiting.iter().filter(|(_, w)| !w.collected_separately && !w.ended).map(|(id, _)| id.as_str())
    }

    /// A child frame's navigation was requested; it replaces one the frame had requested before, which
    /// the new one cancels.
    pub fn navigation_requested(&mut self, frame: &str, url: &str, t: VirtualTime, at: Arrival) {
        self.latest = self.latest.max(Some(at));
        self.overdue_frames.remove(frame);
        self.intercepted.remove(frame);
        let replaced = self.replaced.entry(frame.to_string()).or_default();
        replaced.retain(|u| !same_document(u, url));
        if let Some(pending) = self.navigations.get(frame).filter(|n| !n.redirect && !same_document(&n.url, url)) {
            replaced.push(pending.url.clone());
            if replaced.len() > 8 {
                replaced.remove(0);
            }
        }
        self.navigations.insert(frame.to_string(), RequestedNavigation { url: url.to_string(), t, at, seen: std::time::Instant::now(), redirect: false });
    }

    /// The platform answered the frame's intercepted navigation with a redirect the browser follows: its
    /// next hop keeps that navigation's place in the page's issue order, so requests the page issued later
    /// wait for it as they waited for the first hop. Its arrival follows the browser's timing.
    pub fn navigation_redirected(&mut self, frame: &str) {
        if let Some(mut n) = self.intercepted.remove(frame) {
            n.redirect = true;
            n.seen = std::time::Instant::now();
            self.navigations.insert(frame.to_string(), n);
        }
    }

    /// The frame's navigation ended without an interception (the frame committed another document, or is
    /// gone). True when it had been given up on.
    pub fn navigation_ended(&mut self, frame: &str) -> bool {
        self.navigations.remove(frame);
        self.intercepted.remove(frame);
        self.overdue_frames.remove(frame)
    }

    /// The frame's document request to `url` was intercepted: it ends the frame's pending navigation when
    /// that is the one to `url` (a later one, which replaced it, stays). True when it had been given up on.
    pub fn navigation_ended_at(&mut self, frame: &str, url: &str) -> bool {
        if self.navigations.get(frame).is_some_and(|n| n.redirect || same_document(&n.url, url))
            && let Some(n) = self.navigations.remove(frame)
        {
            self.intercepted.insert(frame.to_string(), n);
        }
        self.overdue_frames.remove(frame)
    }

    /// Whether an interception of `url` in `frame` belongs to a navigation the page replaced with another
    /// before its interception (and not to the one pending now); the first such interception is consumed.
    pub fn replaced_navigation(&mut self, frame: &str, url: &str) -> bool {
        if self.navigations.get(frame).is_some_and(|n| same_document(&n.url, url)) {
            return false;
        }
        let Some(replaced) = self.replaced.get_mut(frame) else { return false };
        match replaced.iter().position(|u| same_document(u, url)) {
            Some(i) => {
                replaced.remove(i);
                true
            }
            None => false,
        }
    }

    /// Where a subresource waiting for its interception was issued.
    pub fn place(&self, id: &str) -> Option<Arrival> {
        self.waiting.get(id).filter(|w| w.ordered).map(|w| w.at)
    }

    /// When a request still waiting was announced, whether or not it has a place among the renderer's
    /// announcements (a browser-issued worker script has none).
    pub fn announced_at(&self, id: &str) -> Option<Arrival> {
        self.waiting.get(id).map(|w| w.at)
    }

    /// The latest announcement taken so far.
    pub fn latest(&self) -> Option<Arrival> {
        self.latest
    }

    /// Still-undecided renderer requests before this request's notification boundary.
    pub fn before(&self, id: &str) -> Vec<(String, Arrival)> {
        let Some(at) = self.place(id) else { return Vec::new() };
        let mut earlier: Vec<_> = self.waiting.iter().filter(|(_, w)| w.ordered && w.at < at)
            .map(|(id, w)| (id.clone(), w.at)).collect();
        earlier.sort_by_key(|(_, at)| *at);
        earlier
    }

    /// Where a child frame's pending navigation was requested.
    pub fn navigation_place(&self, frame: &str) -> Option<Arrival> {
        self.navigations.get(frame).map(|n| n.at)
    }

    /// Whether `id`'s interception was handled before (a redirect's next hop) or given up on.
    pub fn known(&self, id: &str) -> bool {
        self.handled.contains(id) || self.overdue.contains(id)
    }

    /// Renderer announcements and child frames' navigations still waiting for an interception, with
    /// where each was announced (a navigation has no Network-domain id).
    pub fn outstanding(&self) -> impl Iterator<Item = (Option<&str>, Arrival)> {
        self.waiting.iter().filter(|(_, w)| w.ordered).map(|(id, w)| (Some(id.as_str()), w.at))
            .chain(self.navigations.values().map(|n| (None, n.at)))
    }

    /// Whether a request issued before `at` is still to be intercepted.
    pub fn earlier_than(&self, at: Arrival) -> bool {
        self.waiting.values().any(|w| w.ordered && w.at < at) || self.navigations.values().any(|n| n.at < at)
    }

    /// Observer reports after this announcement wait until its request is recorded. Navigation
    /// announcements are excluded: their issue time may itself require an observer report.
    /// Browser-issued requests have no place in renderer report order either.
    pub fn first_request(&self) -> Option<Arrival> {
        self.waiting.values().filter(|w| w.ordered).map(|w| w.at).min()
    }

    /// Marks an interception handled; true when it had been given up on, so its record can no longer be
    /// placed at its issue time.
    pub fn handled(&mut self, id: &str) -> bool {
        self.waiting.remove(id);
        self.handled.insert(id.to_string());
        self.overdue.remove(id)
    }

    /// Keeps `id` at its place after its load ended unintercepted; false when it is not waiting.
    pub fn end_waiting(&mut self, id: &str) -> bool {
        self.waiting.get_mut(id).map(|w| w.ended = true).is_some()
    }

    /// The request whose load ended unintercepted that comes next in the issue order: no request or
    /// navigation still to be intercepted was issued before it. Its URL, issue time and place.
    pub fn next_ended(&self) -> Option<(String, String, VirtualTime, Arrival)> {
        let (id, w) = self.waiting.iter().filter(|(_, w)| w.ended).min_by_key(|(_, w)| w.at)?;
        let earlier = self.waiting.values().any(|o| o.ordered && !o.ended && o.at < w.at) || self.navigations.values().any(|n| n.at < w.at);
        (!earlier).then(|| (id.clone(), w.url.clone(), w.t, w.at))
    }

    /// The request finished or failed in the engine; nothing more is expected of it.
    pub fn ended(&mut self, id: &str) {
        self.waiting.remove(id);
        self.handled.remove(id);
        self.overdue.remove(id);
    }

    pub fn horizon(&self) -> Option<VirtualTime> {
        self.waiting.values().map(|w| w.t).chain(self.navigations.values().map(|n| n.t)).min()
    }

    pub fn is_empty(&self) -> bool {
        self.waiting.is_empty() && self.navigations.is_empty()
    }

    /// Stops waiting for every pending interception and returns them, to be recorded.
    pub fn give_up(&mut self) -> Vec<GivenUp> {
        self.give_up_matching(Arrival(u64::MAX), None, false, &BTreeSet::new())
    }

    /// Stops waiting for the interceptions of requests issued before `at` — those announced no later than
    /// `seen_by` (wall time), when given — and returns them. Engine ids stay out of the record.
    pub fn give_up_before(&mut self, at: Arrival, seen_by: Option<std::time::Instant>) -> Vec<GivenUp> {
        self.give_up_matching(at, seen_by, true, &BTreeSet::new())
    }

    /// `give_up_before`, keeping the requests whose interceptions have arrived (`intercepted`).
    pub fn give_up_unintercepted_before(&mut self, at: Arrival, seen_by: Option<std::time::Instant>, intercepted: &BTreeSet<String>) -> Vec<GivenUp> {
        self.give_up_matching(at, seen_by, true, intercepted)
    }

    fn give_up_matching(&mut self, at: Arrival, seen_by: Option<std::time::Instant>, ordered_only: bool, intercepted: &BTreeSet<String>) -> Vec<GivenUp> {
        let due = |a: Arrival, seen: std::time::Instant| a < at && seen_by.is_none_or(|by| seen <= by);
        let ids: Vec<String> = self.waiting.iter()
            .filter(|(id, w)| (!ordered_only || w.ordered) && due(w.at, w.seen) && !intercepted.contains(*id))
            .map(|(id, _)| id.clone()).collect();
        let frames: Vec<String> = self.navigations.iter().filter(|(_, n)| due(n.at, n.seen)).map(|(f, _)| f.clone()).collect();
        let mut gone = Vec::new();
        for id in ids {
            let w = self.waiting.remove(&id).expect("listed");
            gone.push(GivenUp { what: format!("the request to {}", w.url), t: w.t });
            self.overdue.insert(id);
        }
        for frame in frames {
            let n = self.navigations.remove(&frame).expect("listed");
            gone.push(GivenUp { what: format!("a frame's navigation to {}", n.url), t: n.t });
            self.overdue_frames.insert(frame);
        }
        gone
    }
}

impl Network {
    /// The earliest instant a record still to come belongs at: a request whose interception is to be
    /// handled, or a passed-through response still outstanding.
    pub fn horizon(&self) -> Option<VirtualTime> {
        let outstanding = self.passthrough.values().filter(|p| !p.moved).map(|p| p.issued).min();
        match (self.interceptions.horizon(), outstanding) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// The ordinal given last, if any.
    pub fn last_ordinal(&self) -> Option<String> {
        (self.next_ordinal > 0).then(|| format!("req-{}", self.next_ordinal))
    }

    pub fn ordinal(&mut self) -> String {
        self.next_ordinal += 1;
        format!("req-{}", self.next_ordinal)
    }

    /// The smallest non-zero mocked latency among installed rules, bounding latency segments.
    pub fn min_latency(&self) -> Option<VirtualDuration> {
        use frontcap_core::rules::Action;
        self.rules
            .rules
            .iter()
            .flat_map(|r| match &r.action {
                Action::Mock(m) => vec![m.latency_ms],
                Action::Sequence { steps, .. } => steps.iter().map(|s| s.latency_ms).collect(),
                _ => vec![],
            })
            .filter(|ms| *ms > 0)
            .min()
            .map(VirtualDuration::from_ms)
    }

    pub fn agenda_next(&self) -> Option<VirtualTime> {
        self.agenda.next_time()
    }
}

pub type Timed<T> = Vec<(VirtualTime, T)>;

/// Splits buffered events, sorted by virtual time, into those that can be written now and those after
/// `horizon`, the issue time of the earliest request whose record is still to come.
pub fn split_at_horizon<T>(mut events: Timed<T>, horizon: Option<VirtualTime>) -> (Timed<T>, Timed<T>) {
    events.sort_by_key(|(t, _)| *t);
    let cut = horizon.map_or(events.len(), |h| events.partition_point(|(t, _)| *t <= h));
    let later = events.split_off(cut);
    (events, later)
}

#[cfg(test)]
#[path = "network_tests.rs"]
mod tests;
