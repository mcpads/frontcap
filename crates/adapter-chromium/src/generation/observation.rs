use super::*;

impl Generation {
    pub async fn validate_watch_predicate(&mut self, predicate: &Predicate) -> Result<(), ChromiumError> {
        self.validate_watch_predicate_in(None, predicate).await
    }

    /// `validate_watch_predicate` in `context`'s page (a popup), or the launch page's.
    pub async fn validate_watch_predicate_in(&mut self, context: Option<&str>, predicate: &Predicate) -> Result<(), ChromiumError> {
        let entered = self.enter_for(context)?;
        let valid = self.validate_watch_predicate_here(predicate).await;
        if entered { self.leave_context(); }
        valid
    }

    async fn validate_watch_predicate_here(&mut self, predicate: &Predicate) -> Result<(), ChromiumError> {
        predicate.validate().map_err(|e| ChromiumError::InvalidArgument(e.to_string()))?;
        if predicate.target().has_role() && (!self.patched || (self.world.is_some()
            && self.world_eval("typeof globalThis.__frontcapQueryRole === 'function'").await? != serde_json::Value::Bool(true))) {
            return Err(ChromiumError::Unsupported("role predicates require the patched observer role query; use a current engine or CSS, text, test id, or an observed reference".into()));
        }
        for id in predicate.target().refs() {
            self.resolve(&Locator::Ref { id: id.into() }).await?;
        }
        Ok(())
    }

    /// Retain node identity in the observer, which survives controller/recorder loss. Never turn a
    /// removed node or a new document into a fresh match for the same reference.
    async fn bind_watch_references(&mut self, predicate: &Predicate) -> Result<(), ChromiumError> {
        for id in predicate.target().refs() {
            let quoted = serde_json::to_string(id).unwrap();
            if self.world_eval(&format!("globalThis.__frontcap.hasReference({quoted})")).await? == serde_json::Value::Bool(true) { continue; }
            let node = match self.resolve(&Locator::Ref { id: id.into() }).await {
                Ok(node) => node,
                Err(ChromiumError::StaleRef(_)) => continue,
                Err(e) => return Err(e),
            };
            let object = self.world_object(node).await?;
            self.call_on(&object, &format!("function() {{ globalThis.__frontcap.bindReference({quoted}, this); return true; }}"), vec![]).await?;
        }
        Ok(())
    }

    /// Makes the current document's observer register `name` exactly when `predicate` is given — the page
    /// half of a watch change interrupted by a recorder fault. Future documents follow the registry.
    pub async fn sync_page_watch(&mut self, name: &WatchName, predicate: Option<&Predicate>) -> Result<(), ChromiumError> {
        let context = self.watches.context(name).map(str::to_string);
        self.sync_page_watch_in(context.as_deref(), name, predicate).await
    }

    /// `sync_page_watch` in `context`'s page (a popup), or the launch page's; a closed popup has none.
    pub async fn sync_page_watch_in(&mut self, context: Option<&str>, name: &WatchName, predicate: Option<&Predicate>) -> Result<(), ChromiumError> {
        let entered = match self.enter_for(context) {
            Ok(entered) => entered,
            Err(ChromiumError::ContextClosed(_)) => return Ok(()),
            Err(e) => return Err(e),
        };
        let synced = self.sync_page_watch_here(name, predicate).await;
        if entered { self.leave_context(); }
        synced
    }

    /// Unregisters `name` from every page that may still have it: the launch page and each open popup (a
    /// removal interrupted after the registry forgot where the watch was).
    pub async fn drop_page_watch(&mut self, name: &WatchName) -> Result<(), ChromiumError> {
        self.sync_page_watch_in(None, name, None).await?;
        for popup in self.open_popups() {
            self.sync_page_watch_in(Some(&popup), name, None).await?;
        }
        Ok(())
    }

    async fn sync_page_watch_here(&mut self, name: &WatchName, predicate: Option<&Predicate>) -> Result<(), ChromiumError> {
        self.drain().await?;
        // Before the first document starts, the registered new-document script owns installation.
        if self.world.is_none() { return Ok(()); }
        let quoted = serde_json::to_string(name.as_str()).unwrap();
        let present = self.world_eval(&format!("globalThis.__frontcap.hasWatch({quoted})")).await? == serde_json::Value::Bool(true);
        match (predicate, present) {
            (Some(p), false) => {
                self.bind_watch_references(p).await?;
                self.world_eval(&format!("globalThis.__frontcap.addWatch({quoted}, {})", serde_json::to_string(p).unwrap())).await?;
            }
            (None, true) => {
                self.world_eval(&format!("globalThis.__frontcap.removeWatch({quoted})")).await?;
            }
            _ => {}
        }
        self.drain().await?;
        self.commit()
    }

    /// Evaluates in the observer world, which exists once the first document has started.
    pub(super) async fn world_eval(&mut self, expr: &str) -> Result<serde_json::Value, ChromiumError> {
        self.drain().await?;
        let world = self.world.clone().ok_or(ChromiumError::DocumentNotStarted)?;
        // The observer shares the page's main thread: input it marks or reads may still be running the
        // page's handlers, which can wait on a synchronous request.
        let page = self.page.clone();
        self.serviced(page.eval(expr, Some(world))).await
    }

    /// Adds a new-document script to the observer world (top frame only) and returns its identifier.
    pub(super) async fn world_script(&self, expr: &str) -> Result<page::ScriptIdentifier, ChromiumError> {
        let guarded = format!("if (globalThis.__frontcap) {{ {expr}; }}");
        let r = self
            .page
            .call(&page::AddScriptToEvaluateOnNewDocumentParams { world_name: Some(WORLD.into()), ..page::AddScriptToEvaluateOnNewDocumentParams::new(guarded) })
            .await?;
        Ok(r.identifier)
    }

    pub async fn watch_add(&mut self, name: WatchName, predicate: Predicate) -> Result<SampleValue, ChromiumError> {
        self.watch_add_in(None, name, predicate).await
    }

