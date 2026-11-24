use super::*;

impl Generation {
    /// Replaces the document while frozen. `Fetch` holds the document request, so the document requests
    /// are handled here, by the rules, until the new document commits. `Page.navigate` answers at the
    /// commit; `Page.reload` and `Page.navigateToHistoryEntry` answer before the request is intercepted
    /// (S14), so the commit is awaited as `frameNavigated`. Under paused virtual time the document commits
    /// but runs no script (S1).
    pub(super) async fn navigate_frozen(&mut self, to: Navigation) -> Result<(), ChromiumError> {
        let dialog_control = self.native_dialog_control();
        let prior_dialogs: std::collections::BTreeSet<_> = match &dialog_control {
            Some(control) => control.inspect().await?.receipts.into_iter().map(|r| r.dialog.dialog_id).collect(),
            None => Default::default(),
        };
        let mut dialog_updates = dialog_control.as_ref().map(|control| control.subscribe());
        let client = self.page.client.clone();
        let session = self.page.id.clone();
        let mut navigation = tokio::spawn(async move {
            match to {
                Navigation::To(url) => client.call(Some(&session), &page::NavigateParams::new(url)).await.map(|r| r.error_text),
                Navigation::Entry(id) => client.call(Some(&session), &page::NavigateToHistoryEntryParams::new(id)).await.map(|_| None),
                Navigation::Reload => client.call(Some(&session), &page::ReloadParams::default()).await.map(|_| None),
            }
        });
        let service = self.page.service_clock();
        let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
        let (documents, within) = (self.documents, self.within_document);
        let mut answered = None;
        self.own_navigation = true;
        let result = loop {
            if answered.is_some() && (self.documents != documents || self.within_document != within) {
                break Ok(());
            }
            tokio::select! {
                done = &mut navigation, if answered.is_none() => {
                    match done.map_err(|e| ChromiumError::Shape(e.to_string())).and_then(|r| r.map_err(ChromiumError::from)) {
                        Ok(Some(error)) => {
                            let dismissed = match &dialog_control {
                                Some(control) => control.inspect().await.map(|inventory| beforeunload_dismissed(&inventory.receipts, &prior_dialogs)),
                                None => Ok(false),
                            };
                            break match dismissed {
                                Ok(true) => Err(ChromiumError::NavigationCancelled),
                                Ok(false) => Err(ChromiumError::Navigation(error)),
                                Err(error) => Err(error),
                            };
                        },
                        Ok(None) => answered = Some(()),
                        Err(e) => break Err(e),
                    }
                }
                changed = async {
                    match &mut dialog_updates {
                        Some(updates) => updates.changed().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Err(error) = changed { break Err(ChromiumError::Shape(error.to_string())) }
                    let inventory = dialog_updates.as_mut().expect("subscribed branch").borrow_and_update().clone();
                    match inventory {
                        Ok(inventory) if beforeunload_dismissed(&inventory.receipts, &prior_dialogs) => break Err(ChromiumError::NavigationCancelled),
                        Err(error) => break Err(ChromiumError::Shape(error)),
                        _ => {},
                    }
                }
                ev = self.subs.paused.next() => {
                    let handled = match ev {
                        Ok(ev) => self.handle_request(ev).await,
                        Err(e) => Err(e.into()),
                    };
                    if let Err(e) = handled {
                        break Err(e);
                    }
                }
                ev = self.subs.navigated.next() => match ev {
                    Ok(ev) => if let Err(e) = self.take_navigated(ev).await { break Err(e) },
                    Err(e) => break Err(e.into()),
                },
                ev = self.subs.within.next() => match ev {
                    Ok(ev) => self.take_within(ev),
                    Err(e) => break Err(e.into()),
                },
                _ = service.timeout_at(deadline, std::future::pending::<()>()) => break Err(ChromiumError::CallTimeout("the navigation's commit")),
            }
        };
        self.own_navigation = false;
        if matches!(result, Err(ChromiumError::NavigationCancelled)) {
            self.drain().await?;
            self.commit()?;
            return result;
        }
        result?;
        self.drain().await?;
        self.commit()
    }

    /// A navigation the agent asked for: recorded as the action, then performed while frozen. `back` with
    /// no earlier history entry is refused before anything is dispatched.
    pub async fn navigate_action(&mut self, request: &RequestId, action: &ActAction) -> Result<(), ChromiumError> {
        self.alive()?;
        if self.held_pointer.is_some() { return Err(ChromiumError::InvalidArgument("end the held pointer before navigating".into())); }
        let to = self.navigation_of(action).await?;
        self.record_navigation(request, action);
        self.navigation_request = Some(request.to_string());
        let result = self.navigate_frozen(to).await;
        self.navigation_request = None;
        result
    }

    /// A navigation's action record, once per request.
    fn record_navigation(&mut self, request: &RequestId, action: &ActAction) {
        self.record_navigation_at(self.now, request, action);
    }

    pub(super) fn record_navigation_at(&mut self, t_us: VirtualTime, request: &RequestId, action: &ActAction) {
        self.record_untargeted(t_us, request, action, self.popup_id());
    }

    /// The record of an action without a target node, once per request.
    fn record_untargeted(&mut self, t_us: VirtualTime, request: &RequestId, action: &ActAction, context: Option<String>) {
        if self.recorded_operations.insert(request.clone()) {
            self.record(t_us, EventKind::Action(ActionRecord {
                received_events: None,
                pointer: None,
                keys: None,
                request_id: request.clone(),
                action: action.name().into(),
                target: None,
                args: action.args(),
                delivered_to: None,
                context,
            }));
        }
    }

    /// Whether the trace holds the navigation action. Recovery checks retained cancellation first;
    /// otherwise this is evidence of a navigation committed before the fault.
    pub fn navigation_recorded(&self, request: &RequestId) -> bool {
        self.recorded_operations.contains(request)
    }

    /// Continues a navigation a dead recorder dispatched. The browser started it if its top-frame document
    /// request is retained, paused, among the reconnect's unbound pauses, or if the dead recorder bound
    /// that request to this navigation and answered it. Either way it is handled as the action's
    /// navigation (recorded once) up to its commit, which may already have happened: taken while
    /// reconnecting, or made while no recorder listened and read from the frame tree. Returns
    /// false when there is no such request: the navigation never reached the browser, which starts
    /// every navigation with its request.
    pub async fn resume_navigation(&mut self, request: &RequestId, action: &ActAction) -> Result<bool, ChromiumError> {
        self.alive()?;
        let main = self.main_frame.clone();
        let retained = self.queued.iter().any(|(_, ev)| ev.resource_type == cnet::ResourceType::Document && ev.frame_id == main);
        let answered = if retained { None } else { self.answered_document_request(request)? };
        if !retained && answered.is_none() {
            return Ok(false);
        }
        self.record_navigation(request, action);
        if let Some((document, network)) = answered {
            if self.documents > document {
                self.commit()?;
                return Ok(true);
            }
            // Committed while no recorder listened: its `frameNavigated` went to the dead session. A
            // top-frame navigation's loader is its document request's network id.
            let frame = self.page.call(&page::GetFrameTreeParams::default()).await?.frame_tree.frame;
            if network.as_deref() == Some(frame.loader_id.0.as_str()) {
                self.take_navigated(page::FrameNavigatedEvent { frame, r#type: page::NavigationType::Navigation }).await?;
                self.drain().await?;
                self.commit()?;
                return Ok(true);
            }
        }
        let (documents, within) = (self.documents, self.within_document);
        self.own_navigation = true;
        self.navigation_request = Some(request.to_string());
        let result = self.await_commit(documents, within).await;
        self.own_navigation = false;
        self.navigation_request = None;
        result?;
        self.drain().await?;
        self.commit()?;
        Ok(true)
    }

    /// The document count from which `request`'s navigation left and its document request's network id,
    /// when the binding journal shows that top-frame request bound to it and answered.
    fn answered_document_request(&self, request: &RequestId) -> Result<Option<(u64, Option<String>)>, ChromiumError> {
        use crate::bindings::{BindingEntry, BindingJournal};
        let entries = BindingJournal::read(&self.bundle.root().join(INTERCEPTIONS))?;
        let bound = entries.iter().rev().find_map(|entry| match entry {
            BindingEntry::Bound(b) if b.document_request && b.navigation.as_deref() == Some(request.as_str()) => Some(b),
            _ => None,
        });
        Ok(bound.filter(|b| entries.iter().any(|entry| matches!(entry, BindingEntry::Dispatched { ordinal, .. } if ordinal == &b.ordinal)))
            .map(|b| (b.document, b.network_id.clone())))
    }

    /// Closes the active popup as its user would (`Page.close`): its `beforeunload` runs and the dialog
    /// rules answer a prompt. Recorded as the action first, then performed while frozen; requests its
    /// unload handlers send are handled meanwhile. Returns whether the window closed: it stays open when
    /// its prompt was answered by staying.
    pub async fn close_window(&mut self, request: &RequestId) -> Result<bool, ChromiumError> {
        self.alive()?;
        let Some(context) = self.popup_id() else {
            return Err(ChromiumError::InvalidArgument("close addresses a popup; the launch page ends with stop".into()));
        };
        // Staying is the window's `beforeunload` answered by dismissing it: a receipt its page did not have before.
        let receipts = |g: &Self| g.dialog_session().map(|d| d.receipts_of(Some(context.as_str()))).unwrap_or_default();
        let prior: std::collections::BTreeSet<String> = receipts(self).into_iter().map(|r| r.dialog.dialog_id).collect();
        let stays = |g: &Self| beforeunload_dismissed(&receipts(g), &prior);
        // Committed before it is sent: a recovery without it knows the browser never received the close.
        self.record_navigation(request, &ActAction::Close {});
        self.commit()?;
        let client = self.page.client.clone();
        let session = self.page.id.clone();
        let mut close = tokio::spawn(async move { client.call(Some(&session), &page::CloseParams {}).await });
        let service = self.page.service_clock();
        let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
        let mut sent = false;
        let closed = loop {
            if self.popup_is_closed() {
                break true;
            }
            if sent && stays(self) {
                break false;
            }
            tokio::select! {
                done = &mut close, if !sent => match done.map_err(|e| ChromiumError::Shape(e.to_string()))? {
                    Ok(_) => sent = true,
                    // Gone already: the close reached it.
                    Err(e) if contexts::session_gone(&e) => self.popup_closed(),
                    Err(e) => return Err(e.into()),
                },
                ev = self.subs.paused.next() => {
                    let ev = ev?;
                    self.handle_request(ev).await?;
                }
                // The browser's detach marks the window closed from another task.
                _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                _ = service.timeout_at(deadline, std::future::pending::<()>()) => return Err(ChromiumError::CallTimeout("Page.close")),
            }
        };
        // Its prompt (from the answer's receipt, the window gone or not), then its closing, at the action's instant.
        self.drain_native_dialog_receipts().await?;
        if closed {
            self.popup_closed();
            self.record_contexts();
        } else {
            self.popup_events().await?;
        }
        self.commit()?;
        Ok(closed)
    }

    /// Continues a close of `context` a dead recorder dispatched: whether its window is gone. A gone
    /// window's action and closing are recorded once; an open one cannot tell whether the close reached it.
    pub fn resume_close(&mut self, request: &RequestId, context: &str) -> Result<bool, ChromiumError> {
        if self.contexts.lock().unwrap().popups.iter().any(|p| p.id == context && !p.closed) {
            return Ok(false);
        }
        let at = self.now.max(self.log.last_time());
        self.record_untargeted(at, request, &ActAction::Close {}, Some(context.to_string()));
        self.record_contexts();
        self.commit()?;
        Ok(true)
    }

    /// Handles what a navigation in progress needs (its queued and new paused requests) until the top
    /// frame commits a new document or navigates within one, from the given counts.
    pub(super) async fn await_commit(&mut self, documents: u64, within: u64) -> Result<(), ChromiumError> {
        let service = self.page.service_clock();
        let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
        loop {
            if self.documents != documents || self.within_document != within {
                return Ok(());
            }
            if !self.queued.is_empty() {
                self.handle_queued().await?;
                continue;
            }
            tokio::select! {
                ev = self.subs.paused.next() => match ev {
                    Ok(ev) => self.handle_request(ev).await?,
                    Err(e) => return Err(e.into()),
                },
                ev = self.subs.navigated.next() => match ev {
                    Ok(ev) => self.take_navigated(ev).await?,
                    Err(e) => return Err(e.into()),
                },
                ev = self.subs.within.next() => match ev {
                    Ok(ev) => self.take_within(ev),
                    Err(e) => return Err(e.into()),
                },
                _ = service.timeout_at(deadline, std::future::pending::<()>()) => return Err(ChromiumError::CallTimeout("the resumed navigation's commit")),
            }
        }
    }

    /// Sends `action`'s navigation and abandons the connection before handling anything it causes, as a
    /// recorder that dies right after dispatching does.
    #[cfg(feature = "test-hooks")]
    pub async fn navigate_dispatch_and_abandon(mut self, action: &ActAction) -> Result<(), ChromiumError> {
        let Navigation::To(url) = self.navigation_of(action).await? else {
            return Err(ChromiumError::InvalidArgument("a navigation to a URL".into()));
        };
        let (client, session) = (self.page.client.clone(), self.page.id.clone());
        tokio::spawn(async move { let _ = client.call(Some(&session), &page::NavigateParams::new(url)).await; });
        // The browser starts the navigation and pauses its request; nothing here answers it.
        while self.subs.paused.next_timeout(Duration::from_millis(50)).await?.is_none() {}
        self.abandon();
        Ok(())
    }

    /// Whether `action` can navigate now, without dispatching anything: `back` needs an earlier entry.
    pub async fn navigation_check(&mut self, action: &ActAction) -> Result<(), ChromiumError> {
        self.alive()?;
        self.navigation_of(action).await.map(|_| ())
    }

    pub(super) async fn navigation_of(&mut self, action: &ActAction) -> Result<Navigation, ChromiumError> {
        Ok(match action {
            ActAction::Navigate { url } => Navigation::To(url.clone()),
            ActAction::Reload {} => Navigation::Reload,
            ActAction::Back {} => {
                let history = self.page.call(&page::GetNavigationHistoryParams::default()).await?;
                // A target starts at `about:blank` (entry 0), which precedes the generation's first document.
                let earlier = usize::try_from(history.current_index)
                    .ok()
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| history.entries.get(i).filter(|e| !(i == 0 && e.url == "about:blank")));
                Navigation::Entry(earlier.ok_or_else(|| ChromiumError::InvalidArgument("no earlier history entry".into()))?.id)
            }
            other => return Err(ChromiumError::Unsupported(format!("{} is not a navigation", other.name()))),
        })
    }

    /// Steps of `request` the platform sent but the page has not received (held moves a fault cut
    /// short): each was lost, never sent again.
    pub async fn input_unreceived(&mut self, request: &RequestId) -> Result<Vec<usize>, ChromiumError> {
        let id = serde_json::to_string(request.as_str()).unwrap();
        let v = self.world_eval(&format!("globalThis.__frontcap.unreceived({id})")).await?;
        serde_json::from_str(v.as_str().unwrap_or("[]")).map_err(|e| ChromiumError::Shape(e.to_string()))
    }

    /// Whether `request` started an HTML drag and drop, which the engine ran for the session it lost.
    pub async fn input_html_drag(&mut self, request: &RequestId) -> Result<bool, ChromiumError> {
        let id = serde_json::to_string(request.as_str()).unwrap();
        Ok(self.world_eval(&format!("globalThis.__frontcap.htmlDrag({id})")).await? == serde_json::Value::Bool(true))
    }

    pub async fn click(&mut self, request: RequestId, target: &Locator) -> Result<(), ChromiumError> {
        self.act_input(request, &ActAction::Click { target: target.clone() }).await
    }

    /// Plans an input action, dispatches every substep, and checks its recipients.
    pub async fn act_input(&mut self, request: RequestId, action: &ActAction) -> Result<(), ChromiumError> {
        let plan = self.input_plan(action).await?;
        for step in 0..plan.steps.len() {
            if let Err(e) = self.input_substep(&request, &plan, step).await {
                self.input_abort(&request, &plan, step).await;
                return Err(e);
            }
        }
        self.input_finish(&request, &plan).await
    }

    /// Every actionability check of an input action, without input: the steps it will dispatch.
    pub async fn input_plan(&mut self, action: &ActAction) -> Result<InputPlan, ChromiumError> {
        use crate::input::MouseKind::{Drag, Move, Press, Release};
        self.alive()?;
        if action.is_pointer() { return self.pointer_plan(action).await; }
        if matches!(action, ActAction::KeyDown { .. } | ActAction::KeyUp { .. } | ActAction::KeyRepeat { .. } | ActAction::Compose { .. } | ActAction::Commit { .. }) {
            return self.key_plan(action).await;
        }
        if self.held_pointer.is_some() { return Err(ChromiumError::InvalidArgument("end the held pointer before another input action".into())); }
        if matches!(action, ActAction::Type { .. } | ActAction::Press { .. } | ActAction::Fill { .. }) && self.composition().await?.is_some() {
            return Err(ChromiumError::InvalidArgument("commit or cancel the composition before other keyboard input".into()));
        }
        let mouse = |kind, (x, y): (f64, f64), count| InputStep::Mouse { buttons: None, kind, x, y, count, button: Default::default(), modifiers: 0 };
        // A user's mouse moves to what it clicks: a click where the mouse is not first moves it there.
        let mouse_at = self.mouse_at;
        let reach = |at: (f64, f64)| if mouse_at == Some(at) { vec![] } else { vec![mouse(Move, at, 0)] };
        let (node, steps) = match action {
            ActAction::PointerBegin { .. } | ActAction::PointerMove { .. } | ActAction::PointerPress { .. } | ActAction::PointerRelease { .. } | ActAction::TouchAdd { .. } | ActAction::TouchMove { .. } | ActAction::TouchEnd { .. } | ActAction::PointerEnd { .. } | ActAction::PointerCancel { .. } | ActAction::Wheel { .. }
            | ActAction::KeyDown { .. } | ActAction::KeyUp { .. } | ActAction::KeyRepeat { .. } | ActAction::Compose { .. } | ActAction::Commit { .. } => unreachable!("handled by pointer_plan and key_plan"),
            ActAction::ClickAt { target, point, boundary } => {
                let at = (f64::from(point[0]), f64::from(point[1]));
                self.checked_pointer_point(target, boundary, [at.0, at.1]).await?;
                let node = self.resolve(target).await?;
                (Some(node), [reach(at), vec![mouse(Press, at, 1), mouse(Release, at, 1)]].concat())
            }
            ActAction::Click { target } => {
                let (node, at) = self.pointer_target(target, true).await?;
                (Some(node), [reach(at), vec![mouse(Press, at, 1), mouse(Release, at, 1)]].concat())
            }
            ActAction::Dblclick { target } => {
                let (node, at) = self.pointer_target(target, true).await?;
                (Some(node), [reach(at), vec![mouse(Press, at, 1), mouse(Release, at, 1), mouse(Press, at, 2), mouse(Release, at, 2)]].concat())
            }
            ActAction::Hover { target } => {
                let (node, at) = self.pointer_target(target, false).await?;
                (Some(node), vec![mouse(Move, at, 0)])
            }
            ActAction::HoverAt { target, point, boundary } => {
                let at = (f64::from(point[0]), f64::from(point[1]));
                self.checked_pointer_point(target, boundary, [at.0, at.1]).await?;
                (Some(self.resolve(target).await?), vec![mouse(Move, at, 0)])
            }
            ActAction::Drag { target, to, offset, steps, interval_us } => {
                if *interval_us == 0 && !self.patched {
                    return Err(ChromiumError::Unsupported("frozen drag requires the patched input scheduler; specify an interval only when progress is intended".into()));
                }
                if *interval_us > 60_000_000 || *interval_us % 1000 != 0 {
                    return Err(ChromiumError::InvalidArgument("drag interval_us must be whole milliseconds from 0 to 60000000".into()));
                }
                let moves = steps.unwrap_or(5);
                if !(1..=100).contains(&moves) {
                    return Err(ChromiumError::InvalidArgument(format!("steps is 1 to 100, not {moves}")));
                }
                let (node, from) = self.pointer_target(target, true).await?;
                let dest = match (to, offset) {
                    (Some(to), None) => self.drop_point(to).await?,
                    (None, Some(o)) if o.dx == 0 && o.dy == 0 => return Err(ChromiumError::InvalidArgument("a drag by offset 0, 0 moves nowhere".into())),
                    (None, Some(o)) => (from.0 + f64::from(o.dx), from.1 + f64::from(o.dy)),
                    _ => return Err(ChromiumError::InvalidArgument("a drag takes exactly one of to and offset".into())),
                };
                let viewport = &self.profile.viewport;
                if dest.0 < 0.0 || dest.1 < 0.0 || dest.0 >= f64::from(viewport.width) || dest.1 >= f64::from(viewport.height) {
                    return Err(ChromiumError::NotActionable("the destination is offscreen".into()));
                }
                let mut plan = vec![mouse(Move, from, 0), mouse(Press, from, 1)];
                let mut last = from;
                for i in 1..=moves {
                    let f = f64::from(i) / f64::from(moves);
                    let at = ((from.0 + (dest.0 - from.0) * f).round(), (from.1 + (dest.1 - from.1) * f).round());
                    // A move to where the pointer is fires nothing to wait for.
                    if at == last {
                        continue;
                    }
                    last = at;
                    if *interval_us > 0 { plan.push(InputStep::Advance { by_us: *interval_us }); }
                    plan.push(mouse(Drag, at, 0));
                }
                plan.push(mouse(Release, dest, 1));
                (Some(node), plan)
            }
            ActAction::Upload { target, files } => {
                if files.is_empty() {
                    return Err(ChromiumError::InvalidArgument("an upload names at least one file".into()));
                }
                let node = self.resolve(target).await?;
                let obj = self.input_object(node.clone()).await?;
                let kind = self.call_on(&obj, "function() { return globalThis.__frontcap.is(this, 'input') && this.type === 'file' ? { disabled: this.disabled, multiple: this.multiple } : null; }", vec![]).await?;
                if kind.is_null() {
                    // A control that opens a file chooser: clicked, and the chooser's input gets the files.
                    let (node, at) = self.pointer_target(target, true).await?;
                    let mut set = Vec::new();
                    for f in files {
                        set.push(self.upload_copy(f)?);
                    }
                    while self.subs.chooser.try_next()?.is_some() {}
                    (Some(node), [reach(at), vec![mouse(Press, at, 1), mouse(Release, at, 1), InputStep::ChooseFiles { files: set }]].concat())
                } else {
                    if kind["disabled"] == serde_json::Value::Bool(true) {
                        return Err(ChromiumError::NotActionable("disabled".into()));
                    }
                    if files.len() > 1 && kind["multiple"] != serde_json::Value::Bool(true) {
                        return Err(ChromiumError::InvalidArgument(format!("the input takes one file, not {}", files.len())));
                    }
                    let mut set = Vec::new();
                    for f in files {
                        set.push(self.upload_copy(f)?);
                    }
                    (Some(node), vec![InputStep::SetFiles { files: set }])
                }
            }
            ActAction::Type { target, text } => {
                let node = self.keyboard_target(target, true).await?;
                let mut steps = vec![InputStep::Focus { at_end: true }];
                steps.extend(crate::keys::typed_text(text));
                (Some(node), steps)
            }
            ActAction::Press { target, key } => {
                let keys = crate::keys::chord(key).map_err(ChromiumError::InvalidArgument)?;
                match target {
                    Some(t) => {
                        let node = self.keyboard_target(t, false).await?;
                        (Some(node), std::iter::once(InputStep::Focus { at_end: false }).chain(keys).collect())
                    }
                    None => {
                        if self.world_eval("globalThis.__frontcap.unobservedFrameFocused()").await? == serde_json::Value::Bool(true) {
                            return Err(ChromiumError::NotActionable(format!("focus is in an unobserved frame: {UNOBSERVED_FRAME}")));
                        }
                        (None, keys)
                    }
                }
            }
            ActAction::Fill { target, value } => {
                let node = self.keyboard_target(target, true).await?;
                let mut steps = vec![InputStep::Focus { at_end: false }, InputStep::SelectContents];
                if value.is_empty() {
                    steps.extend(crate::keys::chord("Delete").expect("a named key"));
                } else {
                    steps.push(InputStep::InsertText { text: value.clone() });
                }
                (Some(node), steps)
            }
            ActAction::Select { target, values } => {
                let node = self.keyboard_target(target, false).await?;
                let obj = self.world_object(node.clone()).await?;
                let multiple = self.call_on(&obj, "function() { return globalThis.__frontcap.is(this, 'select') ? this.multiple : null; }", vec![]).await?;
                match multiple.as_bool() {
                    None => return Err(ChromiumError::NotActionable("not a select".into())),
                    Some(false) if values.len() != 1 => {
                        return Err(ChromiumError::InvalidArgument(format!("a single select takes one value, not {}", values.len())));
                    }
                    _ => {}
                }
                let f = format!("function() {{ const m = globalThis.__frontcap.matchOptions(this, {}); return m.chosen ? null : m; }}", serde_json::to_string(values).unwrap());
                let refused = self.call_on(&obj, &f, vec![]).await?;
                let text = |k: &str| refused[k].as_str().map(str::to_string);
                if let Some(m) = text("not_found") {
                    return Err(ChromiumError::NotFound(m));
                }
                if let Some(m) = text("not_actionable") {
                    return Err(ChromiumError::NotActionable(m));
                }
                (Some(node), vec![InputStep::SelectOptions { values: values.clone() }])
            }
            ActAction::Scroll { target, delta_x, delta_y } => {
                if !self.patched {
                    return Err(ChromiumError::Unsupported("scroll is a native wheel, which requires the patched input scheduler".into()));
                }
                if *delta_x == 0 && *delta_y == 0 {
                    return Err(ChromiumError::InvalidArgument("scroll needs a nonzero delta_x or delta_y".into()));
                }
                let (scroller, (x, y)) = self.wheel_point(target.as_ref(), *delta_x, *delta_y).await?;
                (Some(scroller), vec![InputStep::Wheel { x, y, dx: f64::from(*delta_x), dy: f64::from(*delta_y), modifiers: 0 }])
            }
            other => return Err(ChromiumError::Unsupported(format!("{} is not an input action", other.name()))),
        };
        let mut steps = steps;
        crate::input::hold_modifiers(&mut steps, self.held_modifiers());
        Ok(InputPlan { pointer_context: None, action: action.clone(), node: node.map(|n| n.0), steps, start_us: self.now, document: self.documents })
    }

    /// Where a wheel reaches `target`'s nearest scroller (the document's without a target) first, and that
    /// scroller: over the target when it is on screen, otherwise over a shown part of the scroller, as a
    /// user would place the pointer. The page's handlers then decide what the wheel scrolls.
    async fn wheel_point(&mut self, target: Option<&Locator>, dx: i32, dy: i32) -> Result<(dom::BackendNodeId, (f64, f64)), ChromiumError> {
        let unobserved = || ChromiumError::NotActionable(format!("the document is in an unobserved frame: {UNOBSERVED_FRAME}"));
        let (node, origin) = match target {
            Some(t) => {
                let node = self.resolve(t).await?;
                let obj = self.input_object(node.clone()).await?;
                if self.call_on(&obj, "function() { return globalThis.__frontcap.visible(this); }", vec![]).await? != serde_json::Value::Bool(true) {
                    return Err(ChromiumError::NotActionable("not visible".into()));
                }
                (Some(node), obj)
            }
            None => (None, self.page.eval_object("document.documentElement", self.world.clone()).await?.ok_or_else(unobserved)?),
        };
        let scroller_obj = self
            .call_for_object(&origin, &format!("function() {{ return globalThis.__frontcap.scrollerOf(this, {dx}, {dy}); }}"))
            .await?
            .ok_or_else(unobserved)?;
        let scroller = self.page.backend_id_of(scroller_obj.clone()).await?;
        let viewport = &self.profile.viewport;
        let (width, height) = (f64::from(viewport.width), f64::from(viewport.height));
        let center = |q: &[f64]| (((q[0] + q[2] + q[4] + q[6]) / 4.0).round(), ((q[1] + q[3] + q[5] + q[7]) / 4.0).round());
        let mut points = Vec::new();
        if let Some(node) = &node {
            let model = self.page.call(&dom::GetBoxModelParams { backend_node_id: Some(node.clone()), ..Default::default() }).await?.model;
            points.push(center(&model.content.0));
        }
        // The top document scrolls under the whole viewport; any other scroller under its shown box.
        let top_root = "function() { return this === document.scrollingElement; }";
        let (left, top, right, bottom) = if self.call_on(&scroller_obj, top_root, vec![]).await? == serde_json::Value::Bool(true) {
            (0.0, 0.0, width, height)
        } else {
            let q = self.page.call(&dom::GetBoxModelParams { backend_node_id: Some(scroller.clone()), ..Default::default() }).await?.model.border.0;
            let xs = [q[0], q[2], q[4], q[6]];
            let ys = [q[1], q[3], q[5], q[7]];
            let min = |v: [f64; 4]| v.into_iter().fold(f64::INFINITY, f64::min);
            let max = |v: [f64; 4]| v.into_iter().fold(f64::NEG_INFINITY, f64::max);
            (min(xs).max(0.0), min(ys).max(0.0), max(xs).min(width), max(ys).min(height))
        };
        if right > left && bottom > top {
            // The middle of what is shown first, then points nearest it.
            let mid = ((left + right) / 2.0, (top + bottom) / 2.0);
            let mut grid: Vec<(f64, f64)> = (0..25)
                .map(|k| ((left + (f64::from(k % 5) + 0.5) * (right - left) / 5.0).round(), (top + (f64::from(k / 5) + 0.5) * (bottom - top) / 5.0).round()))
                .collect();
            grid.sort_by(|a, b| (a.0 - mid.0).hypot(a.1 - mid.1).total_cmp(&(b.0 - mid.0).hypot(b.1 - mid.1)));
            points.extend(grid);
        }
        let first = format!("function(hit) {{ return globalThis.__frontcap.scrollsFirst(this, hit, {dx}, {dy}); }}");
        for (x, y) in points {
            if x < 0.0 || y < 0.0 || x >= width || y >= height {
                continue;
            }
            let hit = self.hit_test_viewport([x, y]).await?;
            let hit_obj = match self.world_object(hit.backend_node_id).await {
                Ok(o) => o,
                Err(ChromiumError::Unsupported(_)) => continue,
                Err(e) => return Err(e),
            };
            if self.call_on(&scroller_obj, &first, vec![hit_obj]).await? == serde_json::Value::Bool(true) {
                return Ok((scroller, (x, y)));
            }
        }
        let desc = self.call_on(&scroller_obj, DESCRIBE, vec![]).await?;
        Err(ChromiumError::NotActionable(format!("no point on screen reaches {} first: it is offscreen or covered; scroll what contains it, or wheel at a point with input wheel", desc.as_str().unwrap_or_default())))
    }

    /// Where a drag releases over `to`: the center of its box, which must be visible. No hit-test — the
    /// dragged element may cover it by then.
    pub(super) async fn drop_point(&mut self, to: &Locator) -> Result<(f64, f64), ChromiumError> {
        let node = self.resolve(to).await?;
        let obj = self.input_object(node.clone()).await?;
        if self.call_on(&obj, "function() { return globalThis.__frontcap.visible(this); }", vec![]).await? != serde_json::Value::Bool(true) {
            return Err(ChromiumError::NotActionable("the destination is not visible".into()));
        }
        let model = self.page.call(&dom::GetBoxModelParams { backend_node_id: Some(node), ..Default::default() }).await?.model;
        let q = &model.content.0;
        Ok((((q[0] + q[2] + q[4] + q[6]) / 4.0).round(), ((q[1] + q[3] + q[5] + q[7]) / 4.0).round()))
    }

    /// Copies a file to upload into the bundle (`inputs/uploads/<sha256>/<name>`), with its modification
    /// time at the profile's initial date, so the page reads the same file in every run.
    pub(super) fn upload_copy(&self, path: &str) -> Result<crate::input::UploadFile, ChromiumError> {
        let source = std::path::Path::new(path);
        let unreadable = |why: String| ChromiumError::InvalidArgument(format!("cannot read {path}: {why}"));
        let meta = std::fs::metadata(source).map_err(|e| unreadable(e.to_string()))?;
        if !meta.is_file() {
            return Err(unreadable("not a regular file".into()));
        }
        if meta.len() > UPLOAD_MAX_BYTES {
            return Err(unreadable(format!("{} bytes is over the {UPLOAD_MAX_BYTES}-byte limit", meta.len())));
        }
        let bytes = std::fs::read(source).map_err(|e| unreadable(e.to_string()))?;
        let name = source.file_name().and_then(|n| n.to_str()).ok_or_else(|| ChromiumError::InvalidArgument(format!("{path} has no file name")))?.to_string();
        let sha256 = frontcap_core::canonical::sha256_bytes_hex(&bytes);
        let dir = self.bundle.root().join(UPLOADS).join(&sha256);
        std::fs::create_dir_all(&dir)?;
        let copy = dir.join(&name);
        if !copy.exists() {
            // Whole or not at all: a copy is named by its content.
            let partial = dir.join(format!(".{name}.partial"));
            std::fs::write(&partial, &bytes)?;
            std::fs::rename(&partial, &copy)?;
        }
        let date = std::time::UNIX_EPOCH + Duration::from_millis(self.profile.initial_date_ms);
        std::fs::File::options().write(true).open(&copy)?.set_modified(date)?;
        Ok(crate::input::UploadFile { path: copy.display().to_string(), name, sha256, size: bytes.len() as u64 })
    }

    /// The node a keyboard action targets: attached, visible, enabled, and (for text entry) editable.
    /// Keyboard input has no point, so there is no hit-test.
    pub(super) async fn keyboard_target(&mut self, target: &Locator, needs_editable: bool) -> Result<dom::BackendNodeId, ChromiumError> {
        let node = self.resolve(target).await?;
        let obj = self.input_object(node.clone()).await?;
        for (check, reason, needed) in [("visible", "not visible", true), ("enabled", "disabled", true), ("editable", "not editable", needs_editable)] {
            let f = format!("function() {{ return globalThis.__frontcap.{check}(this); }}");
            if needed && self.call_on(&obj, &f, vec![]).await? != serde_json::Value::Bool(true) {
                return Err(ChromiumError::NotActionable(reason.into()));
            }
        }
        // The keys reach what takes focus: the target, or the editing host it is inside.
        let host = self
            .page
            .call(&runtime::CallFunctionOnParams {
                object_id: Some(obj),
                ..runtime::CallFunctionOnParams::new("function() { return globalThis.__frontcap.focusHost(this); }".into())
            })
            .await?;
        let Some(host) = host.result.object_id else { return Err(ChromiumError::NotActionable("not focusable".into())) };
        let described = self.page.call(&dom::DescribeNodeParams { object_id: Some(host), ..Default::default() }).await?;
        Ok(described.node.backend_node_id)
    }

    /// The node a pointer action targets and the point it uses: attached, visible, (enabled), inside
    /// the viewport, and hit at that point by the node or one of its descendants.
    pub(super) async fn pointer_target(&mut self, target: &Locator, needs_enabled: bool) -> Result<(dom::BackendNodeId, (f64, f64)), ChromiumError> {
        let node = self.resolve(target).await?;
        let target_obj = self.input_object(node.clone()).await?;
        if self.call_on(&target_obj, "function() { return globalThis.__frontcap.visible(this); }", vec![]).await? != serde_json::Value::Bool(true) {
            return Err(ChromiumError::NotActionable("not visible".into()));
        }
        if needs_enabled && self.call_on(&target_obj, "function() { return globalThis.__frontcap.enabled(this); }", vec![]).await? != serde_json::Value::Bool(true) {
            return Err(ChromiumError::NotActionable("disabled".into()));
        }
        let model = self.page.call(&dom::GetBoxModelParams { backend_node_id: Some(node.clone()), ..Default::default() }).await?.model;
        let q = &model.content.0;
        let (x, y) = (((q[0] + q[2] + q[4] + q[6]) / 4.0).round(), ((q[1] + q[3] + q[5] + q[7]) / 4.0).round());
        let viewport = &self.profile.viewport;
        let (width, height) = (f64::from(viewport.width), f64::from(viewport.height));
        if x < 0.0 || y < 0.0 || x >= width || y >= height {
            let delta = |at: f64, size: f64| if (0.0..size).contains(&at) { 0 } else { (at - (size / 2.0).floor()) as i32 };
            return Err(ChromiumError::Offscreen { delta_x: delta(x, width), delta_y: delta(y, height) });
        }
        // The hit-test reaches into child frames, so the hit is a node of the frame the point is in.
        let hit = self.hit_test_viewport([x, y]).await?;
        let hit_obj = match self.world_object(hit.backend_node_id).await {
            Ok(o) => o,
            Err(ChromiumError::Unsupported(_)) => return Err(ChromiumError::NotActionable(format!("the point is in an unobserved frame: {UNOBSERVED_FRAME}"))),
            Err(e) => return Err(e),
        };
        if self.call_on(&target_obj, ACCEPTED, vec![hit_obj.clone()]).await? != serde_json::Value::Bool(true) {
            let desc = self.call_on(&hit_obj, DESCRIBE, vec![]).await?;
            return Err(ChromiumError::NotActionable(format!("covered_by {}", desc.as_str().unwrap_or_default())));
        }
        Ok((node, (x, y)))
    }

    /// One low-level step of a plan, marked in the observer world so the page's receipt of it survives
    /// a recorder fault. Input is processed while frozen; the round trip after it makes sure its
    /// handlers have run.
    pub async fn input_substep(&mut self, request: &RequestId, plan: &InputPlan, step: usize) -> Result<(), ChromiumError> {
        self.alive()?;
        if step > 0 && matches!(plan.action, ActAction::Drag { .. }) && self.left_document(plan.document) {
            return Err(ChromiumError::NotActionable("the document changed during the drag".into()));
        }
        if self.acting.is_none() {
            self.commit()?;
            self.acting = Some((self.now, self.pending.len()));
        }
        // Engine patch 6: input is dispatched at once (moves and wheels wait for no frame); the page renders
        // at frame ticks only.
        self.pointer_started(request, plan);
        self.input_substep_inner(request, plan, step).await?;
        if let InputStep::Mouse { x, y, .. } | InputStep::Wheel { x, y, .. } = plan.steps[step] {
            self.mouse_at = Some((x, y));
        }
        if matches!(plan.action, ActAction::PointerMove { .. }) {
            self.finish_pointer_drag_update().await?;
        } else if plan.action.is_pointer() && self.input_html_drag(request).await? {
            return Err(ChromiumError::InputUncertain("unexpected HTML drag outside a mouse move".into()));
        }
        Ok(())
    }

    /// A drag's granted interval, run with the launch page addressed.
    async fn grant_drag_interval(&mut self, by_us: u64) -> Result<(), ChromiumError> {
        let wall = Instant::now();
        let goal = self.now.saturating_add(VirtualDuration(by_us));
        self.clock_record(ClockOp::Drag, ClockPhase::Start, goal, None, None);
        self.run_until(goal, None).await?;
        self.clock_record(ClockOp::Drag, ClockPhase::End, goal, Some(ClockOutcome::Reached), Some(wall));
        self.drain().await?;
        self.commit()
    }

    pub(super) async fn input_substep_inner(&mut self, request: &RequestId, plan: &InputPlan, step: usize) -> Result<(), ChromiumError> {
        let s = plan.steps.get(step).ok_or_else(|| ChromiumError::Shape(format!("{} has no substep {step}", plan.action.name())))?;
        let id = serde_json::to_string(request.as_str()).unwrap();
        // The events that receive a step whose page event is not the usual one; any listed event otherwise.
        let dragging = matches!(plan.action, ActAction::Drag { .. } | ActAction::PointerEnd { .. });
        let only = match s {
            InputStep::CancelMousePointer {} => r#"["pointercancel"]"#,
            InputStep::CancelHtmlDrag { .. } => r#"["dragend", "dragleave"]"#,
            InputStep::Wheel { .. } => r#"["wheel"]"#,
            InputStep::SetFiles { .. } | InputStep::ChooseFiles { .. } => r#"["change", "cancel"]"#,
            // A contact over another document is received by its pointer events; its touch events reach
            // the session's first document only when it lies inside that document's frame.
            InputStep::Touch { kind: crate::input::TouchKind::Start, other_document: true, .. } => r#"["pointerdown"]"#,
            InputStep::Touch { kind: crate::input::TouchKind::Move, other_document: true, .. } => r#"["pointermove", "touchmove"]"#,
            InputStep::Touch { kind: crate::input::TouchKind::End, other_document: true, .. } => r#"["pointerup", "touchend"]"#,
            InputStep::Touch { kind: crate::input::TouchKind::Cancel, other_document: true, .. } => r#"["pointercancel", "touchcancel"]"#,
            InputStep::Touch { kind: crate::input::TouchKind::Start, .. } => r#"["touchstart"]"#,
            InputStep::Touch { kind: crate::input::TouchKind::Move, .. } => r#"["touchmove"]"#,
            InputStep::Touch { kind: crate::input::TouchKind::End, .. } => r#"["touchend"]"#,
            InputStep::Touch { kind: crate::input::TouchKind::Cancel, .. } => r#"["touchcancel"]"#,
            InputStep::Compose { text } if text.is_empty() => r#"["compositionend"]"#,
            InputStep::Compose { .. } => r#"["compositionupdate"]"#,
            InputStep::CommitComposition { .. } => r#"["compositionend"]"#,
            InputStep::Mouse { .. } if s.receipt_event() == Some("pointermove") => r#"["pointermove"]"#,
            InputStep::Mouse { kind: crate::input::MouseKind::Drag, .. } => r#"["pointermove", "drag", "dragover"]"#,
            InputStep::Mouse { kind: crate::input::MouseKind::Release, .. } if dragging => r#"["pointerup", "dragend", "drop"]"#,
            _ => "null",
        };
        self.world_eval(&format!("globalThis.__frontcap.mark({id}, {step}, {only})")).await?;
        let script = |f: &str, extra: &str| format!("function() {{ return globalThis.__frontcap.{f}(this, {extra}{id}, {step}); }}");
        match s {
            InputStep::CancelHtmlDrag { x, y } => {
                self.page.call(&input::CancelDraggingParams::default()).await?;
                // A new CDP session has no browser DragController; the renderer may still
                // retain the source/target. Native dragCancel clears both under the patched engine.
                if self.world_eval(&format!("globalThis.__frontcap.received({id}, {step})")).await? != serde_json::Value::Bool(true) {
                    self.page.call(&input::DispatchDragEventParams::new(
                        input::DispatchDragEventParamsType::DragCancel, *x, *y,
                        input::DragData { items: vec![], files: None, drag_operations_mask: 0 },
                    )).await?;
                }
                self.page.call(&emulation::CancelMousePointerParams::default()).await?;
                let state = self.page.call(&DragStateQuery {}).await?;
                if state.active || state.pending_updates != 0 {
                    return Err(ChromiumError::InputUncertain("native HTML drag remained active after cancellation".into()));
                }
            }
            InputStep::CancelMousePointer {} => {
                if !self.page.call(&emulation::CancelMousePointerParams::default()).await?.cancelled {
                    return Err(ChromiumError::InputUncertain("the owned mouse pointer was not held by the engine".into()));
                }
            }
            InputStep::Mouse { kind: crate::input::MouseKind::Drag, .. } => {
                // Its receipt first: recovery must never send it again (a fault between the two loses one
                // intermediate move, never the release). Then wait for the delivery: a move queued behind
                // another is coalesced with it (S17).
                self.world_eval(&format!("globalThis.__frontcap.sent({id}, {step})")).await?;
                let answer = self.page.send(&s.mouse_params().expect("a mouse step"))?;
                self.drag_answers.push(answer);
                if !self.await_drag_receipt(&id, step).await? {
                    if matches!(plan.action, ActAction::Drag { interval_us: 0, .. } | ActAction::PointerMove { .. }) {
                        return Err(ChromiumError::CallTimeout("frozen pointer move delivery"));
                    }
                    // The renderer had no frame left at this instant; the move lands at the next one.
                    let detail = format!("{} move {step} of {} was not delivered at its instant", plan.action.name(), request.as_str());
                    self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::FrameStarvation, detail }));
                }
                self.check_drag_answers()?;
            }
            InputStep::Advance { by_us } => {
                self.world_eval(&format!("globalThis.__frontcap.sent({id}, {step})")).await?;
                self.world_eval("globalThis.__frontcap.unmark()").await?;
                // Only the caller's explicit progress grant; no automatic frame interval. It runs with the
                // launch page addressed, as all progress does; a popup's drag goes on in its window.
                let popup = self.popup_id();
                self.leave_context();
                let ran = self.grant_drag_interval(*by_us).await;
                if let Some(popup) = popup {
                    self.return_to(&popup)?;
                }
                ran?;
                // A navigation the drag started (a press or a move that leaves the page) loads in wall time:
                // committed, reported by the page, or its document request in flight. The rest of the drag
                // would meet another document.
                if self.popup_is_closed() {
                    return Err(ChromiumError::NotActionable("the window closed during the drag".into()));
                }
                if self.left_document(plan.document) {
                    return Err(ChromiumError::NotActionable("the document changed during the drag".into()));
                }
                return Ok(());
            }
            InputStep::Mouse { kind: crate::input::MouseKind::Release, .. } if dragging => {
                // The release flushes any move still queued; the drop and `dragend` follow it.
                let page = self.page.clone();
                self.serviced(page.call(&s.mouse_params().expect("a mouse step"))).await?;
                self.await_receipt(&id, step).await?;
                self.check_drag_answers()?;
            }
            InputStep::Mouse { .. } | InputStep::Wheel { .. } => {
                self.dispatch_input(&s.mouse_params().expect("a mouse step")).await?;
            }
            InputStep::ChooseFiles { files } => {
                let opened = match self.subs.chooser.try_next()? {
                    Some(ev) => Some(ev),
                    None => self.subs.chooser.next_timeout(STALL_WAIT).await?,
                };
                let Some(ev) = opened else {
                    self.world_eval("globalThis.__frontcap.unmark()").await?;
                    return Err(ChromiumError::NotActionable("clicking the target opened no file chooser".into()));
                };
                let node = ev.backend_node_id.ok_or_else(|| ChromiumError::NotActionable("the file chooser belongs to no input".into()))?;
                if files.len() > 1 && ev.mode == page::FileChooserOpenedEventMode::SelectSingle {
                    return Err(ChromiumError::InvalidArgument(format!("the input takes one file, not {}", files.len())));
                }
                let paths = files.iter().map(|f| f.path.clone()).collect();
                let page = self.page.clone();
                self.serviced(page.call(&dom::SetFileInputFilesParams { backend_node_id: Some(node), ..dom::SetFileInputFilesParams::new(paths) })).await?;
            }
            InputStep::SetFiles { files } => {
                let node = plan.node.ok_or_else(|| ChromiumError::Shape("an upload without a target".into()))?;
                let paths = files.iter().map(|f| f.path.clone()).collect();
                let page = self.page.clone();
                self.serviced(page.call(&dom::SetFileInputFilesParams { backend_node_id: Some(dom::BackendNodeId(node)), ..dom::SetFileInputFilesParams::new(paths) })).await?;
            }
            InputStep::Key { .. } => {
                self.dispatch_input(&s.key_params().expect("a key step")).await?;
            }
            InputStep::Touch { .. } => {
                let page = self.page.clone();
                self.serviced(page.call(&s.touch_params().expect("a touch step"))).await?;
            }
            InputStep::InsertText { text } | InputStep::CommitComposition { text } => {
                let page = self.page.clone();
                self.serviced(page.call(&input::InsertTextParams::new(text.clone()))).await?;
            }
            InputStep::Compose { text } => {
                let end = text.encode_utf16().count() as i64;
                let page = self.page.clone();
                self.serviced(page.call(&input::ImeSetCompositionParams::new(text.clone(), end, end))).await?;
            }
            InputStep::Focus { .. } | InputStep::SelectContents | InputStep::SelectOptions { .. } => {
                let node = plan.node.ok_or_else(|| ChromiumError::Shape("a script substep without a target".into()))?;
                let obj = self.world_object(dom::BackendNodeId(node)).await?;
                let f = match s {
                    InputStep::Focus { at_end } => script("focusTarget", &format!("{at_end}, ")),
                    InputStep::SelectContents => script("selectContents", ""),
                    InputStep::SelectOptions { values } => script("selectOptions", &format!("{}, ", serde_json::to_string(values).unwrap())),
                    _ => unreachable!(),
                };
                // Focus and selection run the page's handlers at once.
                let refused = self.call_on_page(&obj, &f).await?;
                if !refused.is_null() {
                    self.world_eval("globalThis.__frontcap.unmark()").await?;
                    let text = |k: &str| refused[k].as_str().map(str::to_string);
                    return Err(match (text("not_found"), text("not_actionable")) {
                        (Some(m), _) => ChromiumError::NotFound(m),
                        (_, Some(m)) => ChromiumError::NotActionable(m),
                        _ => ChromiumError::Shape(refused.to_string()),
                    });
                }
            }
        }
        // The round trip after input; a navigation the input started is intercepted meanwhile, since the
        // page waits for its document request.
        let documents = self.documents;
        self.round_trip().await?;
        if self.documents == documents && self.navigates.is_empty() && !self.popup_is_closed() {
            if matches!(s, InputStep::Touch { kind: crate::input::TouchKind::Cancel, .. }) {
                // Native cancellation can acknowledge before its DOM event arrives.
                // Keep the mark until receipt or the bounded wait expires; do not
                // resend input or advance virtual time. Missing delivery still fails
                // the existing receipt check after unmarking.
                self.await_receipt(&id, step).await?;
            }
            self.world_eval("globalThis.__frontcap.unmark()").await?;
        }
        Ok(())
    }

    /// A closing popup can detach before its pending native input reply arrives.
    /// Once sent, that completion uses the same closed-document boundary as an
    /// acknowledged input: its receipts left with the window. Refusal to send,
    /// protocol errors, crashes and transport loss still fail the action.
    async fn dispatch_input<C: cdp::Command>(&mut self, params: &C) -> Result<(), ChromiumError> {
        let answer = self.page.send(params)?;
        let service = self.page.service_clock();
        let result = self.serviced(async move {
            match service.timeout(Session::CALL_TIMEOUT, answer.answered()).await {
                Ok(answer) => Ok(answer?),
                Err(_) => Err(ChromiumError::CallTimeout(C::METHOD)),
            }
        }).await;
        match result {
            Err(ChromiumError::Cdp(cdp::CdpError::SessionDetached)) if self.popup_id().is_some() => {
                self.popup_closed();
                Ok(())
            }
            other => other,
        }
    }

    /// Evaluates nothing in the main world, handling the requests the page issues meanwhile: input that
    /// navigates leaves the page waiting for its document request, which the platform intercepts.
    pub(super) async fn round_trip(&mut self) -> Result<(), ChromiumError> {
        // `serviced` may have taken the navigation's interception while awaiting the input's reply.
        // No new paused event will arrive for it. Release queued work before asking the renderer to
        // answer: it may already be waiting for this document response.
        self.handle_queued().await?;
        let client = self.page.client.clone();
        let session = self.page.id.clone();
        let mut eval = tokio::spawn(async move {
            client.call(Some(&session), &runtime::EvaluateParams { return_by_value: Some(true), ..runtime::EvaluateParams::new("0".into()) }).await
        });
        let service = self.page.service_clock();
        let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
        loop {
            tokio::select! {
                done = &mut eval => {
                    return match done.map_err(|e| ChromiumError::Shape(e.to_string()))? {
                        Ok(_) => Ok(()),
                        // The document the input left is gone: nothing to wait for in it.
                        Err(_) if !self.navigates.is_empty() || !self.net.loading_documents.is_empty() => Ok(()),
                        // So is a popup the input closed.
                        Err(e) if self.popup_id().is_some() && contexts::session_gone(&e) => {
                            self.popup_closed();
                            Ok(())
                        }
                        Err(e) => Err(e.into()),
                    };
                }
                ev = self.subs.paused.next() => {
                    let ev = ev?;
                    self.handle_request(ev).await?;
                }
                _ = service.timeout_at(deadline, std::future::pending::<()>()) => return Err(ChromiumError::CallTimeout("Runtime.evaluate")),
            }
        }
    }

    /// Retains a page-issued navigation's interception before the input completion barrier starts.
    /// Exercises the ordering where `serviced` received it during the preceding input call.
    #[cfg(feature = "test-hooks")]
    pub async fn queued_navigation_round_trip(&mut self, url: &str) -> Result<(), ChromiumError> {
        let page = self.page.clone();
        let expression = format!("location.href = {}", serde_json::to_string(url).unwrap());
        let navigation = tokio::spawn(async move { page.eval(&expression, None).await });
        let result = async {
            let paused = self.subs.paused.next_timeout(Session::CALL_TIMEOUT).await?
                .ok_or(ChromiumError::CallTimeout("navigation interception"))?;
            self.queued.push((Instant::now(), paused));
            self.round_trip().await
        }.await;
        navigation.abort();
        let _ = navigation.await;
        result
    }

    /// Waits up to `STALL_WAIT` for a dispatched step's page receipt; whether it came.
    pub(super) async fn await_receipt(&mut self, id: &str, step: usize) -> Result<bool, ChromiumError> {
        let deadline = tokio::time::Instant::now() + STALL_WAIT;
        loop {
            if self.world_eval(&format!("globalThis.__frontcap.received({id}, {step})")).await? == serde_json::Value::Bool(true) {
                return Ok(true);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// Waits up to `STALL_WAIT` for a drag move's receipt: the page's event, or, while the engine runs a
    /// native HTML drag, the engine's own completion of the move (every move answered, no target update
    /// pending), when no page event received it. Over a document of another origin than the drag's
    /// source the engine dispatches no drag event at all (crbug.com/59081, as Chrome does): the move was
    /// delivered and nothing in the page saw it. The page's event, when there is one, runs before the
    /// engine completes the update, so a completion without a receipt means none.
    async fn await_drag_receipt(&mut self, id: &str, step: usize) -> Result<bool, ChromiumError> {
        let deadline = tokio::time::Instant::now() + STALL_WAIT;
        loop {
            if self.world_eval(&format!("globalThis.__frontcap.received({id}, {step})")).await? == serde_json::Value::Bool(true) {
                return Ok(true);
            }
            self.check_drag_answers()?;
            if self.patched && self.drag_answers.is_empty() {
                let state = self.page.call(&DragStateQuery {}).await?;
                if state.active && state.pending_updates == 0 {
                    return Ok(true);
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(false);
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    /// An HTML drag's source receipt can precede its target update. Wait for both the
    /// sent mouse acknowledgments and native target callbacks before exposing a stop.
    async fn finish_pointer_drag_update(&mut self) -> Result<(), ChromiumError> {
        let service = self.page.service_clock();
        let deadline = service.elapsed().saturating_add(Session::CALL_TIMEOUT);
        let native = loop {
            self.check_drag_answers()?;
            let state = self.page.call(&DragStateQuery {}).await?;
            if self.drag_answers.is_empty() && state.pending_updates == 0 { break state; }
            if service.elapsed() >= deadline { return Err(ChromiumError::CallTimeout("native drag update completion")); }
            tokio::time::sleep(Duration::from_millis(2)).await;
        };
        let Some(held) = self.held_pointer.as_mut() else { return Ok(()) };
        if native.active {
            if held.pointer != frontcap_core::pointer::PointerKind::Mouse || held.mouse_buttons() != [frontcap_core::pointer::PointerButton::Left] {
                return Err(ChromiumError::InputUncertain("HTML drag continuation requires a single held left button".into()));
            }
            held.html_drag = true;
        } else if held.html_drag {
            return Err(ChromiumError::InputUncertain("the owned native HTML drag ended outside its controller".into()));
        }
        Ok(())
    }

    /// Fails with the engine's answer to a held move that was refused; answered ones are dropped.
    pub(super) fn check_drag_answers(&mut self) -> Result<(), ChromiumError> {
        let mut pending = Vec::new();
        for mut a in std::mem::take(&mut self.drag_answers) {
            match a.try_answer() {
                None => pending.push(a),
                Some(Ok(())) => {}
                Some(Err(e)) => return Err(e.into()),
            }
        }
        self.drag_answers = pending;
        Ok(())
    }

    /// Releases what a plan that failed at `failed` holds: after a drag's press, the button (and an HTML
    /// drag the engine runs), so the next action does not land in a drag. Best effort; recorded as taint.
    pub async fn input_abort(&mut self, request: &RequestId, plan: &InputPlan, failed: usize) {
        self.acting = None;
        let pressed = plan.steps.iter().take(failed + 1).any(|s| matches!(s, InputStep::Mouse { kind: crate::input::MouseKind::Press, .. }));
        if !matches!(plan.action, ActAction::Drag { .. }) || !pressed {
            return;
        }
        let at = plan.steps.iter().take(failed + 1).rev().find_map(|s| match s {
            InputStep::Mouse { x, y, .. } => Some((*x, *y)),
            _ => None,
        });
        let _ = self.page.call(&input::CancelDraggingParams::default()).await;
        if let Some((x, y)) = at {
            // A renderer whose HTML drag lost the session that ran it still holds the pointer for it.
            let none = input::DragData { items: vec![], files: None, drag_operations_mask: 0 };
            let _ = self.page.call(&input::DispatchDragEventParams::new(input::DispatchDragEventParamsType::DragCancel, x, y, none)).await;
            let release = InputStep::Mouse { buttons: None, kind: crate::input::MouseKind::Release, x, y, count: 1, button: Default::default(), modifiers: 0 };
            let _ = self.page.call(&release.mouse_params().expect("a mouse step")).await;
        }
        self.drag_answers.clear();
        let detail = format!("{} of {} failed at substep {failed}; the button was released", plan.action.name(), request.as_str());
        self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::InputAborted, detail }));
        let _ = self.commit();
    }

    /// Checks the nodes that received the input and records the action.
    pub async fn input_finish(&mut self, request: &RequestId, plan: &InputPlan) -> Result<(), ChromiumError> {
        let acting = self.acting.take();
        self.alive()?;
        let id = serde_json::to_string(request.as_str()).unwrap();
        let expected = serde_json::to_string(&plan.expected()).unwrap();
        // Input that navigated (a link, a handler that leaves the page) took its receipts with the
        // document it left; its first event was checked there before dispatch.
        let left = self.documents != plan.document || !self.navigates.is_empty() || self.popup_is_closed();
        let delivered_to = match plan.node {
            _ if left => serde_json::Value::Null,
            Some(node) => {
                let target_obj = self.world_object(dom::BackendNodeId(node)).await?;
                let f = format!("function() {{ return globalThis.__frontcap.undelivered(this, {id}, {expected}); }}");
                self.call_on(&target_obj, &f, vec![]).await?
            }
            None => self.world_eval(&format!("globalThis.__frontcap.undelivered(null, {id}, {expected})")).await?,
        };
        let delivered_to = if left && plan.action.is_pointer() {
            Some("the pointer document changed before its native receipt could be verified".to_string())
        } else {
            delivered_to.as_str().map(str::to_string)
        };
        self.check_drag_answers()?;
        let received_events = if left {
            None
        } else {
            let mut value = self.world_eval(&format!("typeof globalThis.__frontcap.receivedEvents === 'function' ? globalThis.__frontcap.receivedEvents({id}) : null")).await?;
            // The native observer clock uses a decimal string to retain all u64 microseconds.
            for event in value.as_array_mut().into_iter().flatten() {
                if let Some(t) = event["t_us"].as_str() {
                    event["t_us"] = t.parse::<u64>()
                        .map_err(|e| ChromiumError::Shape(format!("input event time: {e}")))?.into();
                }
            }
            serde_json::from_value(value).map_err(|e| ChromiumError::Shape(format!("input event receipts: {e}")))?
        };
        if self.recorded_operations.insert(request.clone()) {
            if delivered_to.is_none() { self.pointer_finished(request, plan); }
            // At the instant it was dispatched: a request the page waited for may have moved the clock since
            // (engine patch 53). A drag's granted intervals keep it at its end, as before.
            let granted = plan.steps.iter().any(|s| matches!(s, InputStep::Advance { .. }));
            let at = acting.filter(|_| !granted).map_or(self.now, |(start, _)| start);
            self.record(at, EventKind::Action(ActionRecord {
                received_events,
                pointer: if plan.action.is_pointer() { self.held_pointer.clone() } else { None },
                keys: matches!(plan.action, ActAction::KeyDown { .. } | ActAction::KeyUp { .. } | ActAction::KeyRepeat { .. }).then(|| self.held_keys.clone()),
                request_id: request.clone(),
                action: plan.action.name().into(),
                target: plan.action.target().map(frontcap_core::canonical::canonical_of),
                args: upload_args(plan).or_else(|| drag_args(plan)).or_else(|| plan.action.args()),
                delivered_to: delivered_to.clone(),
                context: self.popup_id(),
            }));
            // Ahead of everything the input caused: at its instant, the action comes first.
            if let Some((_, before)) = acting {
                let action = self.pending.pop().expect("just recorded");
                self.pending.insert(before.min(self.pending.len()), action);
            }
        }
        self.drain().await?;
        // Requests the input sent are issued at this instant: handled here, they are the act's effects. One
        // the engine holds back until a frame (S15) is handled by the next advance, at its issue time.
        self.take_issued(Duration::from_millis(100)).await?;
        self.await_popup_navigations().await?;
        self.commit()?;
        if let Some(d) = delivered_to {
            return Err(ChromiumError::AtomicityViolation { delivered_to: d });
        }
        if plan.action.is_pointer() && self.left_document(plan.document) {
            return Err(ChromiumError::InputUncertain("the pointer document changed while completing native input".into()));
        }
        Ok(())
    }

    /// The substeps the page received for `request`, or `None` when the document that would hold the
    /// receipts is gone.
    pub async fn input_receipts(&mut self, request: &RequestId) -> Result<Option<Vec<usize>>, ChromiumError> {
        self.drain_bindings(Duration::ZERO).await?;
        let Some(world) = self.world.clone() else { return Ok(None) };
        let id = serde_json::to_string(request.as_str()).unwrap();
        let v = match self.page.eval(&format!("globalThis.__frontcap.receipts({id})"), Some(world)).await {
            Ok(v) => v,
            Err(ChromiumError::Cdp(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut steps: Vec<usize> = serde_json::from_str(v.as_str().unwrap_or("[]")).map_err(|e| ChromiumError::Shape(e.to_string()))?;
        steps.sort_unstable();
        Ok(Some(steps))
    }
}

fn beforeunload_dismissed(receipts: &[page::FrontcapDialogReceipt], prior: &std::collections::BTreeSet<String>) -> bool {
    receipts.iter().any(|receipt| !prior.contains(&receipt.dialog.dialog_id)
        && receipt.dialog.r#type == page::DialogType::Beforeunload && !receipt.accept
        && receipt.source == page::FrontcapDialogReceiptSource::Answered)
}

#[derive(serde::Serialize)]
struct DragStateQuery {}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeDragState {
    active: bool,
    pending_updates: u64,
}
impl cdp::Command for DragStateQuery {
    const METHOD: &'static str = "Input.frontcapGetDragState";
    type Returns = NativeDragState;
}
