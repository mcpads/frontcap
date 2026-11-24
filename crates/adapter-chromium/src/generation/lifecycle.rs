use super::*;

impl Generation {
    pub async fn launch(o: LaunchOptions) -> Result<Generation, ChromiumError> {
        Self::launch_with_shared_activation(o, true).await
    }

    /// Native protocol tests own registration, activation and grants themselves.
    /// Ordinary generations always activate shared time through `launch`.
    #[cfg(feature = "test-hooks")]
    pub async fn test_launch_before_shared_activation(o: LaunchOptions) -> Result<Generation, ChromiumError> {
        Self::launch_with_shared_activation(o, false).await
    }

    async fn launch_with_shared_activation(mut o: LaunchOptions, activate_shared: bool) -> Result<Generation, ChromiumError> {
        frontcap_core::rules::validate_passthrough_origins(o.passthrough_origins.as_deref().unwrap_or_default())?;
        buckets::validate_initial_buckets(&o.profile.initial_buckets)?;
        cache::validate_initial_caches(&o.profile.initial_caches)?;
        indexed_db::validate_initial_databases(&o.profile.initial_indexed_db)?;
        intervention::validate_initial_storage(o.profile.initial_storage.as_ref())?;
        crate::dom_storage::validate_initial_dom_storage(&o.profile)?;
        cookies::validate_initial_cookies(&o.profile.initial_cookies, o.profile.initial_date_ms)?;
        super::validate_locale(&o.profile.locale)?;
        let (binary, engine_record, lacks) = engine(&o.repo_root)?;
        let patched = engine_record.patched;
        // Without the frame rate limit a native frame takes well under a millisecond and the document
        // timeline reads virtual time at each frame (S10).
        let mut args = vec!["--disable-frame-rate-limit".to_string(), "--disable-gpu-vsync".to_string()];
        // After input the renderer defers its other tasks (a response's included) until the next frame
        // (S15); frames come only where the platform renders, so the tasks must not wait for one. Paint
        // holding keeps a navigation's old content until the new one paints or a wall-clock timeout ends.
        // (One `--disable-features` only: the engine keeps the last.)
        args.push("--disable-features=DeferRendererTasksAfterInput,PaintHolding".to_string());
        // Request a shared renderer clock. Another process has an independent scheduler that the
        // launch page's grants do not control. Keep site-isolation trials off while clock admission
        // relies on the shared scheduler.
        args.push("--disable-site-isolation-trials".to_string());
        // Request the same renderer for popups too. This configuration is not proof that every
        // isolation-policy combination shares its clock; mixed-policy admission remains unqualified.
        args.push("--renderer-process-limit=1".to_string());
        // Scrolls the compositor would animate (keys, wheels) jump at once: its thread runs by the real clock.
        args.push("--disable-smooth-scrolling".to_string());
        // The page's languages (`navigator.language`, `Accept-Language`) are the profile's: without them the
        // headless shell takes the host process's locale (`en-US@posix` under `LANG=C`). The locale override
        // below covers `Intl` only.
        args.push(format!("--accept-lang={}", o.profile.locale));
        // A window the page opens (a link or form aimed at a new window, `window.open`) would run outside
        // the platform's clock, rules and evidence: unless the generation enrolls it (`contexts`), the
        // browser creates none, and each attempt is recorded (`Page.windowOpen`).
        // On the baseline engine they are refused: enrollment needs the patched engine (the baseline taint
        // says so).
        if !patched {
            o.profile.popups = frontcap_core::profile::PopupPolicy::Block;
        }
        if o.profile.popups.is_block() {
            args.push("--block-new-web-contents".to_string());
        }
        // The patched engine prepares document-owned AX caches before scripts, including child
        // frames. Browser-wide forced accessibility also schedules platform serialization work.
        if patched {
            // Engine patch 2: the page's clock is coarsened from the run's seed, the same in every run. (One
            // `--enable-features` only: the engine keeps the last; later patches' features join this list.)
            // Engine patch 1: a completion step waits while another load still counts.
            // Engine patch 6: the compositor runs frames by the real clock while virtual time is paused, main
            // frames run only where the platform renders, and compositors request begin frames only while they
            // need one (an idle page costs no CPU).
            // Engine patch 7: a document's time origin is the virtual instant it commits, and a renderer that
            // takes over the page (another site's document) continues its virtual date. Engine patch 8: a
            // resource's timing entry is on the virtual clock. Engine patch 9: dedicated workers run on the
            // page's virtual clock. Engine patch 10: animated images (GIF, WebP, APNG) advance in the frames the
            // platform renders, by virtual time.
            // Engine patch 11: the observer (an inspector isolated world granted universal access) reads every
            // window, so child frames of another origin are observed, and `document.open()` keeps its window
            // listeners. Engine patch 12: a frame waits for the tasks due now on the dedicated workers joined
            // to the page's virtual time, so a worker's message at a tick is in that tick's frame.
            args.push(format!(
                "--enable-features=FrontcapTimeSeed:seed/{},FrontcapDeferredCompletion,FrontcapFramesWhileFrozen,FrontcapBeginFramesOnDemand,FrontcapVirtualNavigation,FrontcapVirtualResourceTiming,FrontcapVirtualWorkers,FrontcapVirtualImageAnimation,FrontcapObserverFrames,FrontcapFramesAfterWorkers",
                o.profile.random_seed
            ));
            // Animations run on the main thread, by the page's clock; the compositor thread's would follow
            // the real one.
            args.push("--disable-threaded-animation".to_string());
            // Each frame the platform renders is drawn at once, rastered, not paced by the display's deadline:
            // a render costs about a millisecond, not a frame interval of wall time.
            args.push("--run-all-compositor-stages-before-draw".to_string());
        }
        if o.profile.software_rendering {
            args.push("--disable-gpu".to_string());
        }
        if let Some(route) = &o.proxy {
            args.extend(route.args());
        }
        let (browser, browser_ws_url) = match (&o.cell, &o.browser_profile) {
            (Some(cell), _) => {
                let started = cell.start(&binary, &args, Duration::from_secs(30))?;
                // The cell's init stands for the browser: ending it ends every task of the cell.
                let identity = ProcessIdentity::of(cell.init)?.ok_or_else(|| ChromiumError::Shape("the cell ended at launch".into()))?;
                if let Some(record) = o.on_browser.take() {
                    record(&identity);
                }
                (BrowserHandle::Detached(identity), format!("ws://127.0.0.1:{}{}", started.port, started.path))
            }
            (None, Some(dir)) => {
                let d = browser_pin::launch_detached(&binary, &args, dir, Duration::from_secs(30))?;
                let identity = ProcessIdentity::of(d.pid)?.ok_or_else(|| ChromiumError::Shape("the browser exited at launch".into()))?;
                if let Some(record) = o.on_browser.take() {
                    record(&identity);
                }
                (BrowserHandle::Detached(identity), d.browser_ws_url)
            }
            (None, None) => {
                let l = browser_pin::launch(&binary, &args, Duration::from_secs(30))?;
                let url = l.browser_ws_url.clone();
                (BrowserHandle::Owned(l), url)
            }
        };
        let mut guard = KillUnlessDisarmed(match &browser {
            BrowserHandle::Detached(identity) => Some(identity.clone()),
            BrowserHandle::Owned(_) => None,
        });
        let client = super::connect_browser(&browser_ws_url, o.cell.as_ref()).await?;
        let version = client.call(None, &browser::GetVersionParams::default()).await?;
        // The page is the tab the browser opened at start (`about:blank`): a tab nothing drives would never
        // present with main frames held until the platform renders (engine patch 6), and the browser would
        // wait on it.
        let targets = client.call(None, &target::GetTargetsParams::default()).await?.target_infos;
        let target_id = match targets.into_iter().find(|t| t.r#type == "page") {
            Some(t) => t.target_id.0,
            None => client.call(None, &target::CreateTargetParams::new("about:blank".into())).await?.target_id.0,
        };
        let bundle = Bundle::create(&o.runs_root, &o.run)?;
        let dialog_bindings = std::sync::Arc::new(std::sync::Mutex::new(native_dialogs::DialogBindings::open(&bundle.root().join("dialog-bindings.jsonl"))?));
        let dialog_rules: std::sync::Arc<std::sync::Mutex<frontcap_core::dialog::DialogRules>> = Default::default();
        let (page, subs, native_dialogs, native_dialog_driver) = open_session(client, &target_id, patched, dialog_rules.clone(), dialog_bindings.clone(), o.profile.initial_date_ms, None).await?;
        let launch_id = o.launch_id.clone().unwrap_or_else(LaunchId::generate);
        frontcap_core::state::save_atomic(&bundle.root().join("execution.json"), &frontcap_core::bundle::ExecutionMetadata {
            run: o.run.clone(), launch: launch_id.clone(), engine: version.product.clone(),
            engine_revision: engine_record.revision.clone(), profile: o.profile.clone(),
        })?;
        let dialogs_script = prepare_target(&page, &o.profile, &launch_id, patched, &Default::default()).await?;
        let closed_windows = record_new_windows(&page);
        let tree = page.call(&page::GetFrameTreeParams::default()).await?;
        let main_frame = tree.frame_tree.frame.id;

        // The engine this run uses, for a relaunched recorder: what is built may change meanwhile.
        frontcap_core::state::save_atomic(&bundle.root().join("engine.json"), &engine_record)?;
        let engine_revision = engine_record.revision;
        let log = BehaviorLog::open(&bundle.trace())?;
        let origins = match o.passthrough_origins {
            Some(v) => v,
            None => origin_of(&o.url).into_iter().collect(),
        };
        // A replayed cassette is kept in the bundle and the installed rule names that copy.
        let mut caller = o.rules;
        let cassettes = keep_cassettes(&mut caller, bundle.root())?;
        let caller_rules = caller.clone();
        let rules = RuleSet::effective(caller, &origins)?;
        std::fs::write(bundle.rules_dir().join("0000.json"), frontcap_core::canonical::canonical_of(&rules))?;
        let installed = RulesInstalledRecord { request_id: None, revision: 0, sha256: rules.sha256(), count: rules.rules.len() as u32 };

        let derived_operations = std::collections::BTreeSet::new();
        let (now_for_frames, frame_interval_for_frames) = (VirtualTime::ZERO, o.profile.frame_interval_us);
        let mut g = Generation {
            held_pointer: None,
            held_keys: Vec::new(),
            keys_context: None,
            composing: false,
            compose_context: None,
            browser,
            browser_ws_url,
            cell: o.cell.clone(),
            target_id,
            page,
            profile: o.profile,
            subs,
            deferred_binding: None,
            world: None, world_unique: None,
            main_context: None, main_context_unique: None,
            stream_contexts: Default::default(),
            stream_observers: Default::default(),
            main_frame,
            now: VirtualTime::ZERO,
            acquisition: frontcap_core::log::AcquisitionLog::open(&bundle.evidence())?,
            bindings: crate::bindings::BindingJournal::open(&bundle.root().join(INTERCEPTIONS))?,
            dialog_bindings,
            receipts_awaiting: Vec::new(),
            receipts_ready: Vec::new(),
            adopting: false,
            trace_tail: frontcap_core::log::BehaviorTail::shared(&bundle.trace()),
            log,
            pending: Vec::new(),
            acting: None,
            bundle,
            run: o.run,
            launch_id,
            engine: version.product,
            engine_revision,
            patched,
            shared_time: None,
            recorded: 0,
            frames_idle: None,
            pending_render: None,
            pending_frame: None,
            frame_stop: None,
            frame_watch: None,
            #[cfg(feature = "test-hooks")]
            frame_requests: 0,
            #[cfg(feature = "test-hooks")]
            after_round_selection: None,
            #[cfg(feature = "test-hooks")]
            interrupt_synchronous_binding: None,
            state_revision: 0,
            presented: None,
            surface_copies: 0,
            recording_missed: Vec::new(),

            last_frame: frame_slot(now_for_frames, VirtualDuration(frame_interval_for_frames)),
            net: Network { rules, cassettes, ..Default::default() },
            caller_rules,
            watches: WatchRegistry::default(),
            watch_scripts: BTreeMap::new(),
            stop_script: None,
            stop_context: None,
            last_samples: BTreeMap::new(),
            quiet: None,
            progressing: false,
            cancel_requested: Default::default(),
            progress: Default::default(),
            cancel_at: None,
            cancellable: false,
            cancelled_at: None,
            blocking: false,
            deferred_requests: Vec::new(),
            request_round: None,
            deferred_network_completions: Vec::new(),
            collecting_requests: false,
            arrived: false,
            stop_hit: None,
            documents: 0,
            document_request: None,
            navigates: Vec::new(),
            taints: Vec::new(),
            watchdog: o.watchdog,
            dead: false,
            seen_stream_receipts: Default::default(),
            stream_receipts_ready: Default::default(),
            max_rseq: 0,
            written_rseq: 0,
            report_epoch: 0,
            resume_cursor: None,
            own_navigation: false,
            navigation_request: None,
            committed_loader: None,
            within_document: 0,
            starving: false,
            frame: None,
            frames_seen: 0,
            changes: 0,
            timeline: crate::frames::Timeline::default(),
            recording: None,
            missing_inputs: None,
            drag_answers: Vec::new(),
            queued: Vec::new(),
            proxy: o.proxy.clone(),
            upstream: o.upstream.clone(),
            pass: BTreeMap::new(),
            last_slot: 0,
            dom_base: 0,
            dom_doc: 0,
            recorded_operations: derived_operations.clone(),
            interception_stops: Default::default(),
            released_interceptions: Default::default(),
            event_stop: None,
            native_dialogs,
            native_dialog_driver,
            recorded_native_dialogs: Default::default(),
            mouse_at: None,
            window_attaches: None,
            admitting: None,
            popup_navigations: Default::default(),
            exception_contexts: Default::default(),
            recorded_exceptions: Default::default(),
            seen_exception_receipts: Default::default(),
            exception_receipts_ready: Default::default(),
            native_dialog_receipts_ready: Default::default(),
            dialog_rules,
            dialogs_script,
            closed_windows,
            last_navigate: None,
            obs: ObservationState::default(),
            ref_of: BTreeMap::new(),
            contexts: Default::default(),
            parked: BTreeMap::new(),
            entered: None,
            crashed: Default::default(),
        };
        g.watch_crashes();
        g.record(VirtualTime::ZERO, EventKind::RulesInstalled(installed));
        if !patched {
            let detail = format!("the patched engine is not built; the baseline lacks {}; windows the page opens are refused", lacks.join(", "));
            g.record(VirtualTime::ZERO, EventKind::Taint(TaintRecord { source: TaintSource::EngineBaseline, detail }));
        }
        if g.profile.popups == frontcap_core::profile::PopupPolicy::Controlled {
            g.hold_new_pages().await?;
        }
        g.initialize_cookies().await?;
        g.navigate_to_start(o.url).await?;
        if let Some(name) = &g.profile.initial_window_name {
            // This fresh document has admitted no application script or custom
            // property descriptor. Serialize the value as a JS string literal.
            let literal = serde_json::to_string(name).expect("a window name is JSON serializable");
            g.page.eval(&format!("window.name={literal}"), None).await?;
        }
        g.initialize_contexts().await?;
        let (cache_scopes, database_scopes, bucket_scopes) = g.resolve_initial_storage_scopes().await?;
        g.initialize_bucket_policies(&bucket_scopes).await?;
        g.initialize_storage().await?;
        g.initialize_dom_storage().await?;
        g.initialize_caches(&cache_scopes).await?;
        g.initialize_databases(&database_scopes).await?;
        g.verify_initial_bucket_policies(&bucket_scopes).await?;
        if g.patched && activate_shared { g.enable_shared_time().await?; }
        guard.0 = None;
        Ok(g)
    }