    /// A watch sampled in `context` (a popup) or in the launch page: registered with that page's observer
    /// and every future document of it.
    pub async fn watch_add_in(&mut self, context: Option<&str>, name: WatchName, predicate: Predicate) -> Result<SampleValue, ChromiumError> {
        self.alive()?;
        let entered = self.enter_for(context)?;
        let added = self.watch_add_here(context, name, predicate).await;
        if entered { self.leave_context(); }
        added
    }

    /// Enters `context` for page work, or stays on the launch page; whether it entered.
    pub(super) fn enter_for(&mut self, context: Option<&str>) -> Result<bool, ChromiumError> {
        match context {
            Some(popup) => self.enter_context(popup).map(|()| true),
            None => Ok(false),
        }
    }

    async fn watch_add_here(&mut self, context: Option<&str>, name: WatchName, predicate: Predicate) -> Result<SampleValue, ChromiumError> {
        self.validate_watch_predicate(&predicate).await?;
        self.bind_watch_references(&predicate).await?;
        self.watches.add_in(name.clone(), predicate.clone(), context.map(str::to_string))?;
        let expr = format!(
            "globalThis.__frontcap.addWatch({}, {})",
            serde_json::to_string(name.as_str()).unwrap(),
            serde_json::to_string(&predicate).unwrap()
        );
        // Every future document registers the watch right after the observer, so its initial sample lands
        // at the document's start even when no document has run yet.
        let id = self.world_script(&expr).await?;
        self.watch_scripts.insert(name.clone(), id);
        self.drain().await?;
        let value = match self.world.clone() {
            Some(world) => {
                let v = self.page.eval(&expr, Some(world)).await?;
                self.drain().await?;
                serde_json::from_value(v.clone()).map_err(|_| ChromiumError::Shape(v.to_string()))?
            }
            None => SampleValue::Unknown,
        };
        self.commit()?;
        Ok(value)
    }

    /// A predicate's value now, read as a watch samples it, but neither kept nor recorded.
    pub async fn predicate_now(&mut self, predicate: &Predicate) -> Result<SampleValue, ChromiumError> {
        self.predicate_now_in(None, predicate).await
    }

    /// `predicate_now` in `context`'s page (a popup), or the launch page's.
    pub async fn predicate_now_in(&mut self, context: Option<&str>, predicate: &Predicate) -> Result<SampleValue, ChromiumError> {
        self.alive()?;
        let entered = self.enter_for(context)?;
        let value = self.predicate_now_here(predicate).await;
        if entered { self.leave_context(); }
        value
    }

    async fn predicate_now_here(&mut self, predicate: &Predicate) -> Result<SampleValue, ChromiumError> {
        self.validate_watch_predicate(predicate).await?;
        self.bind_watch_references(predicate).await?;
        let Some(world) = self.world.clone() else { return Ok(SampleValue::Unknown) };
        let expr = format!("globalThis.__frontcap.evaluatePredicate({})", serde_json::to_string(predicate).unwrap());
        let v = self.page.eval(&expr, Some(world)).await?;
        serde_json::from_value(v.clone()).map_err(|_| ChromiumError::Shape(v.to_string()))
    }

    pub async fn watch_remove(&mut self, name: &WatchName) -> Result<SampleValue, ChromiumError> {
        self.alive()?;
        let context = self.watches.context(name).map(str::to_string);
        self.watches.remove(name)?;
        let script = self.watch_scripts.remove(name);
        // A closed window took its observer and its watch with it.
        let entered = match self.enter_for(context.as_deref()) {
            Ok(entered) => entered,
            Err(ChromiumError::ContextClosed(_)) => {
                self.commit()?;
                return Ok(SampleValue::Unknown);
            }
            Err(e) => return Err(e),
        };
        let removed = self.watch_remove_here(name, script).await;
        if entered { self.leave_context(); }
        removed
    }

    async fn watch_remove_here(&mut self, name: &WatchName, script: Option<page::ScriptIdentifier>) -> Result<SampleValue, ChromiumError> {
        if let Some(id) = script {
            self.page.call(&page::RemoveScriptToEvaluateOnNewDocumentParams::new(id)).await?;
        }
        self.drain().await?;
        let value = match self.world.clone() {
            Some(world) => {
                let v = self.page.eval(&format!("globalThis.__frontcap.removeWatch({})", serde_json::to_string(name.as_str()).unwrap()), Some(world)).await?;
                self.drain().await?;
                // Recovery can retain the desired registry after page-side removal already succeeded.
                if v.is_null() { SampleValue::Unknown }
                else { serde_json::from_value(v.clone()).map_err(|_| ChromiumError::Shape(v.to_string()))? }
            }
            None => SampleValue::Unknown,
        };
        self.last_samples.remove(name);
        self.commit()?;
        Ok(value)
    }

    /// Advances virtual time until a click on `target` would pass every actionability check, judging now
    /// and at every frame tick, for at most `max`. A target that is missing or not actionable yet is
    /// waited for; any other error ends the wait.
    pub async fn wait_actionable(&mut self, target: &Locator, max: VirtualDuration) -> Result<WaitOutcome, ChromiumError> {
        self.wait_actionable_in(None, target, max).await
    }

