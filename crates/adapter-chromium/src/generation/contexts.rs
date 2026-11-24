//! Windows the generation's pages open (`window.open`, a link or form aimed at a new window) under the
//! `controlled` popup policy: each is a context of the generation (`popup-1`, `popup-2`… in the order they
//! were opened), enrolled before it runs anything.
//!
//! The browser holds every new page at creation (`Target.setAutoAttach {waitForDebuggerOnStart}` on its
//! own session). The enrollment task routes the page's network events to the generation, enables its
//! domains and request interception, installs the page's instrumentation and environment (the engine runs
//! the scripts at once in the document a held window already has), reads the instant it was opened, and
//! only then releases it. A window can share the opener's renderer and hold its main thread:
//! clock preparation reads the partition clock in the browser without waiting on the opener. A window opened
//! without an opener (`noopener`, a `target=_blank` link) does not hold its opener, and its first
//! navigation, pending in the browser, would not stop the renderer's clock: it is enrolled held, and the
//! generation admits it at the next stopped boundary (`admit_held_windows`).
use super::*;
use super::lifecycle::join_workers_to_virtual_time;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Which session sent each routed event, by the event's key (a Fetch interception id, a network request id).
pub(super) type SessionBook = Arc<Mutex<HashMap<String, Arc<str>>>>;

/// A subscription other sessions' events are routed to, keeping which session sent each one.
pub(super) struct Routed<E> {
    sub: cdp::Subscription<E>,
    key: fn(&E) -> String,
    sessions: SessionBook,
}

impl<E: cdp::Event> Routed<E> {
    pub(super) fn new(sub: cdp::Subscription<E>, key: fn(&E) -> String) -> Self {
        Routed { sub, key, sessions: Default::default() }
    }

    fn note(&self, t: cdp::Tagged<E>) -> (cdp::Arrival, E) {
        if let Some(session) = t.session {
            self.sessions.lock().unwrap().insert((self.key)(&t.event), session);
        }
        (t.arrival, t.event)
    }

    pub(super) async fn next(&mut self) -> Result<E, cdp::CdpError> {
        let t = self.sub.next_tagged().await?;
        Ok(self.note(t).1)
    }

    pub(super) fn try_next(&mut self) -> Result<Option<E>, cdp::CdpError> {
        let t = self.sub.try_next_tagged()?;
        Ok(t.map(|t| self.note(t).1))
    }

    pub(super) async fn next_timeout(&mut self, d: Duration) -> Result<Option<E>, cdp::CdpError> {
        Ok(self.next_timeout_stamped(d).await?.map(|(_, e)| e))
    }

    pub(super) async fn next_timeout_stamped(&mut self, d: Duration) -> Result<Option<(cdp::Arrival, E)>, cdp::CdpError> {
        let t = self.sub.next_timeout_tagged(d).await?;
        Ok(t.map(|t| self.note(t)))
    }

    pub(super) fn feed(&self) -> cdp::Feed<E> {
        self.sub.feed()
    }

    pub(super) fn remember_session(&self, key: String, session: &str) {
        self.sessions.lock().unwrap().insert(key, Arc::from(session));
    }

    /// The session that sent the event with this key.
    pub(super) fn session(&self, key: &str) -> Option<Arc<str>> {
        self.sessions.lock().unwrap().get(key).cloned()
    }
}

/// One window a context opened.
pub(super) struct Popup {
    pub(super) id: String,
    pub(super) target_id: String,
    pub(super) session_id: String,
    /// The context that opened it.
    pub(super) opener: String,
    /// The virtual instant it was opened, read while it was held (its opener's thread waited with it).
    pub(super) opened_at: Option<VirtualTime>,
    pub(super) closed: bool,
    /// Enrolled and released: until then it is not in the trace.
    ready: bool,
    /// Opened without an opener: enrolled held, released at the next stopped boundary
    /// (`admit_held_windows`); not open until then.
    pub(super) held: bool,
    pub(super) no_opener: bool,
    recorded_open: bool,
    recorded_close: bool,
    /// Its dedicated workers, joined like the page's.
    pub(super) workers: Workers,
    /// Its page events, subscribed before it was enabled; the generation takes them when it adopts it.
    subs: Option<PageSubs>,
    /// Where a relaunched recorder found it: documents seen, observer epoch and last report (`readopt`).
    resume: Option<(u64, u64, u64)>,
}

/// The subscriptions of one page context's own events (the generation's `Subs` holds the active one's).
pub(super) struct PageSubs {
    binding: cdp::Subscription<runtime::BindingCalledEvent>,
    context: cdp::Subscription<runtime::ExecutionContextCreatedEvent>,
    context_destroyed: cdp::Subscription<runtime::ExecutionContextDestroyedEvent>,
    contexts_cleared: cdp::Subscription<runtime::ExecutionContextsClearedEvent>,
    navigated: cdp::Subscription<page::FrameNavigatedEvent>,
    requested: cdp::Subscription<page::FrameRequestedNavigationEvent>,
    frame_navigated: cdp::Subscription<page::FrameNavigatedEvent>,
    detached: cdp::Subscription<page::FrameDetachedEvent>,
    within: cdp::Subscription<page::NavigatedWithinDocumentEvent>,
    frames: cdp::Subscription<page::ScreencastFrameEvent>,
    chooser: cdp::Subscription<page::FileChooserOpenedEvent>,
}

impl PageSubs {
    fn on(client: &cdp::client::Caller, session: &str) -> PageSubs {
        let s = Some(session);
        PageSubs {
            binding: client.subscribe(s),
            context: client.subscribe(s),
            context_destroyed: client.subscribe(s),
            contexts_cleared: client.subscribe(s),
            navigated: client.subscribe(s),
            requested: client.subscribe(s),
            frame_navigated: client.subscribe(s),
            detached: client.subscribe(s),
            within: client.subscribe(s),
            frames: client.subscribe(s),
            chooser: client.subscribe(s),
        }
    }
}

/// What a page context has of its own. The generation's fields hold the active context's; the launch
/// page is active except while a command addresses a popup (`enter_context`) or the generation reads a
/// popup's events. Progress runs with the launch page active.
pub(super) struct PageState {
    target_id: String,
    page: Session,
    main_frame: page::FrameId,
    world: Option<runtime::ExecutionContextId>,
    world_unique: Option<String>,
    main_context: Option<runtime::ExecutionContextId>,
    main_context_unique: Option<String>,
    stream_contexts: BTreeMap<String, String>,
    stream_observers: BTreeMap<i64, Vec<(cdp::Arrival, String)>>,
    exception_contexts: BTreeMap<i64, Vec<(cdp::Arrival, String)>>,
    deferred_binding: Option<(cdp::Arrival, runtime::BindingCalledEvent)>,
    documents: u64,
    within_document: u64,
    report_epoch: u64,
    max_rseq: u64,
    written_rseq: u64,
    last_samples: BTreeMap<WatchName, SampleValue>,
    resume_cursor: Option<(u64, BTreeMap<WatchName, SampleValue>)>,
    dom_base: u64,
    dom_doc: u64,
    presented: Option<presentation::Presentation>,
    ref_of: BTreeMap<(u64, i64), String>,
    pub(super) mouse_at: Option<(f64, f64)>,
    subs: PageSubs,
}

impl PageState {
    /// The DOM mutation records this context's documents reported.
    pub(super) fn dom_mutations(&self) -> u64 {
        self.dom_base + self.dom_doc
    }
}