    /// Navigates while frozen, to the start boundary.
    pub(super) async fn navigate_to_start(&mut self, url: String) -> Result<(), ChromiumError> {
        self.navigate_frozen(Navigation::To(url)).await
    }

    /// Terminates the browser without finalizing anything: the generation could not be set up.
    pub fn kill_browser(self) {
        if let BrowserHandle::Detached(identity) = &self.browser {
            let _ = identity.kill();
        }
    }

    /// Drops the engine connection without closing the browser or finalizing the bundle, as a recorder
    /// that dies does. The page stays paused (S11). Needs a detached browser.
    pub fn abandon(self) {
        assert!(matches!(self.browser, BrowserHandle::Detached(_)), "abandon needs a detached browser");
        self.page.client.close();
        drop(self);
    }

    /// Installs a fresh native grant and abandons without draining its expiry.
    /// A retained hold cannot stand in for this grant-then-abandon fault boundary.
    #[cfg(feature = "test-hooks")]
    pub async fn advance_and_abandon(mut self, by: VirtualDuration) -> Result<Endpoint, ChromiumError> {
        if self.policy(emulation::VirtualTimePolicy::Advance, Some(by)).await? != super::progress::PolicyOutcome::Granted {
            // The caller owns this consumed generation. Close the controller even
            // when synchronous service completed the interval before a new grant;
            // do not report that the requested fault-injection setup succeeded.
            self.abandon();
            return Err(ChromiumError::Shape("abandonment requires a newly installed virtual-time grant".into()));
        }
        let endpoint = self.endpoint();
        self.abandon();
        Ok(endpoint)
    }

    /// Sends the agent's answer to a held request and abandons the connection before anything records it:
    /// the engine accepted an answer the trace does not show.
    #[cfg(feature = "test-hooks")]
    pub async fn dispatch_and_abandon(mut self, ordinal: &str, spec: MockSpec) -> Result<(), ChromiumError> {
        self.reengage_held(ordinal).await?;
        let h = self.net.held.get(ordinal).ok_or_else(|| ChromiumError::NoSuchHold(ordinal.into()))?;
        let (fetch_id, rule) = (h.fetch_id.clone(), h.rule.clone());
        self.fulfill(ordinal, fetch_id, &rule, &spec, ResponseSource::Agent, None).await?;
        self.abandon();
        Ok(())
    }