    /// `wait_actionable` for a target in `context` (a popup), judged there; the clock runs every context
    /// with the launch page addressed, as any progress does.
    pub async fn wait_actionable_in(&mut self, context: Option<&str>, target: &Locator, max: VirtualDuration) -> Result<WaitOutcome, ChromiumError> {
        self.alive()?;
        let wall = Instant::now();
        let goal = self.now.saturating_add(max);
        self.clock_record(ClockOp::WaitActionable, ClockPhase::Start, goal, None, None);
        let interval = self.profile.frame_interval_us.max(1);
        let outcome = loop {
            let judged = match context {
                Some(popup) => {
                    self.enter_context(popup)?;
                    let judged = self.input_plan(&ActAction::Click { target: target.clone() }).await;
                    self.leave_context();
                    judged
                }
                None => self.input_plan(&ActAction::Click { target: target.clone() }).await,
            };
            let reason = match judged {
                Ok(_) => break WaitOutcome::Actionable { t: self.now },
                Err(ChromiumError::DocumentNotStarted) => "the document has not started yet".into(),
                Err(e @ (ChromiumError::NotFound(_) | ChromiumError::NotActionable(_) | ChromiumError::Offscreen { .. })) => e.to_string(),
                Err(e) => return Err(e),
            };
            if self.now >= goal {
                break WaitOutcome::TimedOut { t: self.now, reason };
            }
            let tick = VirtualTime((self.now.0 / interval + 1) * interval).min(goal);
            if let RunEnd::Cancelled = self.run_cancellable(tick, None).await? {
                break WaitOutcome::Cancelled { t: self.now };
            }
        };
        let recorded = match &outcome {
            WaitOutcome::Actionable { .. } => ClockOutcome::Actionable,
            WaitOutcome::TimedOut { reason, .. } => ClockOutcome::NotActionable { reason: reason.clone() },
            WaitOutcome::Cancelled { .. } => ClockOutcome::Cancelled,
        };
        self.clock_record(ClockOp::WaitActionable, ClockPhase::End, goal, Some(recorded), Some(wall));
        self.commit()?;
        Ok(outcome)
    }

    /// The one node `target` names. A frame scope resolves each frame element from the outermost in, each
    /// within the frames before it, then the target within the last.
    pub(super) async fn resolve(&mut self, target: &Locator) -> Result<dom::BackendNodeId, ChromiumError> {
        fn flatten<'a>(loc: &'a Locator, frames: &mut Vec<&'a Locator>) -> &'a Locator {
            match loc {
                Locator::Frame { frame, target } => {
                    let owner = flatten(frame, frames);
                    frames.push(owner);
                    flatten(target, frames)
                }
                other => other,
            }
        }
        let mut frames = Vec::new();
        let target = flatten(target, &mut frames);
        if !frames.is_empty() && matches!(target, Locator::Ref { .. }) {
            return Err(ChromiumError::InvalidArgument("a reference names one node already; it takes no frame scope".into()));
        }
        let mut scope = None;
        for frame in frames {
            let owner = self.resolve_in(frame, scope.as_ref()).await?;
            let node = self.page.call(&dom::DescribeNodeParams { backend_node_id: Some(owner.clone()), ..Default::default() }).await?.node;
            if !matches!(node.node_name.to_ascii_lowercase().as_str(), "iframe" | "frame" | "object" | "embed") {
                return Err(ChromiumError::NotFound(format!("a frame: {} names a {}", serde_json::to_string(frame).unwrap(), node.node_name.to_lowercase())));
            }
            let Some(id) = node.frame_id else {
                return Err(ChromiumError::NotFound(format!("a frame: {} names a {}", serde_json::to_string(frame).unwrap(), node.node_name.to_lowercase())));
            };
            scope = Some(Scope { owner, frame: id });
        }
        self.resolve_in(target, scope.as_ref()).await
    }

    /// The one node `target` (not a frame scope) names, in the whole page or in the frames `scope` shows.
    pub(super) async fn resolve_in(&mut self, target: &Locator, scope: Option<&Scope>) -> Result<dom::BackendNodeId, ChromiumError> {
        let describe = serde_json::to_string(target).unwrap();
        match target {
            Locator::Ref { id } => {
                self.drain().await?;
                let stale = || ChromiumError::StaleRef(id.clone());
                if !self.obs.refs.contains_key(id)
                    && let Some((_, tag)) = id.split_once('-')
                    && tag != self.ref_tag()
                {
                    return Err(ChromiumError::ForeignRef(id.clone()));
                }
                let entry = self.obs.refs.get(id).cloned().ok_or_else(stale)?;
                if entry.context != self.popup_id() {
                    let named = entry.context.as_deref().unwrap_or(contexts::PAGE);
                    return Err(ChromiumError::InvalidArgument(format!("{id} names a node in context {named}; address that context")));
                }
                if entry.document != self.documents {
                    return Err(stale());
                }
                let object = match self
                    .page
                    .call(&dom::ResolveNodeParams { backend_node_id: Some(dom::BackendNodeId(entry.backend)), execution_context_id: self.world.clone(), ..Default::default() })
                    .await
                {
                    // `null` in the observer world: a frame it cannot reach, not a node gone.
                    Ok(r) => r.object.object_id.ok_or_else(|| ChromiumError::Unsupported(format!("the node is in an unobserved frame: {UNOBSERVED_FRAME}")))?,
                    Err(_) => return Err(stale()),
                };
                if self.call_on(&object, "function() { return this.isConnected; }", vec![]).await? != serde_json::Value::Bool(true) {
                    return Err(stale());
                }
                Ok(dom::BackendNodeId(entry.backend))
            }
            Locator::Role { role, name } => {
                // Snapshots expose the inspector's internal Iframe role as iframe.
                let native_role = if role == "iframe" { "Iframe" } else { role.as_str() };
                // The trees snapshots read, every frame's, so a role and name seen in a snapshot resolve to
                // the same node. (`Accessibility.queryAXTree` after `getFullAXTree` can wait for a frame that
                // does not come while frozen.)
                let ids: Vec<dom::BackendNodeId> = self
                    .ax_trees(scope.map(|s| &s.frame))
                    .await?
                    .into_iter()
                    .flat_map(|t| t.nodes)
                    .filter(|n| !n.ignored && ax_string(&n.role) == native_role && name.as_ref().is_none_or(|want| ax_string(&n.name) == *want))
                    .filter_map(|n| n.backend_dom_node_id)
                    .collect();
                match ids.len() {
                    0 => Err(ChromiumError::NotFound(describe)),
                    1 => Ok(ids.into_iter().next().unwrap()),
                    count => Err(ChromiumError::Ambiguous { locator: describe, count }),
                }
            }
            Locator::Frame { .. } => unreachable!("flattened by `resolve`"),
            _ => {
                let count = match scope {
                    None => self.world_eval(&format!("globalThis.__frontcap.pick({describe})")).await?,
                    Some(s) => {
                        let owner = self.world_object(s.owner.clone()).await?;
                        let found = self.call_on(&owner, &format!("function() {{ return globalThis.__frontcap.pickIn(this, {describe}); }}"), vec![]).await?;
                        // -1: the frame's document is one the observer cannot reach.
                        if found.as_i64() == Some(-1) {
                            return Err(ChromiumError::Unsupported(format!("the frame scope shows an unobserved frame: {UNOBSERVED_FRAME}")));
                        }
                        found
                    }
                }
                .as_u64()
                .unwrap_or(0) as usize;
                match count {
                    0 => Err(ChromiumError::NotFound(describe)),
                    1 => {
                        let object = self
                            .page
                            .eval_object("globalThis.__frontcap.picked[0]", self.world.clone())
                            .await?
                            .ok_or_else(|| ChromiumError::NotFound(describe.clone()))?;
                        self.page.backend_id_of(object).await
                    }
                    count => Err(ChromiumError::Ambiguous { locator: describe, count }),
                }
            }
        }
    }