/// The generation's page contexts besides the launch page, shared with the enrollment task.
#[derive(Default)]
pub(super) struct ContextBook {
    pub(super) page_target: String,
    pub(super) popups: Vec<Popup>,
    /// Dialogs a `hold` rule matched in a popup, answered as no rule had matched, not yet in the trace.
    /// The targets whose attach the enrollment task has handled, whatever it decided.
    decided: std::collections::BTreeSet<String>,
    fault: Option<(String, String)>,
}

pub(super) type Contexts = Arc<Mutex<ContextBook>>;

/// The launch page's context id.
pub const PAGE: &str = "page";

impl ContextBook {
    pub(super) fn check_enrollment(&self) -> Result<(), ChromiumError> {
        if let Some((target, detail)) = &self.fault {
            return Err(ChromiumError::ContextEnrollment { target: target.clone(), detail: detail.clone() });
        }
        Ok(())
    }

    /// The open context a target is: the launch page or a popup.
    fn context_of_target(&self, target: &str) -> Option<String> {
        if target == self.page_target {
            return Some(PAGE.into());
        }
        self.popups.iter().find(|p| p.target_id == target && !p.closed).map(|p| p.id.clone())
    }

    fn knows_target(&self, target: &str) -> bool {
        target == self.page_target || self.popups.iter().any(|p| p.target_id == target)
    }

    pub(super) fn by_session(&self, session: &str) -> Option<&Popup> {
        self.popups.iter().find(|p| p.session_id == session)
    }

    fn storage_participants(&self) -> Result<Vec<(String, String, String)>, ChromiumError> {
        let mut participants = Vec::with_capacity(self.popups.len());
        // A closed popup's writes are the partition's; its session storage left with it.
        for popup in self.popups.iter().filter(|p| !p.closed) {
            if !popup.ready || popup.held {
                return Err(ChromiumError::Unsupported(
                    "DOM storage capture requires completed popup enrollment and admission".into(),
                ));
            }
            participants.push((popup.id.clone(), popup.target_id.clone(), popup.session_id.clone()));
        }
        Ok(participants)
    }
}

/// What enrolling a page takes: the page's instrumentation and environment, and the generation's feeds.
pub(super) struct Enrollment {
    pub(super) profile: DeterminismProfile,
    pub(super) launch: LaunchId,
    pub(super) patched: bool,
    pub(super) dialog_rules: Arc<Mutex<frontcap_core::dialog::DialogRules>>,
    pub(super) paused: cdp::Feed<fetch::RequestPausedEvent>,
    pub(super) network: NetworkFeeds,
    pub(super) console: cdp::Feed<runtime::ConsoleApiCalledEvent>,
    pub(super) log: cdp::Feed<cdp::protocol::log::EntryAddedEvent>,
    pub(super) exceptions: cdp::Feed<runtime::ExceptionThrownEvent>,
    /// The generation's native dialog service, which serves each popup's dialogs too.
    pub(super) dialogs: Option<super::DialogSession>,
    /// The generation's page-command clock, parked while any page's dialog is open.
    pub(super) clock: crate::service_clock::ServiceClock,
}