    /// Reconcile native dialog ownership before reconnect initializes renderer domains. The caller
    /// keeps this connection alive while it journals pending dependent answers or lease-loss cleanup.
    pub async fn prepare_dialog_recovery(o: &ReconnectOptions) -> Result<Option<DialogRecovery>, ChromiumError> {
        let bundle = Bundle::open(&o.runs_root, &o.run)?;
        let record = std::fs::read(bundle.root().join("engine.json")).ok().and_then(|b| serde_json::from_slice::<EngineRecord>(&b).ok());
        if !record.is_some_and(|r| r.patched) { return Ok(None) }
        let client = super::connect_browser(&o.endpoint.browser_ws_url, o.endpoint.cell.as_ref()).await?;
        let attached = client.call(None, &target::AttachToTargetParams {
            flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(o.endpoint.target_id.clone()))
        }).await?;
        let popups = contexts::open_popups(&client, &BehaviorLog::read(&bundle.trace())?).await?;
        let page = Session::new(client, attached.session_id.0);
        let rules = std::sync::Arc::new(std::sync::Mutex::new(o.dialog_rules.clone()));
        let bindings = std::sync::Arc::new(std::sync::Mutex::new(native_dialogs::DialogBindings::open(&bundle.root().join("dialog-bindings.jsonl"))?));
        native_dialogs::DialogRecovery::attach(page, rules, bindings, o, popups).await.map(Some)
    }

    /// Connects to a live generation whose previous connection died, re-establishing what the page does
    /// not keep and re-deriving counters from the trace. For held dialogs prepare recovery first and
    /// transfer its independently serviceable handle with `reconnect_with_dialog_recovery`.
    pub async fn reconnect(o: ReconnectOptions) -> Result<(Generation, Reconnected), ChromiumError> {
        Self::reconnect_inner(o, None, None, None).await
    }

    /// Connects to a generation's browser the controller let go of on purpose, as `reconnect` does, and
    /// records why (`imaged`: after a process image of it; `restored`: a child restored from one) where a
    /// lost recorder records a fault. Time run past the last grant is still a fault.
    pub async fn adopt(o: ReconnectOptions, why: ControllerChange) -> Result<(Generation, Reconnected), ChromiumError> {
        Self::reconnect_inner(o, None, None, Some(why)).await
    }

    /// Transfers the prepared connection, driver and binding writer into the generation. A cloned
    /// DialogSession can keep servicing dependent requests while renderer initialization is parked.
    pub async fn reconnect_with_dialog_recovery(o: ReconnectOptions, recovery: DialogRecovery) -> Result<(Generation, Reconnected), ChromiumError> {
        if !recovery.matches(&o) {
            return Err(ChromiumError::InvalidArgument("dialog recovery belongs to another generation".into()));
        }
        Self::reconnect_inner(o, Some(recovery), None, None).await
    }

    /// Restore a dispatched interception arm before deciding native pauses that were not yet bound.
    pub async fn reconnect_with_interception_arm(o: ReconnectOptions, recovery: Option<DialogRecovery>, arm: EventArm) -> Result<(Generation, Reconnected), ChromiumError> {
        if recovery.as_ref().is_some_and(|r| !r.matches(&o)) {
            return Err(ChromiumError::InvalidArgument("dialog recovery belongs to another generation".into()));
        }
        Self::reconnect_inner(o, recovery, Some(arm), None).await
    }

    async fn reconnect_inner(o: ReconnectOptions, mut recovery: Option<DialogRecovery>, interception_arm: Option<EventArm>, planned: Option<ControllerChange>) -> Result<(Generation, Reconnected), ChromiumError> {
        let blocking_answer_time = recovery.as_ref().map(|r| r.blocking_answer_time.clone()).unwrap_or_default();
        let cancelled_navigation = recovery.as_ref().and_then(|r| r.cancelled_navigation.clone());
        // Ending the generation ends this process: in a cell, the cell's own init and no other.
        if o.endpoint.cell.as_ref().is_some_and(|cell| cell.init != o.endpoint.browser_pid) {
            return Err(ChromiumError::InvalidArgument("a cell's browser process is the cell's init".into()));
        }
        let identity = ProcessIdentity::of(o.endpoint.browser_pid)?
            .ok_or_else(|| ChromiumError::Unsupported("the browser process is gone".into()))?;
        let client = super::connect_browser(&o.endpoint.browser_ws_url, o.endpoint.cell.as_ref()).await?;
        let version = client.call(None, &browser::GetVersionParams::default()).await?;
        let bundle = Bundle::open(&o.runs_root, &o.run)?;
        let dialog_bindings = match &recovery {
            Some(recovery) => recovery.bindings(),
            None => std::sync::Arc::new(std::sync::Mutex::new(native_dialogs::DialogBindings::open(&bundle.root().join("dialog-bindings.jsonl"))?)),
        };
        let engine_record = std::fs::read(bundle.root().join("engine.json")).ok().and_then(|b| serde_json::from_slice::<EngineRecord>(&b).ok());
        // A bundle without the record was launched before patched engines existed: on the baseline.
        let (engine_revision, patched) = match engine_record {
            Some(r) => (r.revision, r.patched),
            None => (browser_pin::load_pin(&o.repo_root)?.revision.to_string(), false),
        };
        let started = BehaviorLog::read(&bundle.trace())?.last().is_some_and(|e| e.t_us > VirtualTime::ZERO);
        let mut target_id = o.endpoint.target_id.clone();
        let dialog_rules = recovery.as_ref().map(|r| r.rules())
            .unwrap_or_else(|| std::sync::Arc::new(std::sync::Mutex::new(o.dialog_rules.clone())));
        let (mut page, mut subs, mut native_dialogs, mut native_dialog_driver) = open_session(client.clone(), &target_id, patched, dialog_rules.clone(), dialog_bindings.clone(), o.profile.initial_date_ms, recovery.take()).await?;
        // The page stays paused while no session is connected (S11); the new session takes the policy over.
        page.call(&emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)).await?;
        // At the start boundary no script has run: a document still loading there starts again in a fresh
        // target (leaving it for another document would add a completion step to the new one).
        let mut restart = None;
        let dialogs_script;
        // A document loading after the start boundary continues: the engine keeps the loader a lost
        // controller froze at its commit for the next one (engine patch 51, ordered admission), and its
        // request is adopted.
        // An enrolled document retains its native loader and clock across loss.
        // Replacing it would abandon membership while the partition remains held.
        let shared_document = if patched {
            match page.call(&page::FrontcapGetSharedClockParams::default()).await {
                Ok(clock) => clock.activated,
                Err(ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. }))
                    if message == "shared clock is not prepared" => false,
                Err(error) => return Err(error),
            }
        } else { false };
        if !started && !shared_document && page.eval("document.readyState", None).await? == "loading" {
            let url = page.call(&page::GetFrameTreeParams::default()).await?.frame_tree.frame.url;
            let created = client.call(None, &target::CreateTargetParams::new("about:blank".into())).await?;
            // An independent recovery handle may still be polling this session while we replace the
            // pristine target. Retire it before closing, not after the new session has initialized.
            if let Some(driver) = &native_dialog_driver { driver.retire(); }
            client.call(None, &target::CloseTargetParams::new(target::TargetID(target_id.clone()))).await?;
            target_id = created.target_id.0;
            (page, subs, native_dialogs, native_dialog_driver) = open_session(client, &target_id, patched, dialog_rules.clone(), dialog_bindings.clone(), o.profile.initial_date_ms, None).await?;
            dialogs_script = prepare_target(&page, &o.profile, &o.launch_id, patched, &o.dialog_rules).await?;
            restart = Some(url);
        } else {
            dialogs_script = instrument::install(&page, &o.profile, &o.launch_id, patched, &o.dialog_rules).await?;
            if patched {
                // Device metrics, locale and timezone belong to the living engine.
                // Reapplying launch values here would undo explicit interventions.
                page.call(&cnet::SetBypassServiceWorkerParams { bypass: true }).await?;
                page.call(&cnet::SetCacheDisabledParams { cache_disabled: true }).await?;
            } else {
                apply_overrides(&page, &o.profile).await?;
            }
        }
        // The windows the generation's pages opened that are still open, enrolled again before the
        // retained requests are taken: theirs are replayed too (`contexts`).
        let book: contexts::Contexts = Default::default();
        if restart.is_none() && patched && o.profile.popups == frontcap_core::profile::PopupPolicy::Controlled {
            let e = contexts::Enrollment {
                profile: o.profile.clone(),
                launch: o.launch_id.clone(),
                patched,
                dialog_rules: dialog_rules.clone(),
                paused: subs.paused.feed(),
                network: NetworkFeeds { worker_changes: subs.workers.lock().unwrap().changed.clone(), attached: subs.attached.feed(), sent: subs.sent.feed(), response: subs.response.feed(), finished: subs.finished.feed(), failed: subs.failed.feed() },
                console: subs.console.feed(),
                log: subs.log.feed(),
                exceptions: subs.exceptions.feed(),
                dialogs: native_dialog_driver.as_ref().map(|driver| driver.dialogs.clone()),
                clock: page.service_clock(),
            };
            contexts::readopt(&page, &book, &BehaviorLog::read(&bundle.trace())?, &target_id, &e).await?;
        }
        let targets = contexts::targets_of(&book, &target_id);
        // The engine replayed the requests it kept paused when interception was enabled. Those the dead
        // recorder bound get their request records first, so counters derived from the trace include them.
        let (bindings, inventory, restored) = if restart.is_none() {
            let entries = adoption::read_bindings(&bundle)?;
            let inventory = adoption::inventory(&mut subs)?;
            let restored = adoption::missing_request_records(&BehaviorLog::read(&bundle.trace())?, &entries, &targets)?;
            (entries, inventory, restored)
        } else {
            (Vec::new(), Vec::new(), Vec::new())
        };
        let closed_windows = record_new_windows(&page);
        let tree = page.call(&page::GetFrameTreeParams::default()).await?;
        let mut events = BehaviorLog::read(&bundle.trace())?;
        let with_restored: Vec<BehaviorEvent> = events
            .iter()
            .cloned()
            .chain(restored.iter().map(|(t, r)| BehaviorEvent { seq: u64::MAX, t_us: *t, kind: EventKind::Request(r.clone()) }))
            .collect();
        let history = crate::rule_history::load(&bundle.rules_dir(), &with_restored)?;
        let installed = history.active;
        let mut installations = history.previous.clone();
        installations.insert(history.revision, installed.clone());
        let derived = derive(&with_restored, &installed, &frontcap_core::rules::installed_since(&installations));
        let cassettes = reload_cassettes(&installed, bundle.root(), &derived.served, &derived.sessions)?;
        let recorded_sessions = recorded_sessions(&installed, bundle.root(), history.revision);
        let log = BehaviorLog::open(&bundle.trace())?;
        let now = log.last_time();
        let derived_operations: std::collections::BTreeSet<RequestId> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Action(a) => Some(a.request_id.clone()),
                EventKind::Intervention(i) => Some(i.request_id.clone()),
                _ => None,
            })
            .collect();
        let surface_copies = if patched { presentation::last_surface_copy(&bundle)? } else { 0 };
        let slot_at_relaunch = frame_slot(now, VirtualDuration(o.profile.frame_interval_us));
        let (now_for_frames, frame_interval_for_frames) = (now, o.profile.frame_interval_us);
        let held_pointer = events.iter().rev().find_map(|e| match &e.kind {
            EventKind::Action(a) if matches!(a.action.as_str(), "pointer_begin" | "pointer_move" | "pointer_end" | "pointer_cancel") => Some(a.pointer.clone()),
            _ => None,
        }).flatten();
        let (held_keys, keys_context) = events.iter().rev().find_map(|e| match &e.kind {
            EventKind::Action(a) if matches!(a.action.as_str(), "key_down" | "key_up") => a.keys.clone().map(|keys| (keys, a.context.clone())),
            _ => None,
        }).unwrap_or_default();
        let keys_context = keys_context.filter(|_| !held_keys.is_empty());
        let (composing, compose_context) = events.iter().rev().find_map(|e| match &e.kind {
            EventKind::Action(a) if a.action == "compose" => Some((a.args.as_ref().and_then(|v| v["text"].as_str()).is_some_and(|t| !t.is_empty()), a.context.clone())),
            EventKind::Action(a) if a.action == "commit" => Some((false, None)),
            _ => None,
        }).unwrap_or((false, None));
        let compose_context = compose_context.filter(|_| composing);
        let mut g = Generation {
            held_pointer,
            held_keys,
            keys_context,
            composing,
            compose_context,
            browser: BrowserHandle::Detached(identity),
            browser_ws_url: o.endpoint.browser_ws_url.clone(),
            cell: o.endpoint.cell.clone(),
            target_id,
            page,
            profile: o.profile,
            subs,
            deferred_binding: None,
            world: None, world_unique: None,
            main_context: None, main_context_unique: None,
            stream_contexts: Default::default(),
            stream_observers: Default::default(),
            main_frame: tree.frame_tree.frame.id,
            now,
            acquisition: frontcap_core::log::AcquisitionLog::open(&bundle.evidence())?,
            bindings: crate::bindings::BindingJournal::open(&bundle.root().join(INTERCEPTIONS))?,
            dialog_bindings,
            receipts_awaiting: Vec::new(),
            receipts_ready: Vec::new(),
            adopting: false,
            trace_tail: frontcap_core::log::BehaviorTail::shared(&bundle.trace()),
            log,
            pending: Vec::new(),
            acting: None,
            bundle,
            run: o.run,
            launch_id: o.launch_id,
            engine: version.product,
            engine_revision,
            patched,
            shared_time: None,
            recorded: 0,
            state_revision: events.iter().filter(|e| exploration::affects_state(&e.kind)).count() as u64,
            frames_idle: None,
            pending_render: None,
            pending_frame: None,
            frame_stop: None,
            frame_watch: None,
            #[cfg(feature = "test-hooks")]
            frame_requests: 0,
            #[cfg(feature = "test-hooks")]
            after_round_selection: None,
            #[cfg(feature = "test-hooks")]
            interrupt_synchronous_binding: None,
            presented: None,
            surface_copies,
            recording_missed: Vec::new(),

            last_frame: frame_slot(now_for_frames, VirtualDuration(frame_interval_for_frames)),
            net: Network { rules_revision: history.revision, previous_rules: history.previous, cassettes, rules: installed, cursors: derived.cursors, next_ordinal: derived.next_ordinal, recorded_sessions, ..Default::default() },
            caller_rules: RuleSet::default(),
            watches: o.watches,
            watch_scripts: BTreeMap::new(),
            stop_script: None,
            stop_context: None,
            last_samples: BTreeMap::new(),
            quiet: None,
            progressing: false,
            cancel_requested: Default::default(),
            progress: Default::default(),
            cancel_at: None,
            cancellable: false,
            cancelled_at: None,
            blocking: false,
            deferred_requests: Vec::new(),
            request_round: None,
            deferred_network_completions: Vec::new(),
            collecting_requests: false,
            arrived: false,
            stop_hit: None,
            documents: derived.documents,
            document_request: None,
            navigates: Vec::new(),
            taints: derived.taints,
            watchdog: o.watchdog,
            dead: false,
            seen_stream_receipts: events.iter().filter_map(|event| match &event.kind {
                EventKind::Stream(stream) => stream.receipt.as_ref().map(|receipt| receipt.id.clone()),
                _ => None,
            }).collect(),
            stream_receipts_ready: events.iter().filter_map(|event| match &event.kind {
                EventKind::Stream(stream) => stream.receipt.as_ref().map(|receipt| receipt.id.clone()),
                _ => None,
            }).collect(),
            max_rseq: 0,
            written_rseq: 0,
            // The observer world found next is the current document's, unless the page restarts in a
            // fresh target, whose document comes after.
            report_epoch: if restart.is_some() { derived.documents } else { derived.documents.saturating_sub(1) },
            resume_cursor: restart.is_none().then(|| (derived.max_rseq, derived.last_samples.clone())),
            own_navigation: false,
            navigation_request: None,
            committed_loader: None,
            within_document: 0,
            starving: false,
            frame: None,
            frames_seen: 0,
            changes: 0,
            timeline: crate::frames::Timeline::default(),
            recording: None,
            missing_inputs: None,
            drag_answers: Vec::new(),
            queued: Vec::new(),
            proxy: o.endpoint.proxy.clone(),
            upstream: o.upstream.clone(),
            pass: BTreeMap::new(),
            last_slot: slot_at_relaunch,
            dom_base: 0,
            dom_doc: 0,
            recorded_operations: derived_operations.clone(),
            interception_stops: Default::default(),
            released_interceptions: Default::default(),
            event_stop: None,
            native_dialogs,
            native_dialog_driver,
            exception_contexts: Default::default(),
            seen_exception_receipts: events.iter().filter_map(|e| match &e.kind {
                EventKind::Exception(e) => e.receipt.as_ref().map(|r| r.id.clone()),
                _ => None,
            }).collect(),
            exception_receipts_ready: events.iter().filter_map(|e| match &e.kind {
                EventKind::Exception(e) => e.receipt.as_ref().map(|r| r.id.clone()),
                _ => None,
            }).collect(),
            recorded_exceptions: events.iter().filter_map(|e| match &e.kind {
                EventKind::Exception(e) => e.native.clone(),
                _ => None,
            }).collect(),
            recorded_native_dialogs: events.iter().filter_map(|e| match &e.kind {
                EventKind::Dialog(d) => d.native.as_ref().map(|n| n.id.clone()),
                _ => None,
            }).collect(),
            // Restored with the recorder's observations; unknown, a click moves the mouse to its point.
            mouse_at: None,
            window_attaches: None,
            admitting: None,
            popup_navigations: Default::default(),
            native_dialog_receipts_ready: Default::default(),
            dialog_rules,
            dialogs_script,
            closed_windows,
            last_navigate: None,
            obs: ObservationState::default(),
            ref_of: BTreeMap::new(),
            contexts: book,
            parked: BTreeMap::new(),
            entered: None,
            crashed: Default::default(),
        };
        if restart.is_none() { g.recover_shared_time().await?; }
        g.watch_crashes();
        // The launch page's and its open windows' stream bindings: a window's streams are its own.
        let stream_targets = g.known_targets();
        for entry in &bindings {
            if let crate::bindings::BindingEntry::StreamConnecting(connection) = entry
                && stream_targets.contains_key(&connection.target_id) {
                let current_installation = bindings.iter().any(|entry| matches!(entry,
                    crate::bindings::BindingEntry::StreamSelected(selected)
                        if selected.target_id == connection.target_id && selected.fetch_id == connection.fetch_id
                            && selected.rules_revision == g.net.rules_revision));
                if current_installation && let Some((_, key, session)) = &connection.record {
                    let next = g.net.recorded_sessions.entry(key.clone()).or_default();
                    *next = (*next).max(session + 1);
                }
                g.net.stream_connections.insert(connection.fetch_id.clone(), connection.as_ref().clone());
            }
        }
        for entry in &bindings {
            if let crate::bindings::BindingEntry::StreamReplay(replay) = entry
                && stream_targets.contains_key(&replay.target_id) {
                if replay.rules_revision == g.net.rules_revision
                    && let Some(session) = replay.session
                    && let Some(cassette) = g.net.cassettes.get_mut(&replay.rule) {
                    cassette.sessions_served.insert((replay.key.clone(), session));
                }
                g.net.stream_replays.insert(replay.fetch_id.clone(), replay.as_ref().clone());
            }
        }
        // Reserve selections even when recorder loss preceded any trace request record.
        // Do this before adopting any unbound sibling so its ordinal cannot collide.
        for entry in &bindings {
            if let crate::bindings::BindingEntry::StreamSelected(selected) = entry
                && stream_targets.contains_key(&selected.target_id) {
                if let Some(n) = selected.ordinal.strip_prefix("req-").and_then(|n| n.parse::<u64>().ok()) {
                    g.net.next_ordinal = g.net.next_ordinal.max(n);
                }
                g.net.stream_selections.insert(selected.fetch_id.clone(), selected.as_ref().clone());
            }
        }
        if let Some((request, action, t_us)) = cancelled_navigation {
            g.record_navigation_at(t_us, &request, &action);
        }
        let mut restored: Vec<_> = restored.into_iter().map(|(t, r)| (t, EventKind::Request(r))).collect();
        restored.extend(adoption::missing_interception_records(&events, &bindings, &g.known_targets())?
            .into_iter().map(|(t, r)| (t, EventKind::Interception(r))));
        restored.sort_by_key(|(t, e)| match e {
            EventKind::Request(r) => (*t, adoption::ordinal_number(&r.id), 0),
            EventKind::Interception(r) => (*t, adoption::ordinal_number(&r.ordinal), 1),
            _ => unreachable!(),
        });
        for (t, record) in restored {
            let taint = if let EventKind::Request(request) = &record {
                bindings.iter().find_map(|entry| match entry {
                    crate::bindings::BindingEntry::Bound(b) if b.ordinal == request.id => b.selection_taint.clone(), _ => None,
                })
            } else { None };
            events.push(BehaviorEvent { seq: u64::MAX, t_us: t, kind: record.clone() });
            g.record(t, record);
            if let Some(taint) = taint { g.record(t, EventKind::Taint(taint)); }
        }
        for binding in &bindings {
            let (target_id, answered_us, response) = match binding {
                crate::bindings::BindingEntry::StreamOpening { target_id, answered_us, response, .. } => (target_id, *answered_us, response),
                crate::bindings::BindingEntry::StreamClosed(closure) => (&closure.target_id, closure.handshake.answered_us, &closure.handshake.response),
                crate::bindings::BindingEntry::StreamAbandoned { target_id, handshake } => (target_id, handshake.answered_us, &handshake.response),
                _ => continue,
            };
            if target_id == &g.target_id
                && !events.iter().any(|event| matches!(&event.kind, EventKind::Response(saved) if saved.id == response.id)) {
                let kind = EventKind::Response(response.clone());
                let t = VirtualTime(answered_us);
                events.push(BehaviorEvent { seq: u64::MAX, t_us: t, kind: kind.clone() });
                g.record(t, kind);
            }
        }
        // Future documents register every watch, a popup's in that popup; the current documents' observers
        // still have them. A popup closed meanwhile has no future documents.
        let registered: Vec<(WatchName, Predicate, Option<String>)> = g.watches.list().into_iter()
            .map(|(n, p)| (n.clone(), p.clone(), g.watches.context(n).map(str::to_string))).collect();
        for (name, predicate, context) in registered {
            let expr = format!(
                "globalThis.__frontcap.addWatch({}, {})",
                serde_json::to_string(name.as_str()).unwrap(),
                serde_json::to_string(&predicate).unwrap()
            );
            let entered = match g.enter_for(context.as_deref()) {
                Ok(entered) => entered,
                Err(ChromiumError::ContextClosed(_)) => continue,
                Err(e) => return Err(e),
            };
            let id = g.world_script(&expr).await;
            if entered { g.leave_context(); }
            g.watch_scripts.insert(name, id?);
        }
        // The buffered reports first: live ones (the binding is installed again above) may already be
        // queued and would move the cursor past them.
        g.take_contexts()?;
        // The trace may stop before a published opening intent. Query the held engine time before
        // deciding whether that opening is due; this read does not advance the page.
        g.now = g.read_time().await?.max(g.now);
        g.recover_stream_handshakes(&bindings)?;
        let restored_openings = g.recover_stream_openings(&bindings, &events).await?;
        for (t, opening) in restored_openings {
            g.record(t, EventKind::Stream(opening.clone()));
            events.push(BehaviorEvent { seq: u64::MAX, t_us: t, kind: EventKind::Stream(opening) });
        }
        for binding in &bindings {
            if let crate::bindings::BindingEntry::StreamClosed(closure) = binding
                && closure.target_id == g.target_id {
                g.restore_early_stream_close(closure, &events);
            }
        }
        g.recover_stream_reports(&events).await?;
        let mut replayed = 0;
        if let Some(world) = g.world.clone() {
            if let Some(total) = g.page.eval("globalThis.__frontcap.mutationTotal()", Some(world.clone())).await?.as_u64() {
                g.dom_doc = total;
            }
            let text = g.page.eval(&format!("globalThis.__frontcap.reportsAfter({})", g.max_rseq), Some(world)).await?;
            let missed: Vec<serde_json::Value> =
                serde_json::from_str(text.as_str().unwrap_or("[]")).map_err(|e| ChromiumError::Shape(e.to_string()))?;
            for m in &missed {
                g.take_report(m)?;
                replayed += 1;
            }
        }
        let mut granted = o.granted_until.unwrap_or(now);
        if let Some(world) = g.world.clone()
            && let Some(us) = g.page.eval("globalThis.__frontcap.grantedUs ?? null", Some(world)).await?.as_str()
        {
            granted = VirtualTime(us.parse().map_err(|_| ChromiumError::Shape("invalid retained virtual time grant".into()))?);
        }
        // Only the observer present now is the trace's current document; a document that starts later
        // (or one already running without instrumentation) begins with its own numbering.
        g.resume_cursor = None;
        g.drain().await?;
        g.now = g.read_time().await?.max(g.now);
        // Retained requests continue as decided, after the reports the dead recorder never read (one
        // commit writes both in time order) and before the new connection grants any time.
        g.event_stop = interception_arm.map(|arm| EventStop {
            event: ProgressEvent::Interception, after_seq: arm.after_seq, triggers: Vec::new(),
        });
        let adopted = g.adopt_interceptions(&bindings, inventory, &events, &blocking_answer_time).await?;
        g.event_stop = None;
        let unanswered: Vec<String> = derived.unanswered.iter().filter(|o| !adopted.contains(o)).cloned().collect();
        let exceeded = if patched { g.now > granted } else { g.now.0 / 1000 > granted.0 / 1000 };
        let ran_past = exceeded.then_some((granted, g.now));
        if let (Some(change), None) = (planned, ran_past) {
            g.record(g.now, EventKind::Controller(ControllerRecord { change }));
        } else {
            g.record(g.now, EventKind::Taint(TaintRecord {
                source: TaintSource::RecorderFault,
                detail: match ran_past {
                    Some((granted, reached)) => format!(
                        "recorder relaunched; virtual time ran from {} us (granted) to {} us while no recorder owned it; requests without a response: {:?}",
                        granted.0,
                        reached.0,
                        unanswered
                    ),
                    None => format!("recorder relaunched; requests without a response: {:?}", unanswered),
                },
            }));
        }
        g.commit()?;
        if restart.is_none() {
            g.take_contexts()?;
            // Include restored close calls so reconstruction cannot synthesize another close or
            // restore a stream the page had already ended before the old recorder disappeared.
            let reconciled = BehaviorLog::read(&g.trace_path())?;
            g.rederive_streams(&reconciled).await?;
            // Each open window's streams in that window, by the launch page's steps: handshakes and
            // openings in flight, early closes, retained reports, then what the trace says is open.
            for popup in g.open_popups() {
                g.enter_context(&popup)?;
                let taken: Result<(), ChromiumError> = async {
                    g.take_contexts()?;
                    g.recover_stream_handshakes(&bindings)?;
                    let mut events = reconciled.clone();
                    for (t, opening) in g.recover_stream_openings(&bindings, &events).await? {
                        g.record(t, EventKind::Stream(opening.clone()));
                        events.push(BehaviorEvent { seq: u64::MAX, t_us: t, kind: EventKind::Stream(opening) });
                    }
                    for binding in &bindings {
                        if let crate::bindings::BindingEntry::StreamClosed(closure) = binding
                            && closure.target_id == g.target_id {
                            g.restore_early_stream_close(closure, &events);
                        }
                    }
                    g.recover_stream_reports(&events).await?;
                    g.commit()?;
                    let reconciled = BehaviorLog::read(&g.trace_path())?;
                    g.rederive_streams(&reconciled).await
                }.await;
                g.leave_context();
                taken?;
            }
        }
        if restart.is_none() { g.recover_stopped_render().await?; }
        if let Some(url) = restart {
            g.navigate_to_start(url).await?;
        }
        // Frames from here show the state the reconnect found.
        g.timeline.changed(crate::frames::wall_now(), g.now, g.documents, g.changes);
        if g.patched && g.profile.popups == frontcap_core::profile::PopupPolicy::Controlled {
            g.hold_new_pages().await?;
        }
        Ok((g, Reconnected { affected: unanswered, replayed_samples: replayed, ran_past }))
    }

    /// Ends the generation: records the final samples, finalizes the bundle, and ends the browser. The
    /// browser is ended even when an earlier step fails; the first failure is returned and the bundle then
    /// stays unfinalized.
    pub async fn stop(mut self) -> Result<Manifest, ChromiumError> {
        let finalized = self.finalize_evidence().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), self.page.client.call(None, &browser::CloseParams::default())).await;
        if let BrowserHandle::Detached(identity) = &self.browser {
            for _ in 0..100 {
                if !identity.is_alive() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            identity.kill()?;
        }
        finalized
    }

    /// Notes the renderer's crash (`Inspector.targetCrashed` on any of its sessions, the launch page's and
    /// its popups' alike, since they share the one renderer): nothing it is asked is answered after it.
    fn watch_crashes(&mut self) {
        let mut crashes = self.page.client.subscribe::<cdp::protocol::inspector::TargetCrashedEvent>(None);
        let crashed = self.crashed.clone();
        tokio::spawn(async move {
            if crashes.next().await.is_ok() {
                crashed.store(true, std::sync::atomic::Ordering::SeqCst);
            }
        });
    }

    /// A finalized copy, under `dest/<run>`, of the evidence recorded so far: the prefix a replay
    /// basis repeats. Only files are copied; the target, its clock and this bundle are unchanged. The
    /// copy names `missing` as the inputs it lacks; the caller holds the boundary frozen.
    pub fn copy_prefix(&mut self, dest: &std::path::Path, missing: Vec<String>) -> Result<Manifest, ChromiumError> {
        self.alive()?;
        self.commit()?;
        self.log.sync()?;
        let copy = Bundle::create(dest, &self.run)?;
        let mut pending = vec![self.bundle.root().to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                let rel = path.strip_prefix(self.bundle.root()).expect("under the bundle");
                // Derived artifacts and screenshots are not evidence a replay reads.
                if rel.starts_with("derived") || rel.starts_with("frames") || rel.starts_with("screenshots") {
                    continue;
                }
                let target = copy.root().join(rel);
                if entry.file_type()?.is_dir() {
                    std::fs::create_dir_all(&target)?;
                    pending.push(path);
                } else {
                    std::fs::copy(&path, &target)?;
                }
            }
        }
        let manifest = Manifest {
            run: self.run.clone(),
            launch: self.launch_id.clone(),
            engine: self.engine.clone(),
            engine_revision: self.engine_revision.clone(),
            host_class: self.profile.host_class.clone(),
            profile: self.profile.clone(),
            determinism: Determinism { level: Level::Time, taints: self.taints.clone() },
            replayable: missing.is_empty(),
            missing_inputs: missing,
            finalization: Finalization::Open,
            recovery_snapshot: None,
            members: Default::default(),
        };
        Ok(copy.finalize(manifest)?)
    }

    pub(super) async fn finalize_evidence(&mut self) -> Result<Manifest, ChromiumError> {
        let crashed = self.crashed.load(std::sync::atomic::Ordering::SeqCst);
        if crashed {
            // Nothing the page did after its last drain can be read: the evidence ends there.
            let detail = "the page's renderer crashed; nothing after the last recorded event was observed".to_string();
            self.record(self.now.max(self.log.last_time()), EventKind::Taint(TaintRecord { source: TaintSource::RendererCrashed, detail }));
        }
        if !self.dead && !crashed {
            let names: Vec<WatchName> = self.watches.list().into_iter().map(|(n, _)| n.clone()).collect();
            for name in names {
                self.watch_remove(&name).await?;
            }
            self.drain().await?;
            self.settle_issued().await?;
        }
        // Nothing comes after finalization: a request record that never arrived, or a passed-through
        // response still outstanding, no longer holds events back.
        self.give_up_interceptions();
        for p in self.net.passthrough.values_mut() {
            p.moved = true;
        }
        // Messages about a response still outstanding are kept at their own instants.
        for (t, message) in std::mem::take(&mut self.net.deferred_console).into_values().flatten() {
            self.record(t, EventKind::Console(message));
        }
        self.commit()?;
        self.log.sync()?;
        let manifest = Manifest {
            run: self.run.clone(),
            launch: self.launch_id.clone(),
            engine: self.engine.clone(),
            engine_revision: self.engine_revision.clone(),
            host_class: self.profile.host_class.clone(),
            profile: self.profile.clone(),
            determinism: Determinism { level: Level::Time, taints: self.taints.clone() },
            replayable: self.missing_inputs.as_ref().is_some_and(Vec::is_empty),
            missing_inputs: self.missing_inputs.clone().unwrap_or_else(|| vec!["inputs/commands.jsonl".into()]),
            finalization: Finalization::Open,
            recovery_snapshot: None,
            members: Default::default(),
        };
        Ok(self.bundle.finalize(manifest)?)
    }
}