    /// The node as an object of the observer world. A node of a frame the world cannot reach (another
    /// origin, without engine patch 11) resolves to `null` there.
    pub(super) async fn world_object(&mut self, node: dom::BackendNodeId) -> Result<runtime::RemoteObjectId, ChromiumError> {
        self.drain().await?;
        let r = self
            .page
            .call(&dom::ResolveNodeParams { backend_node_id: Some(node), execution_context_id: self.world.clone(), ..Default::default() })
            .await?;
        r.object.object_id.ok_or_else(|| ChromiumError::Unsupported(format!("the node is in an unobserved frame: {UNOBSERVED_FRAME}")))
    }

    /// `node` as an object of the observer world for input: refused when it is in a frame the world cannot
    /// place on the page (`UNOBSERVED_FRAME`).
    pub(super) async fn input_object(&mut self, node: dom::BackendNodeId) -> Result<runtime::RemoteObjectId, ChromiumError> {
        self.observed_object(node).await.map_err(|e| match e {
            ChromiumError::Unsupported(m) => ChromiumError::NotActionable(m),
            e => e,
        })
    }

    /// `node` as an object of the observer world, which can place it on the page: `unsupported` for a node
    /// it cannot reach, or reaches only through a frame whose document it cannot read (`UNOBSERVED_FRAME`).
    pub(super) async fn observed_object(&mut self, node: dom::BackendNodeId) -> Result<runtime::RemoteObjectId, ChromiumError> {
        let obj = self.world_object(node).await?;
        if self.call_on(&obj, "function() { return globalThis.__frontcap.observed(this); }", vec![]).await? != serde_json::Value::Bool(true) {
            return Err(ChromiumError::Unsupported(format!("the node is in an unobserved frame: {UNOBSERVED_FRAME}")));
        }
        Ok(obj)
    }

    /// `call_on` for a function that runs page handlers, which may make the main thread wait on a
    /// synchronous request (`serviced`).
    pub(super) async fn call_on_page(&mut self, object: &runtime::RemoteObjectId, function: &str) -> Result<serde_json::Value, ChromiumError> {
        let page = self.page.clone();
        let params = runtime::CallFunctionOnParams {
            object_id: Some(object.clone()),
            return_by_value: Some(true),
            ..runtime::CallFunctionOnParams::new(function.into())
        };
        let r = self.serviced(page.call(&params)).await?;
        if let Some(ex) = r.exception_details {
            return Err(ChromiumError::Script(ex.text));
        }
        Ok(r.result.value.unwrap_or(serde_json::Value::Null))
    }

    pub(super) async fn call_on(&self, object: &runtime::RemoteObjectId, function: &str, args: Vec<runtime::RemoteObjectId>) -> Result<serde_json::Value, ChromiumError> {
        let r = self
            .page
            .call(&runtime::CallFunctionOnParams {
                object_id: Some(object.clone()),
                arguments: Some(args.into_iter().map(|a| runtime::CallArgument { object_id: Some(a), ..Default::default() }).collect()),
                return_by_value: Some(true),
                ..runtime::CallFunctionOnParams::new(function.into())
            })
            .await?;
        if let Some(ex) = r.exception_details {
            return Err(ChromiumError::Script(ex.text));
        }
        Ok(r.result.value.unwrap_or(serde_json::Value::Null))
    }

    /// The object `function` returns when called on `object`, in its world; none for a primitive.
    pub(super) async fn call_for_object(&self, object: &runtime::RemoteObjectId, function: &str) -> Result<Option<runtime::RemoteObjectId>, ChromiumError> {
        let r = self.page.call(&runtime::CallFunctionOnParams { object_id: Some(object.clone()), ..runtime::CallFunctionOnParams::new(function.into()) }).await?;
        if let Some(ex) = r.exception_details {
            return Err(ChromiumError::Script(ex.text));
        }
        Ok(r.result.object_id)
    }

    /// Six hex digits naming this generation in the references it issues, so a reference used with
    /// another generation (a parent's in its child, one child's in another) is refused, never resolved.
    pub(super) fn ref_tag(&self) -> String {
        frontcap_core::canonical::sha256_hex(self.launch_id.as_str())[..6].to_string()
    }

    pub(super) fn ref_for(&mut self, backend: i64) -> String {
        let key = (self.documents, backend);
        if let Some(r) = self.ref_of.get(&key) {
            return r.clone();
        }
        self.obs.next_ref += 1;
        let r = format!("e{}-{}", self.obs.next_ref, self.ref_tag());
        self.obs.refs.insert(r.clone(), RefEntry { document: self.documents, backend, context: self.popup_id() });
        self.ref_of.insert(key, r.clone());
        r
    }

    /// Forgets the names a replay bound from its recording (another generation's, or untagged older
    /// ones): once the recorded prefix is over, this generation names its nodes itself and refuses
    /// another's references. Watches keep the nodes their observer already holds.
    pub fn retire_bound_refs(&mut self) {
        let own = format!("-{}", self.ref_tag());
        self.obs.refs.retain(|r, _| r.ends_with(&own));
        self.ref_of.retain(|_, r| r.ends_with(&own));
    }