impl Generation {
    /// Holds every page the browser creates from now on, and enrolls those the generation's contexts open.
    pub(super) async fn hold_new_pages(&mut self) -> Result<(), ChromiumError> {
        self.contexts.lock().unwrap().page_target = self.target_id.clone();
        let client = self.page.client.clone();
        let attached = client.subscribe::<target::AttachedToTargetEvent>(None);
        let detached = client.subscribe::<target::DetachedFromTargetEvent>(None);
        self.window_attaches = Some(client.subscribe(None));
        let e = Enrollment {
            profile: self.profile.clone(),
            launch: self.launch_id.clone(),
            patched: self.patched,
            dialog_rules: self.dialog_rules.clone(),
            paused: self.subs.paused.feed(),
            console: self.subs.console.feed(),
            log: self.subs.log.feed(),
            exceptions: self.subs.exceptions.feed(),
            dialogs: self.dialog_session(),
            clock: self.page.service_clock(),
            network: NetworkFeeds {
                worker_changes: self.subs.workers.lock().unwrap().changed.clone(),
                attached: self.subs.attached.feed(),
                sent: self.subs.sent.feed(),
                response: self.subs.response.feed(),
                finished: self.subs.finished.feed(),
                failed: self.subs.failed.feed(),
            },
        };
        tokio::spawn(enroll_new_pages(client.caller(), attached, detached, self.contexts.clone(), e));
        // Pages only: workers stay with the page sessions that start them.
        client.call(None, &target::SetAutoAttachParams {
            flatten: Some(true),
            filter: Some(target::TargetFilter(vec![
                target::FilterEntry { r#type: Some("page".into()), exclude: Some(false) },
                target::FilterEntry { r#type: None, exclude: Some(true) },
            ])),
            ..target::SetAutoAttachParams::new(true, true)
        }).await?;
        Ok(())
    }

    /// The target that issued an intercepted request: its context's.
    pub(super) fn target_of_fetch(&self, id: &fetch::RequestId) -> String {
        let session = self.subs.paused.session(&id.0);
        let popup = session.and_then(|s| self.contexts.lock().unwrap().by_session(&s).map(|p| p.target_id.clone()));
        popup.unwrap_or_else(|| self.entered.as_ref().map_or(&self.target_id, |(_, home)| &home.target_id).clone())
    }

    pub(super) fn interception_pages(&self) -> Vec<(String, Session)> {
        let launch = self.launch_page();
        let target = self.entered.as_ref().map_or(&self.target_id, |(_, home)| &home.target_id);
        let mut pages = vec![(target.clone(), launch.clone())];
        pages.extend(self.contexts.lock().unwrap().popups.iter().filter(|p| !p.closed && !p.session_id.is_empty())
            .map(|p| (p.target_id.clone(), Session::sharing_clock(launch.client.clone(), p.session_id.clone(), launch.service_clock()))));
        pages
    }

    /// The live targets, each with its context (`None`: the launch page).
    pub(super) fn known_targets(&self) -> adoption::Targets {
        targets_of(&self.contexts, self.entered.as_ref().map_or(&self.target_id, |(_, h)| &h.target_id))
    }

    /// The context that issued an intercepted request, when not the launch page.
    pub(super) fn context_of_fetch(&self, id: &fetch::RequestId) -> Option<String> {
        let session = self.subs.paused.session(&id.0)?;
        self.contexts.lock().unwrap().by_session(&session).map(|p| p.id.clone())
    }

    /// The session to answer an intercepted request on: its context's.
    pub(super) fn fetch_session(&self, id: &fetch::RequestId) -> Session {
        self.session_of(self.subs.paused.session(&id.0))
    }

    /// The session a network request was announced on: its context's.
    pub(super) fn network_session(&self, id: &str) -> Session {
        self.session_of(self.subs.sent.session(id))
    }

    /// The popup an event came from, by its session; `None` for the launch page's own.
    pub(super) fn context_of_session(&self, session: Option<&str>) -> Option<String> {
        let session = session?;
        self.contexts.lock().unwrap().by_session(session).map(|p| p.id.clone())
    }

    fn session_of(&self, session: Option<Arc<str>>) -> Session {
        match session {
            Some(s) if *s != *self.page.id && self.contexts.lock().unwrap().by_session(&s).is_some() => {
                Session::new(self.page.client.clone(), s.to_string())
            }
            _ => self.page.clone(),
        }
    }

    /// Marks the active popup closed: its session is gone (the browser's detach may not have come yet).
    pub(super) fn popup_closed(&mut self) {
        if let Some(id) = self.popup_id()
            && let Some(p) = self.contexts.lock().unwrap().popups.iter_mut().find(|p| p.id == id)
        {
            p.closed = true;
        }
    }

    /// Whether the active popup is closed.
    pub(super) fn popup_is_closed(&self) -> bool {
        self.popup_id().is_some_and(|id| self.contexts.lock().unwrap().popups.iter().any(|p| p.id == id && p.closed))
    }

    /// Whether the active context has left `document`: replaced, its window closed, or on the launch page a
    /// navigation reported or its document request in flight (a popup's is seen when it commits).
    pub(super) fn left_document(&self, document: u64) -> bool {
        self.documents != document
            || self.popup_is_closed()
            || (self.popup_id().is_none() && (!self.navigates.is_empty() || !self.net.loading_documents.is_empty()))
    }

    /// Construction only: create every namespace before installing any DOM
    /// storage seed. Browser creation never evaluates window.open in the source.
    pub(super) async fn initialize_contexts(&mut self) -> Result<(), ChromiumError> {
        let mut closing = Vec::new();
        for (index, context) in self.profile.initial_contexts.clone().into_iter().enumerate() {
            self.enter_context(&context.opener)?;
            let created = self.page.client.call(None, &target::CreateTargetParams {
                frontcap_opener_target_id: Some(target::TargetID(self.target_id.clone())),
                frontcap_window_name: Some(context.name),
                frontcap_no_opener: Some(context.no_opener),
                ..target::CreateTargetParams::new("about:blank".into())
            }).await;
            self.leave_context();
            let created = created?;
            let service = self.page.service_clock();
            let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
            while !self.contexts.lock().unwrap().decided.contains(&created.target_id.0) {
                if service.elapsed() >= deadline {
                    return Err(ChromiumError::CallTimeout("initial context enrollment"));
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            self.contexts.lock().unwrap().check_enrollment()?;
            let id = format!("popup-{}", index + 1);
            let release = {
                let mut book = self.contexts.lock().unwrap();
                let popup = book.popups.iter_mut().find(|p| p.target_id == created.target_id.0)
                    .ok_or_else(|| ChromiumError::Unsupported("initial context was not enrolled".into()))?;
                if popup.id != id || popup.opener != context.opener ||
                    popup.no_opener != context.no_opener || popup.closed || !popup.ready {
                    return Err(ChromiumError::Unsupported("initial context ownership differs from its declaration".into()));
                }
                if popup.held {
                    popup.held = false;
                    popup.opened_at = Some(self.now);
                    Some(popup.session_id.clone())
                } else { None }
            };
            self.record_contexts();
            if let Some(session) = release {
                // The browser-created about:blank may already have committed
                // before Page.enable. Await our explicit navigation below,
                // rather than an initial frame event that need not be replayed.
                self.release_held_page(&session).await?;
            }
            if context.closed {
                closing.push((id, created.target_id.0));
                continue;
            }
            self.enter_context(&id)?;
            let navigated = self.navigate_to_start(context.url).await;
            self.leave_context();
            navigated?;
        }
        // Closed once every window that names one as its opener exists: closing severs that opener.
        for (id, target_id) in &closing {
            self.page.client.call(None, &target::CloseTargetParams::new(target::TargetID(target_id.clone()))).await?;
            let service = self.page.service_clock();
            let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
            while !self.contexts.lock().unwrap().popups.iter().any(|p| &p.id == id && p.closed) {
                if service.elapsed() >= deadline {
                    return Err(ChromiumError::CallTimeout("initial context closure"));
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
        if !closing.is_empty() {
            self.record_contexts();
        }
        Ok(())
    }

    pub(super) async fn initialize_dom_storage(&self) -> Result<(), ChromiumError> {
        let Some(state) = &self.profile.initial_dom_storage else { return Ok(()); };
        let empty = frontcap_core::dom_storage::SessionStorageState { context: PAGE.into(), areas: Vec::new() };
        let page_state = state.sessions.iter().find(|s| s.context == PAGE).unwrap_or(&empty);
        crate::dom_storage::restore_target(&self.page, state.encoding, &state.local, page_state).await?;
        for session_state in state.sessions.iter().filter(|s| s.context != PAGE) {
            let session_id = {
                let book = self.contexts.lock().unwrap();
                book.popups.iter().find(|p| p.id == session_state.context && p.ready && !p.held && !p.closed)
                    .ok_or_else(|| ChromiumError::Unsupported("initial session owner is not available".into()))?
                    .session_id.clone()
            };
            let session = Session::new(self.page.client.clone(), session_id);
            crate::dom_storage::restore_target(&session, state.encoding, &[], session_state).await?;
        }
        Ok(())
    }

    /// Reads the enrolled live page contexts using their generation-owned sessions.
    ///
    /// The native read covers every renderer's writes, child frames in other
    /// processes and retired renderers included. The caller must hold a stopped
    /// boundary. A closed popup has no session storage left to read; incompletely
    /// enrolled popups are refused rather than silently omitted.
    pub async fn read_live_context_dom_storage(&self) -> Result<crate::dom_storage::LiveDomStorage, ChromiumError> {
        self.alive()?;
        let participants = self.contexts.lock().unwrap().storage_participants()?;
        // While a popup is selected, the launch page is parked in `entered`,
        // not in `self.page`. Logical ownership must survive that selection.
        let home = self.entered.as_ref().map_or(&self.page, |(_, state)| &state.page);
        let mut sessions = vec![(PAGE.to_string(), home.clone())];
        sessions.extend(participants.iter().map(|(id, _, session)| {
            (id.clone(), Session::new(home.client.clone(), session.clone()))
        }));
        let mut expected_targets = std::collections::BTreeSet::from([
            self.entered.as_ref().map_or(&self.target_id, |(_, state)| &state.target_id).clone(),
        ]);
        expected_targets.extend(participants.iter().map(|(_, target, _)| target.clone()));
        self.verify_storage_targets(&expected_targets).await?;
        let targets: Vec<_> = sessions.iter().map(|(id, session)| (id.as_str(), session)).collect();
        let state = crate::dom_storage::read_live_targets(&targets).await?;
        self.verify_storage_targets(&expected_targets).await?;
        self.alive()?;
        if self.contexts.lock().unwrap().storage_participants()? != participants {
            return Err(ChromiumError::Unsupported("DOM storage context enrollment changed during capture".into()));
        }
        Ok(state)
    }

    /// Derive construction inputs from current native metadata, not launch declarations.
    /// Document and history state are not part of them.
    pub async fn read_context_storage_inputs(&self) -> Result<crate::dom_storage::ContextStorageInputs, ChromiumError> {
        let live = self.read_live_context_dom_storage().await?;
        let unsupported = |message: &str| ChromiumError::Unsupported(message.into());
        let root = live.windows.first().filter(|window| window.context == PAGE)
            .ok_or_else(|| unsupported("DOM storage capture has no launch window"))?;
        if root.opener_frame_id.is_some() {
            return Err(unsupported("launch window opener cannot be reconstructed"));
        }
        let book = self.contexts.lock().unwrap();
        let mut windows = live.windows.iter().skip(1);
        let mut frames = BTreeMap::from([(root.frame_id.as_str(), PAGE)]);
        let mut closed = std::collections::BTreeSet::new();
        let mut contexts = Vec::new();
        for (index, popup) in book.popups.iter().enumerate() {
            if popup.id != format!("popup-{}", index + 1) {
                return Err(unsupported("context construction requires contiguous creation order"));
            }
            if popup.closed {
                contexts.push(frontcap_core::profile::InitialContext {
                    opener: popup.opener.clone(), url: "about:blank".into(), name: String::new(),
                    no_opener: popup.no_opener, closed: true,
                });
                closed.insert(popup.id.as_str());
                continue;
            }
            let window = windows.next().filter(|window| window.context == popup.id)
                .ok_or_else(|| unsupported("captured context lost its creation provenance"))?;
            let (opener, no_opener) = match window.opener_frame_id.as_deref() {
                Some(frame) => (frames.get(frame).copied()
                    .ok_or_else(|| unsupported("current opener is not an earlier top-level context"))?, false),
                // The window that opened it closed: reconstruction closes it too.
                None if !popup.no_opener && closed.contains(popup.opener.as_str()) => (popup.opener.as_str(), false),
                // Severed while both lived, or never there: suppressed from creation.
                None => (popup.opener.as_str(), true),
            };
            contexts.push(frontcap_core::profile::InitialContext {
                opener: opener.into(), url: window.url.clone(), name: window.name.clone(), no_opener, closed: false,
            });
            frames.insert(&window.frame_id, &popup.id);
        }
        if windows.next().is_some() {
            return Err(unsupported("captured windows differ from the generation's contexts"));
        }
        Ok(crate::dom_storage::ContextStorageInputs {
            url: root.url.clone(), window_name: root.name.clone(), contexts, storage: live.storage,
        })
    }

    async fn verify_storage_targets(&self, expected: &std::collections::BTreeSet<String>) -> Result<(), ChromiumError> {
        let targets = self.page.client.call(None, &target::GetTargetsParams::default()).await?.target_infos;
        let mut actual = std::collections::BTreeSet::new();
        // A child frame in another process writes into its page's namespace and the
        // shared local storage; the partition's write confirmation covers its renderer.
        for info in targets {
            if info.r#type == "page" {
                actual.insert(info.target_id.0);
            }
        }
        if &actual != expected {
            return Err(ChromiumError::Unsupported(
                "DOM storage browser targets do not match enrolled page contexts".into(),
            ));
        }
        Ok(())
    }

    /// The generation's contexts, the launch page first, then its popups in the order they were opened.
    pub async fn contexts(&mut self) -> Result<Vec<frontcap_core::observation::ContextEntry>, ChromiumError> {
        use frontcap_core::observation::ContextEntry;
        let urls: BTreeMap<String, String> = self.page.client.call(None, &target::GetTargetsParams::default()).await?
            .target_infos.into_iter().map(|t| (t.target_id.0, t.url)).collect();
        let home = self.entered.as_ref().map_or(&self.target_id, |(_, h)| &h.target_id).clone();
        let mut out = vec![ContextEntry { id: PAGE.into(), opener: None, url: urls.get(&home).cloned().unwrap_or_default(), open: true, no_opener: false, closed_us: None }];
        // A closed window is told by the trace: when it closed and its last document (app-state consumer
        // rerun: a closed popup had neither).
        let mut ended: BTreeMap<String, (Option<VirtualTime>, String)> = BTreeMap::new();
        if self.contexts.lock().unwrap().popups.iter().any(|p| p.closed) {
            for e in BehaviorLog::read(&self.trace_path())? {
                match &e.kind {
                    EventKind::Navigation(n) => if let Some(c) = &n.context { ended.entry(c.clone()).or_default().1 = n.url.clone() },
                    EventKind::Context(c) if c.event == frontcap_core::event::ContextEvent::Closed => { ended.entry(c.context.clone()).or_default().0 = Some(e.t_us) }
                    _ => {}
                }
            }
        }
        let book = self.contexts.lock().unwrap();
        out.extend(book.popups.iter().filter(|p| p.ready && !p.held).map(|p| {
            let (closed_us, last) = ended.get(&p.id).cloned().unwrap_or_default();
            ContextEntry {
                id: p.id.clone(),
                opener: Some(p.opener.clone()),
                url: if p.closed { last } else { urls.get(&p.target_id).cloned().unwrap_or_default() },
                open: !p.closed,
                no_opener: p.no_opener,
                closed_us: if p.closed { closed_us } else { None },
            }
        }));
        Ok(out)
    }

    /// The context a reference names a node in: `None` for a reference this generation did not issue,
    /// `Some("page")` for the launch page's.
    pub fn context_of_ref(&self, id: &str) -> Option<String> {
        self.obs.refs.get(id).map(|e| e.context.clone().unwrap_or_else(|| PAGE.into()))
    }

    /// The launch page's main frame, whichever context is active: the network state is the launch page's.
    pub(super) fn launch_frame(&self) -> &page::FrameId {
        self.entered.as_ref().map_or(&self.main_frame, |(_, home)| &home.main_frame)
    }

    /// The launch page's session, whichever context is active.
    pub(super) fn launch_page(&self) -> &Session {
        self.entered.as_ref().map_or(&self.page, |(_, home)| &home.page)
    }

    /// The popup the generation's fields hold now, if one does.
    pub(super) fn popup_id(&self) -> Option<String> {
        self.entered.as_ref().map(|(id, _)| id.clone())
    }

    /// Exchanges the active context's state with `s`.
    fn swap_page(&mut self, s: &mut PageState) {
        use std::mem::swap;
        swap(&mut self.target_id, &mut s.target_id);
        swap(&mut self.page, &mut s.page);
        swap(&mut self.main_frame, &mut s.main_frame);
        swap(&mut self.world, &mut s.world);
        swap(&mut self.world_unique, &mut s.world_unique);
        swap(&mut self.main_context, &mut s.main_context);
        swap(&mut self.main_context_unique, &mut s.main_context_unique);
        swap(&mut self.stream_contexts, &mut s.stream_contexts);
        swap(&mut self.stream_observers, &mut s.stream_observers);
        swap(&mut self.exception_contexts, &mut s.exception_contexts);
        swap(&mut self.deferred_binding, &mut s.deferred_binding);
        swap(&mut self.documents, &mut s.documents);
        swap(&mut self.within_document, &mut s.within_document);
        swap(&mut self.report_epoch, &mut s.report_epoch);
        swap(&mut self.max_rseq, &mut s.max_rseq);
        swap(&mut self.written_rseq, &mut s.written_rseq);
        swap(&mut self.last_samples, &mut s.last_samples);
        swap(&mut self.resume_cursor, &mut s.resume_cursor);
        swap(&mut self.dom_base, &mut s.dom_base);
        swap(&mut self.dom_doc, &mut s.dom_doc);
        swap(&mut self.presented, &mut s.presented);
        swap(&mut self.ref_of, &mut s.ref_of);
        swap(&mut self.mouse_at, &mut s.mouse_at);
        swap(&mut self.subs.binding, &mut s.subs.binding);
        swap(&mut self.subs.context, &mut s.subs.context);
        swap(&mut self.subs.context_destroyed, &mut s.subs.context_destroyed);
        swap(&mut self.subs.contexts_cleared, &mut s.subs.contexts_cleared);
        swap(&mut self.subs.navigated, &mut s.subs.navigated);
        swap(&mut self.subs.requested, &mut s.subs.requested);
        swap(&mut self.subs.frame_navigated, &mut s.subs.frame_navigated);
        swap(&mut self.subs.detached, &mut s.subs.detached);
        swap(&mut self.subs.within, &mut s.subs.within);
        swap(&mut self.subs.frames, &mut s.subs.frames);
        swap(&mut self.subs.chooser, &mut s.subs.chooser);
    }

    /// Takes the popups enrolled since the last look into the generation's own keeping.
    fn adopt_popups(&mut self) {
        type Adopted = (String, String, String, PageSubs, (u64, u64, u64));
        let adopted: Vec<Adopted> = self.contexts.lock().unwrap().popups.iter_mut()
            .filter(|p| p.ready)
            .filter_map(|p| p.subs.take().map(|s| (p.id.clone(), p.target_id.clone(), p.session_id.clone(), s, p.resume.unwrap_or_default())))
            .collect();
        for (id, target_id, session, subs, (documents, epoch, rseq)) in adopted {
            let state = PageState {
                page: Session::sharing_clock(self.page.client.clone(), session, self.page.service_clock()),
                main_frame: page::FrameId(target_id.clone()),
                target_id,
                world: None, world_unique: None, main_context: None, main_context_unique: None,
                stream_contexts: BTreeMap::new(), stream_observers: BTreeMap::new(), exception_contexts: BTreeMap::new(),
                deferred_binding: None,
                documents, within_document: 0,
                // A relaunched recorder's observer epoch counts on from the trace's (`take_contexts`).
                report_epoch: epoch.saturating_sub(1), max_rseq: 0, written_rseq: 0, last_samples: BTreeMap::new(),
                resume_cursor: (epoch > 0).then(|| (rseq, BTreeMap::new())), dom_base: 0, dom_doc: 0,
                presented: None, ref_of: BTreeMap::new(), mouse_at: None,
                subs,
            };
            self.parked.insert(id, state);
        }
    }

    /// Makes `context` the one commands address until `leave_context`: the launch page (`page`) or an
    /// open popup.
    pub fn enter_context(&mut self, context: &str) -> Result<(), ChromiumError> {
        if self.popup_id().as_deref() == Some(context) || (context == PAGE && self.entered.is_none()) {
            return Ok(());
        }
        self.leave_context();
        if context == PAGE {
            return Ok(());
        }
        self.adopt_popups();
        let open = self.contexts.lock().unwrap().popups.iter().any(|p| p.id == context && !p.closed);
        let mut state = match (open, self.parked.remove(context)) {
            (true, Some(state)) => state,
            (false, Some(state)) => {
                self.parked.insert(context.to_string(), state);
                return Err(ChromiumError::ContextClosed(context.to_string()));
            }
            (_, None) => return Err(ChromiumError::NoSuchContext(context.to_string())),
        };
        self.swap_page(&mut state);
        self.entered = Some((context.to_string(), state));
        Ok(())
    }

    /// A watch's last recorded value, from the page it samples in (its popup's, or the launch page's)
    /// whichever context is active.
    pub(super) fn last_sample(&self, name: &WatchName) -> Option<&SampleValue> {
        let active = self.popup_id();
        match (self.watches.context(name), active.as_deref()) {
            (Some(watched), Some(active)) if watched == active => self.last_samples.get(name),
            (Some(watched), _) => self.parked.get(watched).and_then(|s| s.last_samples.get(name)),
            (None, None) => self.last_samples.get(name),
            (None, Some(_)) => self.entered.as_ref().and_then(|(_, s)| s.last_samples.get(name)),
        }
    }

    /// Makes `previous` (a popup, or the launch page for none) the active context again, after work done
    /// in another.
    pub(super) fn reenter(&mut self, previous: Option<String>) -> Result<(), ChromiumError> {
        match previous {
            Some(popup) => self.enter_context(&popup),
            None => {
                self.leave_context();
                Ok(())
            }
        }
    }

    /// Makes `popup` active again after progress ran with the launch page, open or closed meanwhile: the
    /// input that addressed it ends there.
    pub(super) fn return_to(&mut self, popup: &str) -> Result<(), ChromiumError> {
        self.leave_context();
        let mut state = self.parked.remove(popup).ok_or_else(|| ChromiumError::NoSuchContext(popup.to_string()))?;
        self.swap_page(&mut state);
        self.entered = Some((popup.to_string(), state));
        Ok(())
    }

    /// Makes the launch page the active context again.
    pub fn leave_context(&mut self) {
        if let Some((id, mut state)) = self.entered.take() {
            self.swap_page(&mut state);
            self.parked.insert(id, state);
        }
    }

    /// The open popups the generation has taken in, in the order they were opened.
    pub(super) fn open_popups(&mut self) -> Vec<String> {
        if self.entered.is_some() {
            return Vec::new();
        }
        self.adopt_popups();
        let book = self.contexts.lock().unwrap();
        // A window being admitted has no document to read yet.
        book.popups.iter().filter(|p| !p.closed && !p.held && self.admitting.as_deref() != Some(p.id.as_str()) && self.parked.contains_key(&p.id))
            .map(|p| p.id.clone()).collect()
    }

    /// Reads each open popup's own events (execution contexts, observer reports, navigations), with the
    /// launch page active: progress reads them at the boundaries it reads the page's.
    pub(super) async fn drain_popups(&mut self) -> Result<(), ChromiumError> {
        if self.entered.is_some() {
            return Ok(());
        }
        let held = self.held_windows_attached()?;
        if !held.is_empty() {
            // Boxed: admission handles requests, and every drain would otherwise carry its future.
            Box::pin(self.admit_held_windows(held)).await?;
        }
        self.read_popups(false).await
    }

    /// Reads each open popup's events after waiting for its navigation in progress: one whose document
    /// request was decided since its last drain, or, with `every`, any (input completing: a window the
    /// input opened may have its document request still to come).
    async fn read_popups(&mut self, every: bool) -> Result<(), ChromiumError> {
        for id in self.open_popups() {
            self.enter_context(&id)?;
            let read = match Box::pin(self.await_popup_navigation(every)).await {
                Ok(()) => self.popup_events().await,
                Err(e) => Err(e),
            };
            self.leave_context();
            read?;
        }
        Ok(())
    }

    /// At an input's completion, with the launch page active: every open popup's navigation the input
    /// started (the first document of a window it opened included) has committed or ended, whenever its
    /// document request reached the platform.
    pub(super) async fn await_popup_navigations(&mut self) -> Result<(), ChromiumError> {
        if self.entered.is_some() {
            return Ok(());
        }
        self.read_popups(true).await
    }

    /// Waits, with the clock stopped, until the active popup's document request decided since its last
    /// drain has committed or ended: the engine answers the window's commands only then (a pending
    /// navigation keeps their barrier), and the clock does not pass the navigation anyway. Its commit is
    /// read at the first boundary after the decision (the instant of the input that opened the window),
    /// not at whichever later one wall time brings it to, where a replay would order it differently.
    async fn await_popup_navigation(&mut self, every: bool) -> Result<(), ChromiumError> {
        let frame = self.main_frame.clone();
        // A document held at a stop is answered later; its window answers meanwhile (inspected).
        let held = self.interception_stops.values().any(|(ev, _)| ev.frame_id == frame && ev.resource_type == cnet::ResourceType::Document);
        if held || !(every || self.popup_navigations.contains(&frame.0)) {
            return Ok(());
        }
        self.round_trip().await?;
        self.popup_navigations.remove(&frame.0);
        Ok(())
    }

    /// Admits, at this stopped boundary, the windows opened without an opener that the browser has
    /// announced. The browser attaches such a window held before it answers the page that opened it, so on
    /// the connection its attach precedes the end of the step that opened it: this is the first boundary
    /// after its opening. Each is recorded opened here, released, its first document answered by the rules
    /// and committed before the clock moves again; its scripts run from the next grant.
    pub(super) async fn admit_held_windows(&mut self, targets: Vec<String>) -> Result<(), ChromiumError> {
        for target in targets {
            // The enrollment task decides on it apart from the generation; wait for its decision.
            let service = self.page.service_clock();
            let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
            let held = loop {
                let decided = {
                    let b = self.contexts.lock().unwrap();
                    b.check_enrollment()?;
                    b.decided.contains(&target).then(|| b.popups.iter().find(|p| p.target_id == target && p.held).map(|p| p.id.clone()))
                };
                match decided {
                    Some(held) => break held,
                    None if service.elapsed() < deadline => tokio::time::sleep(Duration::from_millis(2)).await,
                    None => return Err(ChromiumError::CallTimeout("the enrollment of a window without an opener")),
                }
            };
            if let Some(id) = held {
                self.admit(&id).await?;
            }
        }
        Ok(())
    }

    /// A shared grant must wait for every announced page's enrollment decision,
    /// including opener-bearing pages whose setup is still running asynchronously.
    pub(super) async fn settle_shared_window_enrollment(&mut self) -> Result<(), ChromiumError> {
        let mut targets = Vec::new();
        if let Some(feed) = self.window_attaches.as_mut() {
            while let Some(a) = feed.try_next_tagged()? {
                if a.session.is_none() && a.event.target_info.r#type == "page" {
                    targets.push(a.event.target_info.target_id.0);
                }
            }
        }
        self.admit_held_windows(targets).await
    }

    /// The pages the browser attached held without an opener since the last look (its own attachments, not a
    /// page's workers or frames).
    fn held_windows_attached(&mut self) -> Result<Vec<String>, ChromiumError> {
        let mut targets = Vec::new();
        if let Some(feed) = self.window_attaches.as_mut() {
            while let Some(a) = feed.try_next_tagged()? {
                let (a, browser) = (a.event, a.session.is_none());
                if browser && a.target_info.r#type == "page" && a.waiting_for_debugger && !a.target_info.can_access_opener {
                    targets.push(a.target_info.target_id.0);
                }
            }
        }
        Ok(targets)
    }

    async fn admit(&mut self, id: &str) -> Result<(), ChromiumError> {
        let session = {
            let mut b = self.contexts.lock().unwrap();
            let p = b.popups.iter_mut().find(|p| p.id == id).expect("a held window");
            p.held = false;
            p.opened_at = Some(self.now);
            p.session_id.clone()
        };
        // Its opening precedes its requests.
        self.record_contexts();
        self.adopt_popups();
        self.admitting = Some(id.to_string());
        let committed = self.admit_document(id, &session).await;
        self.admitting = None;
        committed
    }

    /// No-opener targets cannot answer renderer commands until debugger release. Queue the clock
    /// preparation first, then release and service its document request while awaiting attachment and the receipt.
    async fn release_held_page(&mut self, session: &str) -> Result<(), ChromiumError> {
        let service = self.page.service_clock();
        let result: Result<(), ChromiumError> = async {
            let mut prepared = if self.patched {
                Some(prepare_clock(&self.page.client.caller(), session, &service).await?)
            } else { None };
            let mut released = Some(self.page.client.send(Some(session), &runtime::RunIfWaitingForDebuggerParams::default())?);
            let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
            loop {
                if let Some(answer) = prepared.as_mut().and_then(cdp::Answer::try_answer) {
                    answer?;
                    prepared = None;
                }
                if let Some(answer) = released.as_mut().and_then(cdp::Answer::try_answer) {
                    answer?;
                    released = None;
                }
                if prepared.is_none() && released.is_none() { break; }
                // A no-opener target may not create its renderer until the first navigation
                // response is available. Service the held document while waiting for attachment.
                self.take_paused()?;
                self.handle_queued().await?;
                if service.elapsed() >= deadline {
                    return Err(ChromiumError::CallTimeout("held page clock preparation"));
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Ok(())
        }.await;
        if let Err(error) = &result {
            let mut book = self.contexts.lock().unwrap();
            let target = book.popups.iter().find(|p| p.session_id == session)
                .map(|p| p.target_id.clone()).unwrap_or_else(|| session.to_string());
            book.fault.get_or_insert_with(|| (target, error.to_string()));
        }
        result
    }

    /// Releases the held window and services it until its first document commits, with the clock stopped.
    async fn admit_document(&mut self, id: &str, session: &str) -> Result<(), ChromiumError> {
        self.release_held_page(session).await?;
        let service = self.page.service_clock();
        let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
        loop {
            self.take_paused()?;
            self.handle_queued().await?;
            self.enter_context(id)?;
            let navigated = self.subs.navigated.next_timeout(Duration::from_millis(5)).await;
            let committed = match navigated {
                Ok(Some(ev)) => match self.take_navigated(ev).await {
                    Ok(()) => true,
                    Err(e) => {
                        self.leave_context();
                        return Err(e);
                    }
                },
                Ok(None) => false,
                Err(e) => {
                    self.leave_context();
                    return Err(e.into());
                }
            };
            self.leave_context();
            if committed {
                return Ok(());
            }
            if service.elapsed() >= deadline {
                return Err(ChromiumError::CallTimeout("the first document of a window without an opener"));
            }
        }
    }

    /// The active popup's own events.
    pub(super) async fn popup_events(&mut self) -> Result<(), ChromiumError> {
        self.drain_bindings(Duration::ZERO).await?;
        let mut navigated = false;
        while let Some(ev) = self.subs.navigated.try_next()? {
            navigated |= ev.frame.parent_id.is_none();
            self.take_navigated(ev).await?;
        }
        while let Some(ev) = self.subs.within.try_next()? {
            self.take_within(ev);
        }
        // A window's first document of its opener's origin reuses the window of the document it opened
        // with, whose observer listeners do not receive the new document's input: the observer adopts the
        // window again for it (as the top observer adopts a child frame's new document).
        if navigated && let Some(world) = self.world.clone() {
            let _ = self.page.eval("globalThis.__frontcap.adopt(window)", Some(world)).await;
        }
        // The popup's dialogs are the generation's (one service serves every page): those it answered
        // belong to the command that addressed it, after the openings its observer reported above.
        self.drain_native_dialog_receipts().await
    }

    /// Records the contexts opened and closed since the last look.
    pub(super) fn record_contexts(&mut self) {
        let (opened, closed) = {
            let mut book = self.contexts.lock().unwrap();
            let mut opened = Vec::new();
            let mut closed = Vec::new();
            for p in book.popups.iter_mut().filter(|p| p.ready && !p.held) {
                if !p.recorded_open {
                    p.recorded_open = true;
                    opened.push((p.id.clone(), p.opener.clone(), p.opened_at, p.target_id.clone(), p.no_opener));
                }
                if p.closed && !p.recorded_close {
                    p.recorded_close = true;
                    closed.push(p.id.clone());
                }
            }
            (opened, closed)
        };
        let at = self.now.max(self.log.last_time());
        for (context, opener, t, target, no_opener) in opened {
            let t = t.map_or(at, |t| t.max(self.log.last_time()));
            self.record(t, EventKind::Context(ContextRecord { context, event: ContextEvent::Opened, opener: Some(opener), target: Some(target), no_opener }));
        }
        for context in closed {
            self.record(at, EventKind::Context(ContextRecord { context, event: ContextEvent::Closed, opener: None, target: None, no_opener: false }));
        }
    }
}

async fn enroll_new_pages(
    client: cdp::client::Caller,
    mut attached: cdp::Subscription<target::AttachedToTargetEvent>,
    mut detached: cdp::Subscription<target::DetachedFromTargetEvent>,
    book: Contexts,
    e: Enrollment,
) {
    loop {
        tokio::select! {
            a = attached.next_tagged() => match a {
                // The browser's own attachments; a page session's (its workers) are not new pages.
                Ok(a) if a.session.is_none() => {
                    let target = a.event.target_info.target_id.0.clone();
                    if let Err(error) = attach(&client, &book, &e, a.event).await {
                        let mut b = book.lock().unwrap();
                        b.fault.get_or_insert_with(|| (target.clone(), error.to_string()));
                    }
                    book.lock().unwrap().decided.insert(target);
                }
                Ok(_) => {}
                Err(cdp::CdpError::Closed) => break,
                Err(_) => {}
            },
            d = detached.next_tagged() => match d {
                Ok(d) if d.session.is_none() => {
                    let session = d.event.session_id.0;
                    let closed = {
                        let mut b = book.lock().unwrap();
                        b.popups.iter_mut().find(|p| p.session_id == session).map(|p| {
                            p.closed = true;
                            client.unroute(&session);
                            p.id.clone()
                        })
                    };
                    if let (Some(id), Some(dialogs)) = (closed, &e.dialogs) {
                        dialogs.remove_context(&id);
                    }
                }
                Ok(_) => {}
                Err(cdp::CdpError::Closed) => break,
                Err(_) => {}
            },
        }
    }
}

async fn attach(client: &cdp::client::Caller, book: &Contexts, e: &Enrollment, a: target::AttachedToTargetEvent) -> Result<(), ChromiumError> {
    let session = a.session_id.0.clone();
    let info = a.target_info;
    let (known, opener) = {
        let b = book.lock().unwrap();
        (b.knows_target(&info.target_id.0), info.opener_id.as_ref().and_then(|o| b.context_of_target(&o.0)))
    };
    let detach = target::DetachFromTargetParams { session_id: Some(target::SessionID(session.clone())), ..Default::default() };
    match opener {
        // A page the generation drives already (its own session is elsewhere), or one it did not open.
        _ if known || info.r#type != "page" => {
            let _ = client.send(None, &detach);
        }
        None => {
            if a.waiting_for_debugger {
                let _ = client.send(Some(&session), &runtime::RunIfWaitingForDebuggerParams::default());
            }
            let _ = client.send(None, &detach);
        }
        // Without an opener the page does not wait for it, and its first navigation would not stop the clock:
        // it stays held, enrolled, until the generation admits it at the next stopped boundary.
        Some(opener) if !info.can_access_opener => {
            let id = {
                let mut b = book.lock().unwrap();
                let id = format!("popup-{}", b.popups.len() + 1);
                b.popups.push(Popup { id: id.clone(), target_id: info.target_id.0.clone(), session_id: session.clone(), opener, opened_at: None, closed: false, ready: false, held: true, no_opener: true, recorded_open: false, recorded_close: false, workers: Default::default(), subs: Some(PageSubs::on(client, &session)), resume: None });
                id
            };
            let (workers, _) = enroll(client, &session, e, false, true).await?;
            serve_dialogs(client, &session, &id, e).await;
            if let Some(p) = book.lock().unwrap().popups.iter_mut().find(|p| p.id == id) {
                p.workers = workers;
                p.ready = true;
            }
        }
        Some(opener) => {
            let id = {
                let mut b = book.lock().unwrap();
                let id = format!("popup-{}", b.popups.len() + 1);
                b.popups.push(Popup { id: id.clone(), target_id: info.target_id.0.clone(), session_id: session.clone(), opener, opened_at: None, closed: false, ready: false, held: false, no_opener: false, recorded_open: false, recorded_close: false, workers: Default::default(), subs: Some(PageSubs::on(client, &session)), resume: None });
                id
            };
            let (workers, _) = enroll(client, &session, e, true, false).await?;
            serve_dialogs(client, &session, &id, e).await;
            // The opener's thread waits with the held window, so this is the instant it opened it.
            let opened_at = tokio::time::timeout(Session::CALL_TIMEOUT, client.call(Some(&session), &emulation::GetVirtualTimeParams::default())).await
                .ok().and_then(|r| r.ok())
                .and_then(|t| t.unix_time_us.parse::<i128>().ok())
                .and_then(|us| u64::try_from(us - i128::from(e.profile.initial_date_ms) * 1000).ok())
                .map(VirtualTime);
            if let Some(p) = book.lock().unwrap().popups.iter_mut().find(|p| p.id == id) {
                p.opened_at = opened_at;
                p.workers = workers;
                p.ready = true;
            }
            let _ = client.send(Some(&session), &runtime::RunIfWaitingForDebuggerParams::default());
        }
    }
    Ok(())
}

/// Obtain the instant without an opener response and queue preparation before debugger release.
async fn prepare_clock(client: &cdp::client::Caller, session: &str, clock: &crate::service_clock::ServiceClock) -> Result<cdp::Answer, ChromiumError> {
    let storage = clock.timeout(Session::CALL_TIMEOUT,
        client.call(Some(session), &page::FrontcapGetStorageClockParams::default())).await
        .map_err(|_| ChromiumError::CallTimeout("Page.frontcapGetStorageClock"))??;
    Ok(client.send(Some(session), &emulation::FrontcapPrepareClockParams { unix_time_us: storage.unix_time_us })?)
}

/// Enrolls instrumentation. A held no-opener page prepares its clock at its later admission instant.
async fn enroll(client: &cdp::client::Caller, session: &str, e: &Enrollment, immediately: bool, held: bool) -> Result<(Workers, Option<cdp::Answer>), ChromiumError> {
    client.route(session, &e.paused);
    client.route(session, &e.console);
    client.route(session, &e.log);
    client.route(session, &e.exceptions);
    client.route(session, &e.network.sent);
    client.route(session, &e.network.response);
    client.route(session, &e.network.finished);
    client.route(session, &e.network.failed);
    client.route(session, &e.network.attached);
    let workers = join_workers_to_virtual_time(client.clone(), session.to_owned(), client.subscribe(Some(session)), client.subscribe(Some(session)), e.network.clone());
    let s = Some(session);
    if e.patched && !held {
        let prepared = prepare_clock(client, session, &e.clock).await?;
        e.clock.timeout(Session::CALL_TIMEOUT, prepared.answered()).await
            .map_err(|_| ChromiumError::CallTimeout("Emulation.frontcapPrepareClock"))??;
    }
    let _ = client.send(s, &page::EnableParams::default());
    let _ = client.send(s, &page::SetInterceptFileChooserDialogParams { enabled: true, cancel: None });
    let _ = client.send(s, &runtime::EnableParams::default());
    let _ = client.send(s, &cdp::protocol::log::EnableParams::default());
    let _ = client.send(s, &cnet::EnableParams::default());
    let _ = client.send(s, &dom::EnableParams::default());
    let rules = e.dialog_rules.lock().unwrap().clone();
    let i = instrument::instrumentation(&e.profile, &e.launch, e.patched, &rules);
    let _ = client.send(s, &i.binding);
    for script in &i.scripts {
        let _ = client.send(s, &page::AddScriptToEvaluateOnNewDocumentParams { run_immediately: Some(immediately), ..script.clone() });
    }
    // The environment `apply_overrides` gives the page.
    let p = &e.profile;
    let _ = client.send(s, &emulation::SetTimezoneOverrideParams::new(p.time_zone.clone()));
    let _ = client.send(s, &emulation::SetLocaleOverrideParams { locale: Some(p.locale.clone()) });
    let _ = client.send(s, &emulation::SetDeviceMetricsOverrideParams::new(
        i64::from(p.viewport.width),
        i64::from(p.viewport.height),
        f64::from(p.viewport.device_scale_milli) / 1000.0,
        false,
    ));
    if p.touch {
        let _ = client.send(s, &emulation::SetTouchEmulationEnabledParams { max_touch_points: Some(5), ..emulation::SetTouchEmulationEnabledParams::new(true) });
    }
    let _ = client.send(s, &cnet::SetBypassServiceWorkerParams { bypass: true });
    let _ = client.send(s, &cnet::SetCacheDisabledParams { cache_disabled: true });
    let _ = client.send(s, &target::SetAutoAttachParams { flatten: Some(true), ..target::SetAutoAttachParams::new(true, true) });
    let fetching = client.send(s, &fetch::EnableParams {
        patterns: Some(vec![fetch::RequestPattern {
            url_pattern: Some("*".into()),
            request_stage: Some(fetch::RequestStage::Request),
            ..Default::default()
        }]),
        ..Default::default()
    }).ok();
    Ok((workers, fetching))
}

/// `known_targets` from a book and the launch page's target.
pub(super) fn targets_of(book: &Contexts, page_target: &str) -> adoption::Targets {
    let mut targets = adoption::Targets::new();
    targets.insert(page_target.to_string(), None);
    for p in book.lock().unwrap().popups.iter().filter(|p| !p.closed) {
        targets.insert(p.target_id.clone(), Some(p.id.clone()));
    }
    targets
}

/// A window the trace records opened: its context, opener, target and whether it has no opener.
type RecordedPopup = (String, String, String, bool);

/// The windows the trace records opened and those it records closed.
fn recorded_popups(events: &[BehaviorEvent]) -> (Vec<RecordedPopup>, std::collections::BTreeSet<String>) {
    let mut opened = Vec::new();
    let mut closed = std::collections::BTreeSet::new();
    for ev in events {
        if let EventKind::Context(c) = &ev.kind {
            match (c.event, &c.opener, &c.target) {
                (ContextEvent::Opened, Some(opener), Some(target)) => opened.push((c.context.clone(), opener.clone(), target.clone(), c.no_opener)),
                (ContextEvent::Closed, _, _) => { closed.insert(c.context.clone()); }
                _ => {}
            }
        }
    }
    (opened, closed)
}

/// The windows the trace records open that the browser still has, as `(context, target)`: a dialog one of
/// them holds blocks every page, so recovery serves their dialogs before any page command.
pub(super) async fn open_popups(client: &cdp::Client, events: &[BehaviorEvent]) -> Result<Vec<(String, String)>, ChromiumError> {
    let (opened, closed) = recorded_popups(events);
    if opened.is_empty() { return Ok(Vec::new()) }
    let live: std::collections::BTreeSet<String> = client.call(None, &target::GetTargetsParams::default()).await?
        .target_infos.into_iter().map(|t| t.target_id.0).collect();
    Ok(opened.into_iter().filter(|(id, _, target, _)| !closed.contains(id) && live.contains(target)).map(|(id, _, target, _)| (id, target)).collect())
}

/// A relaunched recorder takes back the windows the trace records open: each still in the browser is
/// enrolled again (its documents keep their instrumentation; new ones get it), its retained requests
/// replayed by the engine for adoption once its interception is enabled; one that is gone is closed.
#[allow(clippy::too_many_arguments)]
pub(super) async fn readopt(page: &Session, book: &Contexts, events: &[BehaviorEvent], page_target: &str, e: &Enrollment) -> Result<(), ChromiumError> {
    book.lock().unwrap().page_target = page_target.to_string();
    let (opened, closed) = recorded_popups(events);
    if opened.is_empty() {
        return Ok(());
    }
    let live: std::collections::BTreeSet<String> = page.client.call(None, &target::GetTargetsParams::default()).await?
        .target_infos.into_iter().map(|t| t.target_id.0).collect();
    let caller = page.client.caller();
    for (id, opener, target_id, no_opener) in opened {
        let gone = closed.contains(&id);
        let mut popup = Popup {
            id: id.clone(), target_id: target_id.clone(), session_id: String::new(), opener, opened_at: None,
            closed: true, ready: true, held: false, no_opener, recorded_open: true, recorded_close: gone, workers: Default::default(), subs: None, resume: None,
        };
        if gone || !live.contains(&target_id) {
            book.lock().unwrap().popups.push(popup);
            continue;
        }
        let session = page.client.call(None, &target::AttachToTargetParams {
            flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(target_id))
        }).await?.session_id.0;
        let documents = events.iter().filter_map(|ev| match &ev.kind {
            EventKind::Navigation(n) if n.context.as_deref() == Some(id.as_str()) && n.url != "about:blank" => Some(n.document),
            _ => None,
        }).max().unwrap_or(0);
        let last = events.iter().rev().find_map(|ev| match &ev.kind {
            EventKind::Navigation(n) if n.rseq != 0 && n.context.as_deref() == Some(id.as_str()) => Some((n.document, n.rseq)),
            EventKind::Focus(f) if f.context.as_deref() == Some(id.as_str()) => Some((f.document, f.rseq)),
            EventKind::Message(m) if m.context.as_deref() == Some(id.as_str()) => Some((m.document, m.rseq)),
            EventKind::Dialog(d) if d.context.as_deref() == Some(id.as_str()) => d.observer.as_ref().map(|o| (o.document, o.rseq)),
            EventKind::Watch(w) if w.context.as_deref() == Some(id.as_str()) => Some((w.document, w.rseq)),
            _ => None,
        });
        let documents = documents.max(last.map_or(0, |(d, _)| d));
        popup.session_id = session.clone();
        popup.closed = false;
        popup.subs = Some(PageSubs::on(&caller, &session));
        popup.resume = Some((documents, last.map_or(0, |(d, _)| d), last.map_or(0, |(_, r)| r)));
        book.lock().unwrap().popups.push(popup);
        let (workers, fetching) = enroll(&caller, &session, e, false, false).await?;
        serve_dialogs(&caller, &session, &id, e).await;
        if let Some(p) = book.lock().unwrap().popups.iter_mut().find(|p| p.id == id) {
            p.workers = workers;
        }
        // The engine replays the retained requests before it acknowledges interception.
        if let Some(fetching) = fetching {
            tokio::time::timeout(Session::CALL_TIMEOUT, fetching.answered()).await
                .map_err(|_| ChromiumError::CallTimeout("Fetch.enable"))??;
        }
    }
    Ok(())
}

/// Serves a popup's native dialogs with the generation's, before the popup runs: the rules answer them, and
/// one a `hold` rule matches waits for the agent, as in the launch page. A popup whose dialogs cannot be
/// served would block every page at its first one, so that fails the dialog service.
async fn serve_dialogs(client: &cdp::client::Caller, session: &str, id: &str, e: &Enrollment) {
    let Some(dialogs) = &e.dialogs else { return };
    if let Err(error) = dialogs.add_context(id.to_string(), client.clone(), session, e.clock.clone()).await {
        dialogs.launch_control().report_fault(format!("{id}'s dialogs could not be served: {error}"));
    }
}

/// A command to a session the browser no longer has: its target closed.
pub(super) fn session_gone(e: &cdp::CdpError) -> bool {
    matches!(e, cdp::CdpError::SessionDetached | cdp::CdpError::Protocol { code: -32001, .. })
}