/// Keeps each `replay` rule's cassette in the bundle (`cassettes/<sha256>.jsonl`), makes the rule name
/// that copy, and loads it.
pub(super) fn keep_cassettes(rules: &mut RuleSet, bundle: &std::path::Path) -> Result<BTreeMap<RuleId, crate::network::ReplayCassette>, ChromiumError> {
    let mut out = BTreeMap::new();
    for r in rules.rules.iter_mut() {
        let Action::Replay { cassette, .. } = &mut r.action else { continue };
        let text = std::fs::read_to_string(&*cassette).map_err(|e| ChromiumError::InvalidArgument(format!("cassette {cassette} of rule {}: {e}", r.id)))?;
        let (lines, streams) = frontcap_core::cassette::parse_all(&text).map_err(|e| ChromiumError::InvalidArgument(format!("cassette {cassette}: {e}")))?;
        let kept = format!("cassettes/{}.jsonl", frontcap_core::canonical::sha256_hex(&text));
        std::fs::create_dir_all(bundle.join("cassettes"))?;
        frontcap_core::state::save_atomic_bytes(&bundle.join(&kept), text.as_bytes())?;
        *cassette = kept;
        out.insert(r.id.clone(), crate::network::ReplayCassette { lines, served: Default::default(), streams, sessions_served: Default::default() });
    }
    Ok(out)
}