    pub fn observation_state(&self) -> ObservationState {
        let mouse = self.entered.as_ref().map_or(self.mouse_at, |(_, home)| home.mouse_at).map(|(x, y)| [x, y]);
        ObservationState { dom_base: self.dom_base, state_revision: self.state_revision, mouse, ..self.obs.clone() }
    }

    pub fn restore_observations(&mut self, state: ObservationState) {
        // A fault invalidates pre-fault expectations even when recovery found no later logged event.
        // The persisted high-water mark includes DOM changes not represented by behavior events.
        self.state_revision = self.state_revision.max(state.state_revision.saturating_add(1));
        self.take_observations(state);
    }

    /// After a planned controller change (letting the browser go for a process image): the page did not
    /// change, so boundaries observed before it stay current.
    pub fn continue_observations(&mut self, state: ObservationState) {
        self.state_revision = self.state_revision.max(state.state_revision);
        self.take_observations(state);
    }

    fn take_observations(&mut self, state: ObservationState) {
        self.dom_base = state.dom_base;
        self.ref_of = state.refs.iter().map(|(r, e)| ((e.document, e.backend), r.clone())).collect();
        self.mouse_at = state.mouse.map(|[x, y]| (x, y));
        self.obs = state;
    }

    /// The accessibility role and name of `node`, from the trees snapshots read.
    pub(super) async fn ax_identity(&mut self, node: i64) -> Result<Option<(String, String)>, ChromiumError> {
        let trees = self.ax_trees(None).await?;
        Ok(trees
            .into_iter()
            .flat_map(|t| t.nodes)
            .find(|n| !n.ignored && n.backend_dom_node_id.as_ref().map(|b| b.0) == Some(node))
            .map(|n| (ax_string(&n.role), ax_string(&n.name))))
    }

    /// Every frame's accessibility tree, the top frame's first and each child frame's after its parent's,
    /// with the element that shows it. A frame's tree holds its own document only (an iframe is a leaf in
    /// its parent's). A frame gone meanwhile is left out.
    pub(super) async fn ax_trees(&mut self, root: Option<&page::FrameId>) -> Result<Vec<FrameTree>, ChromiumError> {
        let mut tree = self.page.call(&page::GetFrameTreeParams::default()).await?.frame_tree;
        if let Some(root) = root {
            // The subtree of the scope's frame.
            let mut stack = vec![tree];
            let found = loop {
                let Some(t) = stack.pop() else { return Ok(Vec::new()) };
                if &t.frame.id == root {
                    break t;
                }
                stack.extend(t.child_frames.into_iter().flatten());
            };
            tree = found;
        }
        let mut frames = Vec::new();
        let mut stack = vec![tree];
        while let Some(t) = stack.pop() {
            frames.push((t.frame.id.clone(), t.frame.url.clone()));
            stack.extend(t.child_frames.into_iter().flatten().rev());
        }
        let mut trees = Vec::new();
        for (i, (frame, url)) in frames.into_iter().enumerate() {
            let owner = if i == 0 && root.is_none() {
                None
            } else {
                match self.page.call(&dom::GetFrameOwnerParams::new(frame.clone())).await {
                    Ok(o) => Some(o.backend_node_id.0),
                    Err(_) => continue,
                }
            };
            let nodes = match self.page.call(&accessibility::GetFullAxTreeParams { frame_id: Some(frame), ..Default::default() }).await {
                Ok(t) => t.nodes,
                Err(e) if i == 0 && root.is_none() => return Err(e),
                Err(_) => continue,
            };
            trees.push(FrameTree { owner, url, nodes });
        }
        Ok(trees)
    }

    /// Where reference `id` points, for re-execution without the snapshot that issued it: the element
    /// path from `documentElement` and the node's accessibility role and name. `None` when the node is
    /// not an element reachable that way (a text node, or inside a closed shadow root the observer was
    /// not told of: on the baseline engine).
    pub async fn resolution_of(&mut self, id: &str) -> Result<Option<frontcap_core::inputs::Resolution>, ChromiumError> {
        #[derive(serde::Deserialize)]
        struct ElementPath {
            frames: Vec<Vec<frontcap_core::inputs::PathStep>>,
            path: Vec<frontcap_core::inputs::PathStep>,
        }
        let backend = self.resolve(&Locator::Ref { id: id.to_string() }).await?;
        let node = backend.0;
        let object = self.world_object(backend).await?;
        let found: Option<ElementPath> = serde_json::from_value(self.call_on(&object, "function() { return globalThis.__frontcap.elementPath(this); }", vec![]).await?)
            .map_err(|e| ChromiumError::Shape(e.to_string()))?;
        let Some(found) = found else { return Ok(None) };
        let identity = self.ax_identity(node).await?;
        Ok(Some(frontcap_core::inputs::Resolution {
            document: self.documents,
            frames: found.frames,
            path: found.path,
            role: identity.as_ref().map(|(r, _)| r.clone()),
            name: identity.map(|(_, n)| n),
            context: self.popup_id(),
        }))
    }

    /// Issues reference `id` for the element `resolution` names in the current document, as the snapshot
    /// of the recorded run did. `stale_ref` when the document differs, the path leads nowhere, or the
    /// element there has another accessibility role or name — never a reference to another node.
    pub async fn bind_ref(&mut self, id: &str, resolution: &frontcap_core::inputs::Resolution) -> Result<(), ChromiumError> {
        self.alive()?;
        let stale = || ChromiumError::StaleRef(id.to_string());
        if resolution.document != self.documents {
            return Err(stale());
        }
        let frames = serde_json::to_string(&resolution.frames).unwrap();
        let path = serde_json::to_string(&resolution.path).unwrap();
        let found = self.world_eval(&format!("globalThis.__frontcap.pickPath({frames}, {path})")).await?;
        if found != serde_json::Value::Bool(true) {
            return Err(stale());
        }
        let object = self.page.eval_object("globalThis.__frontcap.picked[0]", self.world.clone()).await?.ok_or_else(stale)?;
        let backend = self.page.backend_id_of(object).await?.0;
        let identity = self.ax_identity(backend).await?;
        let recorded = (resolution.role.clone(), resolution.name.clone());
        if (identity.as_ref().map(|(r, _)| r.clone()), identity.map(|(_, n)| n)) != recorded {
            return Err(stale());
        }
        if let Some(old) = self.obs.refs.insert(id.to_string(), RefEntry { document: self.documents, backend, context: self.popup_id() }) {
            self.ref_of.remove(&(old.document, old.backend));
        }
        self.ref_of.insert((self.documents, backend), id.to_string());
        if let Some(n) = id.strip_prefix('e').and_then(|n| n.split('-').next()).and_then(|n| n.parse::<u64>().ok()) {
            self.obs.next_ref = self.obs.next_ref.max(n);
        }
        Ok(())
    }