/// The cassettes of installed `replay` rules, from the bundle, with the lines the trace shows answered.
#[allow(clippy::type_complexity)]
pub(super) fn reload_cassettes(rules: &RuleSet, bundle: &std::path::Path, served: &BTreeMap<RuleId, std::collections::BTreeSet<usize>>, sessions: &BTreeMap<RuleId, std::collections::BTreeSet<(frontcap_core::cassette::Key, u64)>>) -> Result<BTreeMap<RuleId, crate::network::ReplayCassette>, ChromiumError> {
    let mut out = BTreeMap::new();
    for r in &rules.rules {
        let Action::Replay { cassette, .. } = &r.action else { continue };
        let text = std::fs::read_to_string(bundle.join(cassette))?;
        let (lines, streams) = frontcap_core::cassette::parse_all(&text).map_err(|e| ChromiumError::Shape(format!("cassette {cassette}: {e}")))?;
        let sessions_served = sessions.get(&r.id).cloned().unwrap_or_default();
        out.insert(r.id.clone(), crate::network::ReplayCassette { lines, served: served.get(&r.id).cloned().unwrap_or_default(), streams, sessions_served });
    }
    Ok(out)
}

/// The next session of each stream key the installed `record` rules have kept, from their cassettes in
/// the bundle (written and synced as each handshake was decided): a relaunch numbers on after them.
pub(super) fn recorded_sessions(rules: &RuleSet, bundle: &std::path::Path, revision: u64) -> BTreeMap<frontcap_core::cassette::Key, u64> {
    let mut out: BTreeMap<frontcap_core::cassette::Key, u64> = BTreeMap::new();
    let names: std::collections::BTreeSet<&str> = rules.rules.iter().filter_map(|r| match &r.action {
        Action::Record { cassette } => Some(cassette.as_str()),
        _ => None,
    }).collect();
    for name in names {
        let name = frontcap_core::cassette::recording_name(revision, name);
        let Ok(text) = std::fs::read_to_string(bundle.join("cassettes").join(format!("{name}.jsonl"))) else { continue };
        // A torn last line is ignored, as a replay ignores it.
        let Ok((_, streams)) = frontcap_core::cassette::parse_all(&text) else { continue };
        for e in streams {
            let next = out.entry(e.key).or_default();
            *next = (*next).max(e.session + 1);
        }
    }
    out
}

/// Reconstruct allocation state from published requests plus durable, unpublished bindings.
pub(super) fn recovery_network(bundle: &Bundle, entries: &[crate::bindings::BindingEntry]) -> Result<Network, ChromiumError> {
    let mut events = BehaviorLog::read(&bundle.trace())?;
    let targets = entries.iter().filter_map(|e| match e {
        crate::bindings::BindingEntry::Bound(b) => Some((b.target_id.clone(), None)), _ => None,
    }).collect();
    events.extend(adoption::missing_request_records(&events, entries, &targets)?.into_iter()
        .map(|(t_us, r)| BehaviorEvent { seq: u64::MAX, t_us, kind: EventKind::Request(r) }));
    let history = crate::rule_history::load(&bundle.rules_dir(), &events)?;
    let mut installations = history.previous.clone();
    installations.insert(history.revision, history.active.clone());
    let derived = derive(&events, &history.active, &frontcap_core::rules::installed_since(&installations));
    let cassettes = reload_cassettes(&history.active, bundle.root(), &derived.served, &derived.sessions)?;
    Ok(Network { rules_revision: history.revision, previous_rules: history.previous, rules: history.active,
        cursors: derived.cursors, next_ordinal: derived.next_ordinal, cassettes, ..Default::default() })
}

/// Counters a relaunched recorder re-derives from the trace instead of trusting any memory.
pub(super) struct Derived {
    next_ordinal: u64,
    cursors: SequenceCursors,
    taints: Vec<TaintRecord>,
    documents: u64,
    /// The highest `rseq` and each watch's latest sample, both of the last document.
    max_rseq: u64,
    unanswered: Vec<String>,
    last_samples: BTreeMap<WatchName, SampleValue>,
    /// Cassette lines each `replay` rule has answered with.
    served: BTreeMap<RuleId, std::collections::BTreeSet<usize>>,
    /// Recorded stream sessions each `replay` rule has played.
    sessions: BTreeMap<RuleId, std::collections::BTreeSet<(frontcap_core::cassette::Key, u64)>>,
}

/// `since` names, for each installed rule, the revision from which it has stayed unchanged; requests
/// that selected it since then make up its consumption.
pub(super) fn derive(events: &[BehaviorEvent], rules: &RuleSet, since: &BTreeMap<RuleId, u64>) -> Derived {
    let mut d = Derived {
        next_ordinal: 0,
        cursors: SequenceCursors::default(),
        taints: vec![],
        documents: 0,
        max_rseq: 0,
        unanswered: vec![],
        last_samples: BTreeMap::new(),
        served: BTreeMap::new(),
        sessions: BTreeMap::new(),
    };
    // The handshakes of streams: their entry is a recorded session, not a cassette line.
    let streams: std::collections::BTreeSet<&str> = events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::Stream(s) => Some(s.stream.as_str()),
            _ => None,
        })
        .collect();
    let mut open = std::collections::BTreeSet::new();
    let mut sample_document = 0;
    for e in events {
        // A popup's documents and observer reports are its own (`recorded_popups`): they do not move the
        // launch page's document count or report cursor.
        let popup = match &e.kind {
            EventKind::Navigation(n) => n.context.is_some(),
            EventKind::Focus(f) => f.context.is_some(),
            EventKind::Dialog(d) => d.context.is_some(),
            EventKind::Message(m) => m.context.is_some(),
            EventKind::Watch(w) => w.context.is_some(),
            _ => false,
        };
        if popup { continue; }
        match &e.kind {
            EventKind::Request(r) => {
                if let Some(n) = r.id.strip_prefix("req-").and_then(|n| n.parse::<u64>().ok()) {
                    d.next_ordinal = d.next_ordinal.max(n);
                }
                open.insert(r.id.clone());
                // A new or changed rule starts its own consumption history; an unchanged one continues.
                if !r.rule.as_ref().and_then(|rule| since.get(rule)).is_some_and(|from| r.rules_revision >= *from) { continue; }
                if let (Some(rule), Some(entry)) = (&r.rule, r.entry) {
                    if streams.contains(r.id.as_str()) {
                        d.sessions.entry(rule.clone()).or_default().insert((frontcap_core::cassette::Key::of("GET", &r.url, None), entry));
                    } else {
                        d.served.entry(rule.clone()).or_default().insert(entry as usize);
                    }
                }
                if let Some(rule) = &r.rule
                    && let Some(found) = rules.rules.iter().find(|x| &x.id == rule)
                    && let Action::Sequence { steps, after_last } = &found.action
                {
                    d.cursors.next(rule, steps, *after_last);
                }
            }
            EventKind::Response(r) => {
                open.remove(&r.id);
            }
            EventKind::Taint(t) => d.taints.push(t.clone()),
            EventKind::Navigation(n) => {
                d.documents = d.documents.max(n.document);
                if n.rseq != 0 {
                    if n.document != sample_document {
                        sample_document = n.document;
                        d.max_rseq = 0;
                        d.last_samples.clear();
                    }
                    d.max_rseq = d.max_rseq.max(n.rseq);
                } else {
                    // A new document: an earlier observer's cursor and values do not carry over.
                    d.max_rseq = 0;
                    d.last_samples.clear();
                }
            }
            EventKind::Dialog(dialog) => {
                if let Some(observer) = &dialog.observer {
                    if observer.document != 0 && observer.document != sample_document {
                        sample_document = observer.document;
                        d.max_rseq = 0;
                        d.last_samples.clear();
                    }
                    d.max_rseq = d.max_rseq.max(observer.rseq);
                }
            }
            EventKind::Focus(f) => {
                if f.document != 0 && f.document != sample_document {
                    sample_document = f.document;
                    d.max_rseq = 0;
                    d.last_samples.clear();
                }
                d.max_rseq = d.max_rseq.max(f.rseq);
            }
            EventKind::Message(m) => {
                if m.document != 0 && m.document != sample_document {
                    sample_document = m.document;
                    d.max_rseq = 0;
                    d.last_samples.clear();
                }
                d.max_rseq = d.max_rseq.max(m.rseq);
            }
            EventKind::Watch(w) => {
                // Cursor and values belong to one document; a later document starts both afresh.
                if w.document != 0 && w.document != sample_document {
                    sample_document = w.document;
                    d.max_rseq = 0;
                    d.last_samples.clear();
                }
                d.max_rseq = d.max_rseq.max(w.rseq);
                d.last_samples.insert(w.name.clone(), w.value);
            }
            _ => {}
        }
    }
    d.unanswered = open.into_iter().collect();
    d
}

/// Instruments a fresh page target and applies the determinism profile, leaving its clock paused at the
/// profile's initial date.
pub(super) async fn prepare_target(page: &Session, p: &DeterminismProfile, launch: &LaunchId, patched: bool, dialog_rules: &frontcap_core::dialog::DialogRules) -> Result<page::ScriptIdentifier, ChromiumError> {
    let dialogs = instrument::install(page, p, launch, patched, dialog_rules).await?;
    apply_overrides(page, p).await?;
    page.call(&emulation::SetVirtualTimePolicyParams {
        initial_virtual_time: Some(cnet::TimeSinceEpoch(p.initial_date_ms as f64 / 1000.0)),
        ..emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::Pause)
    })
    .await?;
    Ok(dialogs)
}