    /// Writes an `observation` entry to the acquisition log, placed after the behavior log's last event.
    pub fn record_observation(&mut self, call: &str, summary: String) -> Result<(), ChromiumError> {
        let after = self.log.next_seq().saturating_sub(1);
        self.acquisition.append(after, self.now, AcquisitionKind::Observation(ObservationRecord { call: call.into(), summary }))?;
        Ok(())
    }

    /// Evaluates `expression` in the page's main world, recorded as taint: the platform cannot prove it
    /// leaves the page unchanged. A promise it returns is awaited at this instant: the page's work queued
    /// for the instant runs under zero-length grants (an IndexedDB request, a message), and one that needs
    /// a later instant is refused.
    pub async fn evaluate(&mut self, expression: &str) -> Result<serde_json::Value, ChromiumError> {
        self.evaluate_in_context(expression, None).await
    }

    /// Resolve a frame's current document on each call; never retain a world across its navigation.
    pub async fn evaluate_in_frame(&mut self, expression: &str, frame: &Locator) -> Result<serde_json::Value, ChromiumError> {
        self.alive()?;
        let root = self.resolve(&Locator::Frame {
            frame: Box::new(frame.clone()), target: Box::new(Locator::Document { selector: None }),
        }).await?;
        let frame = self.page.call(&dom::DescribeNodeParams { backend_node_id: Some(root), ..Default::default() }).await?
            .node.frame_id.ok_or_else(|| ChromiumError::NotFound("the frame's current document".into()))?;
        self.take_contexts()?;
        // The existing main-world inventory uses browser-unique identities: a replaced renderer
        // cannot redirect this evaluation by reusing a numeric execution-context id.
        let context = self.stream_contexts.get(&frame.0).cloned()
            .ok_or_else(|| ChromiumError::NotFound("the frame's current main world".into()))?;
        self.evaluate_in_context(expression, Some(context)).await
    }

    async fn evaluate_in_context(&mut self, expression: &str, context: Option<String>) -> Result<serde_json::Value, ChromiumError> {
        self.alive()?;
        self.record(self.now, EventKind::Taint(TaintRecord { source: TaintSource::ArbitraryEvaluate, detail: expression.to_string() }));
        self.commit()?;
        let page = self.page.clone();
        let watchdog = self.watchdog;
        let params = runtime::EvaluateParams { return_by_value: Some(false), await_promise: Some(false),
            unique_context_id: context, ..runtime::EvaluateParams::new(expression.into()) };
        // The expression may make the main thread wait on a synchronous request (engine patch 53).
        let evaluated = self.serviced(async { Ok(page.service_clock().timeout(watchdog, page.call(&params)).await) }).await?;
        let value = match evaluated {
            Ok(Ok(r)) => match r.exception_details {
                Some(ex) => Err(ChromiumError::Script(format!("{} {}", ex.text, ex.exception.and_then(|e| e.description).unwrap_or_default()))),
                None => self.evaluated_value(r.result).await,
            },
            Ok(Err(e)) => Err(e),
            Err(_) => return self.unresponsive(self.now).await,
        };
        self.drain().await?;
        self.commit()?;
        value
    }

    /// An evaluation's result by value; a promise's once it settles at this instant.
    async fn evaluated_value(&mut self, result: runtime::RemoteObject) -> Result<serde_json::Value, ChromiumError> {
        let Some(object) = result.object_id else { return Ok(result.value.unwrap_or(serde_json::Value::Null)) };
        let by_value = |object, declaration: &str| runtime::CallFunctionOnParams {
            object_id: Some(object), return_by_value: Some(true), ..runtime::CallFunctionOnParams::new(declaration.into())
        };
        if result.subtype != Some(runtime::RemoteObjectSubtype::Promise) {
            return Ok(self.page.call(&by_value(object, "function() { return this; }")).await?.result.value.unwrap_or(serde_json::Value::Null));
        }
        let slot = self.page.call(&runtime::CallFunctionOnParams {
            object_id: Some(object), return_by_value: Some(false),
            ..runtime::CallFunctionOnParams::new("function() { const s = { state: 'pending' }; this.then((v) => { s.state = 'settled'; s.value = v; }, (e) => { s.state = 'failed'; s.error = String(e && e.message || e); }); return s; }".into())
        }).await?.result.object_id.ok_or_else(|| ChromiumError::Shape("no promise slot".into()))?;
        for serving in 0..=8 {
            let outcome = self.page.call(&by_value(slot.clone(), "function() { return this; }")).await?.result.value.unwrap_or_default();
            match outcome["state"].as_str() {
                Some("settled") => return Ok(outcome["value"].clone()),
                Some("failed") => return Err(ChromiumError::Script(format!("the expression's promise failed: {}", outcome["error"].as_str().unwrap_or_default()))),
                _ if serving == 8 || !self.patched => break,
                _ => self.finish_current_deliveries().await?,
            }
        }
        Err(ChromiumError::Script("the expression's promise did not settle at this instant: it waits for a later one; advance the clock, then read what it left".into()))
    }

    /// The current document's accessibility tree, pruned (see `observe::prune`), at most `max_nodes`
    /// lines. Reading it changes neither the page nor virtual time.
    pub async fn snapshot(&mut self, scope: Option<&Locator>, depth: Option<u32>, max_nodes: u32) -> Result<Snapshot, ChromiumError> {
        self.snapshot_with_geometry(scope, depth, max_nodes, false, true).await
    }

    /// Opt-in geometry is retained for every node, including ones omitted from the returned page.
    /// Unless `unfold`, runs of siblings of one shape are folded (`fold_repeats`) before the limit.
    pub async fn snapshot_with_geometry(&mut self, scope: Option<&Locator>, depth: Option<u32>, max_nodes: u32, geometry: bool, unfold: bool) -> Result<Snapshot, ChromiumError> {
        let mut full = self.full_snapshot(scope, depth, geometry).await?;
        if !unfold {
            // A folded run keeps the rows on screen too, so a scrolled page reads as its image shows.
            let runs = frontcap_core::observation::fold_candidates(&full.nodes);
            let on_screen = self.on_screen(&full.nodes, &runs).await?;
            full.nodes = frontcap_core::observation::fold_repeats_keeping(&full.nodes, |i| on_screen.contains(&i));
        }
        let omitted = full.nodes.len().saturating_sub(max_nodes as usize) as u32;
        let mut shown = full;
        shown.nodes.truncate(max_nodes as usize);
        shown.omitted = omitted;
        Ok(shown)
    }

    /// Up to the first three siblings of each run (`fold_candidates`) whose border box meets the viewport.
    /// Rows of one shape lie in document order down the page: a run's first that reaches below the
    /// viewport's top is found by halving, and those after it are taken while they are on screen.
    async fn on_screen(&mut self, nodes: &[SnapshotNode], runs: &[Vec<usize>]) -> Result<std::collections::HashSet<usize>, ChromiumError> {
        let (width, height) = (f64::from(self.profile.viewport.width), f64::from(self.profile.viewport.height));
        let mut shown = std::collections::HashSet::new();
        for run in runs {
            let (mut lo, mut hi) = (0, run.len());
            while lo < hi {
                let mid = (lo + hi) / 2;
                match self.border_box(&nodes[run[mid]]).await? {
                    Some([_, _, _, bottom]) if bottom > 0.0 => hi = mid,
                    _ => lo = mid + 1,
                }
            }
            for &i in run[lo..].iter().take(frontcap_core::observation::FOLD_KEEP) {
                match self.border_box(&nodes[i]).await? {
                    Some([left, top, right, bottom]) if top < height && bottom > 0.0 && left < width && right > 0.0 => { shown.insert(i); }
                    _ => break,
                }
            }
        }
        Ok(shown)
    }