/// Initialize the profile's environment. A patched-engine reconnect adopts the
/// native environment instead: interventions may have changed these launch values.
pub(super) async fn apply_overrides(page: &Session, p: &DeterminismProfile) -> Result<(), ChromiumError> {
    page.call(&emulation::SetTimezoneOverrideParams::new(p.time_zone.clone())).await?;
    page.call(&emulation::SetLocaleOverrideParams { locale: Some(p.locale.clone()) }).await?;
    page.call(&emulation::SetDeviceMetricsOverrideParams::new(
        i64::from(p.viewport.width),
        i64::from(p.viewport.height),
        f64::from(p.viewport.device_scale_milli) / 1000.0,
        false,
    ))
    .await?;
    if p.touch {
        page.call(&emulation::SetTouchEmulationEnabledParams { max_touch_points: Some(5), ..emulation::SetTouchEmulationEnabledParams::new(true) }).await?;
    }
    // A Service Worker answering a request would replace the network rules with page code, and the HTTP
    // cache would answer by the real clock's freshness: every request reaches the rules.
    page.call(&cnet::SetBypassServiceWorkerParams { bypass: true }).await?;
    page.call(&cnet::SetCacheDisabledParams { cache_disabled: true }).await?;
    Ok(())
}

/// Puts each dedicated worker the page starts on the page's virtual clock before it runs: a worker thread's
/// clock is the process's virtual one, but its timers wake by the real clock unless its scheduler joins the
/// process's virtual time (then time advances no further than any thread's next task). A worker's own
/// requests stop virtual time at their issue like the page's (`pauseIfNetworkFetchesPending`); its network
/// events join the page's, and the platform releases its held requests on its session (engine patch 9).
/// Workers are held at start (`waitForDebuggerOnStart`) until this is done; other targets are let go at once.
/// A worker's own workers attach through its session and are joined the same way.
pub(super) fn join_workers_to_virtual_time(
    client: cdp::client::Caller,
    owner: String,
    attached: cdp::Subscription<target::AttachedToTargetEvent>,
    detached: cdp::Subscription<target::DetachedFromTargetEvent>,
    feeds: NetworkFeeds,
) -> Workers {
    let workers: Workers = std::sync::Arc::new(std::sync::Mutex::new(WorkerSessions { changed: feeds.worker_changes.clone(), ..Default::default() }));
    let joined = workers.clone();
    let (mut attached, mut detached) = (attached, detached);
    let (attach_feed, detach_feed) = (attached.feed(), detached.feed());
    tokio::spawn(async move {
        loop {
            // A target's detach follows its attach on the connection, so an attach already received is
            // taken first: a short-lived worker's routes are not left behind.
            let (ev, gone) = tokio::select! {
                biased;
                a = attached.next() => match a {
                    Ok(a) => (Some(a), None),
                    Err(cdp::CdpError::Closed) => break,
                    Err(_) => continue,
                },
                d = detached.next() => match d {
                    Ok(d) => (None, Some(d)),
                    Err(cdp::CdpError::Closed) => break,
                    Err(_) => continue,
                },
            };
            if let Some(d) = gone {
                for s in joined.lock().unwrap().forget(&d.session_id.0) {
                    client.unroute(&s);
                }
                continue;
            }
            let Some(ev) = ev else { continue };
            let session = ev.session_id.0.clone();
            let worker = ev.target_info.r#type == "worker";
            if worker {
                let identity = joined.lock().unwrap().lineages.remember(&session, &ev.target_info);
                if let Err(error) = identity {
                    let mut book = joined.lock().unwrap();
                    book.failure.get_or_insert(WorkerEnrollmentFailure {
                        target: ev.target_info.target_id.0.clone(), method: "Target.attachedToTarget", detail: error.to_string(),
                    });
                    book.changed.notify_one();
                    continue;
                }
                client.route(&session, &feeds.sent);
                client.route(&session, &feeds.response);
                client.route(&session, &feeds.finished);
                client.route(&session, &feeds.failed);
                // Its own workers are announced (taint) and joined like the page's.
                client.route(&session, &feeds.attached);
                client.route(&session, &attach_feed);
                client.route(&session, &detach_feed);
                // Which worker issued each request in flight (a release goes to that session alone), and the
                // workers it starts.
                let mut sent: cdp::Subscription<cnet::RequestWillBeSentEvent> = client.subscribe(Some(&session));
                let mut finished: cdp::Subscription<cnet::LoadingFinishedEvent> = client.subscribe(Some(&session));
                let mut failed: cdp::Subscription<cnet::LoadingFailedEvent> = client.subscribe(Some(&session));
                let mut children: cdp::Subscription<target::AttachedToTargetEvent> = client.subscribe(Some(&session));
                let mut waits: cdp::Subscription<cdp::protocol::inspector::FrontcapSynchronousWaitEvent> = client.subscribe(Some(&session));
                let (stop, mut stopped) = tokio::sync::oneshot::channel();
                joined.lock().unwrap().sessions.insert(session.clone(), WorkerSession {
                    _stop: stop, target: ev.target_info.target_id.0.clone(), enrollment: WorkerEnrollment::Joining, synchronous_wait: None,
                });
                let (book, by, gone_client) = (joined.clone(), session.clone(), client.clone());
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            biased;
                            // A child announced before this worker went is recorded first.
                            ev = children.next() => match ev {
                                Ok(ev) => {
                                    let mut w = book.lock().unwrap();
                                    if w.sessions.contains_key(&by) {
                                        w.parents.insert(ev.session_id.0, by.clone());
                                    } else {
                                        // This worker is gone already: so is its child (its detach came, if at
                                        // all, on a session no longer routed).
                                        for s in w.forget(&ev.session_id.0) {
                                            gone_client.unroute(&s);
                                        }
                                    }
                                }
                                Err(cdp::CdpError::Closed) => break,
                                Err(_) => {}
                            },
                            _ = &mut stopped => break,
                            ev = waits.next_stamped() => match ev {
                                Ok((arrival, event)) => {
                                    let mut book = book.lock().unwrap();
                                    if let Some(worker) = book.sessions.get_mut(&by) {
                                        worker.synchronous_wait = Some((event.request_id, arrival));
                                        book.changed.notify_one();
                                    }
                                }
                                Err(cdp::CdpError::Closed) => break,
                                Err(_) => {}
                            },
                            ev = sent.next() => match ev {
                                Ok(ev) => { book.lock().unwrap().requests.insert(ev.request_id.0, by.clone()); }
                                Err(cdp::CdpError::Closed) => break,
                                Err(_) => {}
                            },
                            ev = finished.next() => match ev {
                                Ok(ev) => { book.lock().unwrap().requests.remove(&ev.request_id.0); }
                                Err(cdp::CdpError::Closed) => break,
                                Err(_) => {}
                            },
                            // A failed load stays mapped: the platform may still answer it (an aborted one it
                            // holds), and that call goes to this worker alone.
                            ev = failed.next() => match ev {
                                Ok(_) => {}
                                Err(cdp::CdpError::Closed) => break,
                                Err(_) => {}
                            },
                        }
                    }
                });
            }
            // Each target on its own: one that cannot answer keeps no other waiting.
            let client = client.clone();
            let waiting = ev.waiting_for_debugger;
            let book = joined.clone();
            let owner = owner.clone();
            tokio::spawn(async move {
                if !worker {
                    if waiting { bounded(&client, &session, &runtime::RunIfWaitingForDebuggerParams::default()).await; }
                    return;
                }
                let result = enroll_worker(&client, &session, &owner, waiting, &book).await;
                let mut workers = book.lock().unwrap();
                match result {
                    Ok(()) => {
                        if let Some(worker) = workers.sessions.get_mut(&session) {
                            worker.enrollment = WorkerEnrollment::Ready;
                        }
                    },
                    Err(WorkerJoinError::Retired) => {
                        for session in workers.forget(&session) { client.unroute(&session); }
                    },
                    Err(WorkerJoinError::Failed { method, detail }) => {
                        if let Some(worker) = workers.sessions.get_mut(&session) {
                            worker.enrollment = WorkerEnrollment::Failed;
                            let target = worker.target.clone();
                            // A real setup failure remains a fault even if the worker later
                            // detaches. It must not turn into a certified progress boundary.
                            workers.failure.get_or_insert(WorkerEnrollmentFailure { target, method, detail });
                        }
                    },
                }
                workers.changed.notify_one();
            });
        }
    });
    workers
}

enum WorkerJoinError {
    Retired,
    Failed { method: &'static str, detail: String },
}

async fn worker_step<C: cdp::Command>(client: &cdp::client::Caller, session: &str, owner: Option<&str>, book: &Workers, command: &C) -> Result<(), WorkerJoinError> {
    let active = || book.lock().unwrap().sessions.contains_key(session);
    if !active() { return Err(WorkerJoinError::Retired); }
    let call = client.call(Some(session), command);
    tokio::pin!(call);
    loop {
        let result = tokio::time::timeout(Session::CALL_TIMEOUT, &mut call).await;
        if !active() { return Err(WorkerJoinError::Retired); }
        match result {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(cdp::CdpError::SessionDetached)) => return Err(WorkerJoinError::Retired),
            Ok(Err(error)) => return Err(WorkerJoinError::Failed { method: C::METHOD, detail: error.to_string() }),
            Err(_) => {
                // A reattached resident worker may be intentionally held in sync
                // XHR. Retain the original command only while a fresh native
                // owner inventory proves that exact worker is still blocked.
                if let Some(owner) = owner {
                    let target = book.lock().unwrap().sessions.get(session).map(|w| w.target.clone());
                    let inventory = emulation::FrontcapGetWorkersParams::default();
                    let snapshot = tokio::select! {
                        result = &mut call => return match result {
                            Ok(_) => Ok(()),
                            Err(cdp::CdpError::SessionDetached) => Err(WorkerJoinError::Retired),
                            Err(error) => Err(WorkerJoinError::Failed { method: C::METHOD, detail: error.to_string() }),
                        },
                        snapshot = tokio::time::timeout(Session::CALL_TIMEOUT,
                            client.call(Some(owner), &inventory)) => snapshot,
                    };
                    if !active() { return Err(WorkerJoinError::Retired); }
                    if let Ok(Ok(snapshot)) = snapshot
                        && snapshot.workers.iter().any(|w| Some(&w.target_id) == target.as_ref()
                            && w.thread_started && w.synchronous_wait_id.is_some()) { continue; }
                }
                return Err(WorkerJoinError::Failed { method: C::METHOD, detail: "command did not acknowledge before the call timeout".into() });
            }
        }
    }
}

async fn enroll_worker(client: &cdp::client::Caller, session: &str, owner: &str, waiting: bool, book: &Workers) -> Result<(), WorkerJoinError> {
    let retained_owner = (!waiting).then_some(owner);
    worker_step(client, session, retained_owner, book, &cnet::EnableParams::default()).await?;
    worker_step(client, session, retained_owner, book, &emulation::SetVirtualTimePolicyParams::new(emulation::VirtualTimePolicy::PauseIfNetworkFetchesPending)).await?;
    worker_step(client, session, retained_owner, book, &target::SetAutoAttachParams { flatten: Some(true), ..target::SetAutoAttachParams::new(true, true) }).await?;
    if waiting {
        worker_step(client, session, retained_owner, book, &runtime::RunIfWaitingForDebuggerParams::default()).await?;
    }
    Ok(())
}

/// A command to a target the platform does not drive, bounded like a page command; its outcome is not needed.
pub(super) async fn bounded<C: cdp::Command>(client: &cdp::client::Caller, session: &str, params: &C) {
    let _ = tokio::time::timeout(Session::CALL_TIMEOUT, client.call(Some(session), params)).await;
}

/// Answers dialogs that script cannot replace (`beforeunload`, or any a page raised another way) the
/// moment they open: a dialog blocks the page's main thread, and nothing else could reach it then.
/// The windows the page tried to open (a link or form aimed at a new window): the browser creates none
/// (`--block-new-web-contents`), and each attempt is kept for the trace. `window.open` is refused before
/// it reaches the browser (dialogs.js).
pub(super) fn record_new_windows(page: &Session) -> std::sync::Arc<std::sync::Mutex<Vec<String>>> {
    let tried: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let mut opening = page.subscribe::<page::WindowOpenEvent>();
    let out = tried.clone();
    tokio::spawn(async move {
        while let Ok(ev) = opening.next().await {
            out.lock().unwrap().push(ev.url);
        }
    });
    tried
}

pub(super) fn answer_native_dialogs(page: &Session, mut opening: cdp::Subscription<page::JavascriptDialogOpeningEvent>, rules: std::sync::Arc<std::sync::Mutex<frontcap_core::dialog::DialogRules>>) -> NativeDialogs {
    let answered: NativeDialogs = Default::default();
    // A caller, not a client: this task must not keep the connection open after its owners are gone.
    let client = page.client.caller();
    let session = page.id.clone();
    let out = answered.clone();
    tokio::spawn(async move {
        while let Ok(d) = opening.next().await {
            use frontcap_core::dialog::DialogReply;
            let kind = match d.r#type {
                page::DialogType::Alert => DialogKind::Alert,
                page::DialogType::Confirm => DialogKind::Confirm,
                page::DialogType::Prompt => DialogKind::Prompt,
                page::DialogType::Beforeunload => DialogKind::Beforeunload,
            };
            let Some(reply) = rules.lock().unwrap().answer(kind, &d.message, d.default_prompt.as_deref()) else { continue };
            let (accept, prompt_text) = match &reply {
                DialogReply::Alert => (true, None),
                DialogReply::Confirm(accepted) | DialogReply::Beforeunload(accepted) => (*accepted, None),
                DialogReply::Prompt(text) => (text.is_some(), text.clone()),
            };
            out.lock().unwrap().push(DialogRecord { kind, message: d.message, answer: reply.recorded(), native: None, observer: None, context: None });
            let _ = client.call(Some(&session), &page::HandleJavaScriptDialogParams { prompt_text, ..page::HandleJavaScriptDialogParams::new(accept) }).await;
        }
    });
    answered
}

/// Attaches to a page target and enables every domain the generation drives, with all subscriptions
/// in place before the first event can arrive.
pub(super) async fn open_session(client: cdp::Client, target_id: &str, patched: bool, dialog_rules: std::sync::Arc<std::sync::Mutex<frontcap_core::dialog::DialogRules>>, dialog_bindings: std::sync::Arc<std::sync::Mutex<native_dialogs::DialogBindings>>, initial_date_ms: u64, recovery: Option<DialogRecovery>) -> Result<(Session, Subs, NativeDialogs, Option<native_dialogs::Driver>), ChromiumError> {
    if !patched && dialog_rules.lock().unwrap().0.iter().any(|r| r.answer == frontcap_core::dialog::DialogAnswer::Hold) {
        return Err(ChromiumError::Unsupported("native dialog holds require the patched engine".into()));
    }
    let (page, recovered_driver, recovery_inventory) = match recovery {
        Some(recovery) => { let (page, driver, inventory) = recovery.into_parts().await?; (page, Some(driver), inventory) }
        None => {
            let attached = client.call(None, &target::AttachToTargetParams {
                flatten: Some(true), ..target::AttachToTargetParams::new(target::TargetID(target_id.into()))
            }).await?;
            (Session::new(client, attached.session_id.0), None, Vec::new())
        }
    };
    let (holds, answers) = BlockingHolds::new();
    let mut subs = Subs {
        recovery_inventory,
        holds,
        answers,
        binding: page.subscribe(),
        context: page.subscribe(),
        context_destroyed: page.subscribe(),
        contexts_cleared: page.subscribe(),
        console: page.subscribe(),
        log: page.subscribe(),
        navigated: page.subscribe(),
        paused: contexts::Routed::new(page.subscribe(), |e: &fetch::RequestPausedEvent| e.request_id.0.clone()),
        expired: page.subscribe(),
        sent: contexts::Routed::new(page.subscribe(), |e: &cnet::RequestWillBeSentEvent| e.request_id.0.clone()),
        requested: page.subscribe(),
        frame_navigated: page.subscribe(),
        detached: page.subscribe(),
        response: page.subscribe(),
        finished: page.subscribe(),
        failed: page.subscribe(),
        exceptions: page.subscribe(),
        attached: page.subscribe(),
        within: page.subscribe(),
        frames: page.subscribe(),
        chooser: page.subscribe(),
        workers: Default::default(),
    };
    // Subscribed before Page.enable, so a dialog that is already open when a recorder reconnects is seen.
    let (native, driver) = if let Some(driver) = recovered_driver {
        (Default::default(), Some(driver))
    } else if patched {
        (Default::default(), Some(native_dialogs::Driver::attach(&page, dialog_rules, dialog_bindings, initial_date_ms).await?))
    } else {
        (answer_native_dialogs(&page, page.subscribe(), dialog_rules), None)
    };
    page.call(&page::EnableParams::default()).await?;
    // A file chooser a page opens is reported (`Page.fileChooserOpened`) instead of shown: `upload`
    // through a "Choose…" control sets its files.
    page.call(&page::SetInterceptFileChooserDialogParams { enabled: true, cancel: None }).await?;
    // Every rendered frame, with no frame of its own: captures read these and change nothing (S16). Not on the
    // patched engine, where a capture copies the surface the display shows (`copy_surface`).
    if !patched {
        page.call(&page::StartScreencastParams { format: Some(page::StartScreencastParamsFormat::Png), every_nth_frame: Some(1), ..Default::default() }).await?;
    }
    page.call(&runtime::EnableParams::default()).await?;
    page.call(&cdp::protocol::log::EnableParams::default()).await?;
    page.call(&cnet::EnableParams::default()).await?;
    page.call(&dom::EnableParams::default()).await?;
    // Workers announce themselves; they run outside the seeded realms (taint, `drain_page_events`).
    let feeds = NetworkFeeds { worker_changes: subs.workers.lock().unwrap().changed.clone(), attached: subs.attached.feed(), sent: subs.sent.feed(), response: subs.response.feed(), finished: subs.finished.feed(), failed: subs.failed.feed() };
    subs.workers = join_workers_to_virtual_time(page.client.caller(), page.id.to_string(), page.subscribe(), page.subscribe(), feeds);
    page.call(&target::SetAutoAttachParams { flatten: Some(true), ..target::SetAutoAttachParams::new(true, true) }).await?;
    let intercept = fetch::EnableParams {
        patterns: Some(vec![fetch::RequestPattern {
            url_pattern: Some("*".into()),
            request_stage: Some(fetch::RequestStage::Request),
            ..Default::default()
        }]),
        ..Default::default()
    };
    // The engine's interception has one consumer. A controller that has just gone (a recorder replaced
    // after its death, a dropped connection) holds it until the browser has seen its connection close; a
    // live one still holds it after this bound, and taking over fails.
    let wall = std::time::Instant::now();
    loop {
        match page.call(&intercept).await {
            Err(ChromiumError::Cdp(cdp::CdpError::Protocol { message, .. }))
                if message.contains("already has a consumer") && wall.elapsed() < Duration::from_secs(2) =>
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            taken => {
                taken?;
                break;
            }
        }
    }
    Ok((page, subs, native, driver))
}

/// The engine a generation runs (`engine.json` in its bundle): its revision — `<revision>+frontcap.<patchset>`
/// for frontcap's patched build — and, for the Chrome for Testing baseline, the patches it lacks.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct EngineRecord {
    revision: String,
    patched: bool,
}

/// The engine a generation launches: the patched build of the current patch set when it has been built,
/// the pinned baseline otherwise, with the names of the patches the baseline lacks.
pub(super) fn engine(repo_root: &std::path::Path) -> Result<(std::path::PathBuf, EngineRecord, Vec<String>), ChromiumError> {
    let pin = browser_pin::load_pin(repo_root)?;
    let platform = browser_pin::current_platform()?;
    Ok(match browser_pin::engine::patched_binary(repo_root, platform)? {
        Some((binary, m)) => (binary, EngineRecord { revision: format!("{}+frontcap.{}", pin.revision, m.patchset), patched: true }, Vec::new()),
        None => {
            let lacks = browser_pin::engine::patches(repo_root)?.iter().filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned())).collect();
            (browser_pin::install(repo_root, &pin, platform)?, EngineRecord { revision: pin.revision.to_string(), patched: false }, lacks)
        }
    })
}

#[cfg(test)]
#[path = "../rule_recovery_tests.rs"]
mod rule_revision_tests;

#[cfg(test)]
#[path = "worker_enrollment_tests.rs"]
mod worker_enrollment_tests;