    /// A node's border box `[left, top, right, bottom]` in top-viewport CSS pixels, or none without one.
    async fn border_box(&mut self, node: &SnapshotNode) -> Result<Option<[f64; 4]>, ChromiumError> {
        use frontcap_core::observation::SnapshotGeometry;
        let quad = match &node.geometry {
            Some(SnapshotGeometry::Box { border_quad }) => *border_quad,
            Some(SnapshotGeometry::NoBox) => return Ok(None),
            None => {
                let Some(entry) = self.obs.refs.get(&node.r#ref) else { return Ok(None) };
                match self.page.call(&dom::GetBoxModelParams { backend_node_id: Some(dom::BackendNodeId(entry.backend)), ..Default::default() }).await {
                    Ok(model) => match <[f64; 8]>::try_from(model.model.border.0) { Ok(q) => q, Err(_) => return Ok(None) },
                    Err(ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. })) if message.contains("Could not compute box model") => return Ok(None),
                    Err(e) => return Err(e),
                }
            }
        };
        let xs = [quad[0], quad[2], quad[4], quad[6]];
        let ys = [quad[1], quad[3], quad[5], quad[7]];
        let min_max = |v: [f64; 4]| (v.iter().copied().fold(f64::INFINITY, f64::min), v.iter().copied().fold(f64::NEG_INFINITY, f64::max));
        let ((left, right), (top, bottom)) = (min_max(xs), min_max(ys));
        Ok(Some([left, top, right, bottom]))
    }

    /// A snapshot without the size limit, kept as an observation for later diffs.
    pub(super) async fn full_snapshot(&mut self, scope: Option<&Locator>, depth: Option<u32>, geometry: bool) -> Result<Snapshot, ChromiumError> {
        self.alive()?;
        self.drain().await?;
        let boundary = if geometry { Some(self.observation_boundary().await?) } else { None };
        let scope_backend = match scope {
            Some(l) => Some(self.resolve(l).await?.0),
            None => None,
        };
        let trees = self.ax_trees(None).await?;
        let mut inputs = crate::observe::stitch(trees.iter().map(|t| (t.owner, t.nodes.iter().filter(|n| n.backend_dom_node_id.is_some()).map(ax_input).collect())).collect());
        // A frame element's value is the address of the document it shows (the browser's error page for a
        // refused one), its first 256 characters: a `data:` address can hold a whole document.
        for t in &trees {
            if t.url.is_empty() { continue }
            if let Some(n) = inputs.iter_mut().find(|n| t.owner.is_some() && n.backend == t.owner && n.role == "Iframe") {
                n.value.get_or_insert_with(|| match t.url.char_indices().nth(256) {
                    Some((cut, _)) => format!("{}…", &t.url[..cut]),
                    None => t.url.clone(),
                });
            }
        }
        let (kept, _) = crate::observe::prune_with_text_merging(&inputs, scope_backend, depth, u32::MAX, !geometry);
        let mut nodes = Vec::with_capacity(kept.len());
        for k in kept {
            let Some(backend) = k.backend else { continue };
            let geometry = if geometry {
                use frontcap_core::observation::SnapshotGeometry;
                Some(match self.page.call(&dom::GetBoxModelParams { backend_node_id: Some(dom::BackendNodeId(backend)), ..Default::default() }).await {
                    Ok(model) => {
                        let quad: [f64; 8] = model.model.border.0.try_into().map_err(|_| ChromiumError::Shape("border quad must contain eight coordinates".into()))?;
                        if quad.iter().any(|v| !v.is_finite()) { return Err(ChromiumError::Shape("non-finite border quad".into())); }
                        SnapshotGeometry::Box { border_quad: quad }
                    }
                    Err(ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. })) if message.contains("Could not compute box model") => SnapshotGeometry::NoBox,
                    Err(e) => return Err(e),
                })
            } else { None };
            nodes.push(SnapshotNode { r#ref: self.ref_for(backend), role: k.role, name: k.name, value: k.value, depth: k.depth, states: k.states, geometry, folded: None });
        }
        if let Some(expected) = &boundary {
            self.expect_boundary(expected).await?;
        }
        self.obs.next_observation += 1;
        let snapshot = Snapshot { observation_id: self.obs.next_observation, document: self.documents, nodes, omitted: 0, scope: scope.cloned(), depth, geometry, boundary, presentation: None };
        self.obs.snapshots.push_back(snapshot.clone());
        while self.obs.snapshots.len() > KEPT_SNAPSHOTS {
            self.obs.snapshots.pop_front();
        }
        Ok(snapshot)
    }

    /// What changed since observation `since` (one of this document's last 16), with that snapshot's
    /// scope and depth, as a new observation. Both sides are compared whole; the size limit applies to
    /// the result.
    pub async fn diff(&mut self, since: u64, max_nodes: u32, unfold: bool) -> Result<Diff, ChromiumError> {
        self.alive()?;
        self.drain().await?;
        let old = self
            .obs
            .snapshots
            .iter()
            .find(|s| s.observation_id == since && s.document == self.documents)
            .cloned()
            .ok_or(ChromiumError::StaleCursor(since))?;
        let new = self.full_snapshot(old.scope.as_ref(), old.depth, old.geometry).await?;
        let crate::observe::NodeDiff { mut added, mut removed, mut changed, replaced } = crate::observe::diff_nodes(&old.nodes, &new.nodes);
        if !unfold {
            added = frontcap_core::observation::fold_runs(added);
            changed = frontcap_core::observation::fold_runs(changed);
        }
        let mut replaced: Vec<Replaced> = replaced.into_iter().map(|(was, r#ref)| Replaced { was, r#ref }).collect();
        let total = added.len() + removed.len() + changed.len() + replaced.len();
        let mut room = max_nodes as usize;
        let take = |n: usize, room: &mut usize| {
            let k = n.min(*room);
            *room -= k;
            k
        };
        let (a, r, c) = (take(added.len(), &mut room), take(removed.len(), &mut room), take(changed.len(), &mut room));
        let p = take(replaced.len(), &mut room);
        added.truncate(a);
        removed.truncate(r);
        changed.truncate(c);
        replaced.truncate(p);
        Ok(Diff { observation_id: new.observation_id, since, added, removed, changed, replaced, omitted: (total - a - r - c - p) as u32, boundary: new.boundary })
    }

    /// Reads only the requested properties of the one node `target` names.
    /// How many nodes `target` matches: none, one, or the count an ambiguous match names.
    pub async fn count(&mut self, target: &Locator) -> Result<u32, ChromiumError> {
        self.alive()?;
        match self.resolve(target).await {
            Ok(_) => Ok(1),
            Err(ChromiumError::NotFound(_)) => Ok(0),
            Err(ChromiumError::Ambiguous { count, .. }) => Ok(count as u32),
            Err(e) => Err(e),
        }
    }

    pub async fn query(&mut self, target: &Locator, props: &[Prop]) -> Result<QueryResult, ChromiumError> {
        self.alive()?;
        let node = self.resolve(target).await?;
        let object = self.observed_object(node.clone()).await?;
        let keys: Vec<String> = props.iter().map(Prop::key).collect();
        let f = format!("function() {{ return globalThis.__frontcap.query(this, {}); }}", serde_json::to_string(&keys).unwrap());
        // The observer's own failure is the platform's, not the page's or the agent's.
        let v = self.call_on(&object, &f, vec![]).await.map_err(|e| match e {
            ChromiumError::Script(detail) => ChromiumError::Shape(format!("the observer could not read {keys:?}: {detail}")),
            e => e,
        })?;
        let mut values: BTreeMap<String, serde_json::Value> = serde_json::from_value(v).map_err(|e| ChromiumError::Shape(e.to_string()))?;
        // The box as laid out on the page: the engine's border quad, in the top frame's viewport with every
        // transform of the node and of the frames it is shown in applied (scaled, rotated, scrolled), as
        // the bounds of its corners. A node without a box keeps the observer's (empty) one.
        let model = if props.contains(&Prop::Bbox) {
            match self.page.call(&dom::GetBoxModelParams { backend_node_id: Some(node.clone()), ..Default::default() }).await {
                Ok(m) => Some(m),
                // The engine's answer for a node that is not laid out.
                Err(ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. })) if message.contains("Could not compute box model") => None,
                Err(e) => return Err(e),
            }
        } else {
            None
        };
        if let Some(model) = model {
            let q = &model.model.border.0;
            let xs = [q[0], q[2], q[4], q[6]];
            let ys = [q[1], q[3], q[5], q[7]];
            let (x0, x1) = (xs.iter().copied().fold(f64::INFINITY, f64::min), xs.iter().copied().fold(f64::NEG_INFINITY, f64::max));
            let (y0, y1) = (ys.iter().copied().fold(f64::INFINITY, f64::min), ys.iter().copied().fold(f64::NEG_INFINITY, f64::max));
            values.insert("bbox".into(), serde_json::json!({"x": x0, "y": y0, "width": x1 - x0, "height": y1 - y0}));
        }
        Ok(QueryResult { r#ref: self.ref_for(node.0), values, presentation: None })
    }

    /// `Pending` when the page changed after its last frame, so what an observation reads is not on the
    /// screen yet; none otherwise.
    pub async fn unpresented(&mut self) -> Result<Option<frontcap_core::observation::PresentationRelation>, ChromiumError> {
        use frontcap_core::observation::PresentationRelation;
        Ok((self.presented_relation().await? == PresentationRelation::Pending).then_some(PresentationRelation::Pending))
    }
}
