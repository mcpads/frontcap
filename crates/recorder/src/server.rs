//! The recorder's socket: connections, leases bound to connections, serialization of mutations, and the
//! status snapshot that observation reads without waiting for a running request.
use crate::control::ControlState;
use crate::execute;
use crate::runtime::LaunchSpec;
use frontcap_chromium::generation::Generation;
use frontcap_core::event::ControllerChange;
use frontcap_core::ids::RequestId;
use frontcap_core::journal::Journal;
use frontcap_recorder_protocol::frame::{read_frame, write_frame};
use frontcap_recorder_protocol::{
    Answer, Call, ClientFrame, ErrorCode, ErrorReply, LeaseToken, Op, Reply, ServerFrame, StatusReport,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub struct Recorder {
    pub(crate) cleanup_pending: std::sync::atomic::AtomicBool,
    browser_identity: Option<frontcap_core::process::ProcessIdentity>,
    pointer_admission: std::sync::atomic::AtomicBool,
    pub dir: PathBuf,
    pub spec: LaunchSpec,
    pub(crate) generation: tokio::sync::Mutex<Option<Generation>>,
    pub(crate) lease: Mutex<Option<(LeaseToken, u64)>>,
    pub(crate) running: Mutex<Option<RequestId>>,
    /// A running probe's request id and its steps': nothing else changes the generation until it ends.
    probing: Mutex<Option<(RequestId, Vec<RequestId>)>>,
    /// Set by `Call::Cancel` to cancel the running request's waits and a probe's later steps; cleared
    /// when the next request begins. The generation's own handle (`cancel_handle`).
    cancel_requested: std::sync::RwLock<Arc<std::sync::atomic::AtomicBool>>,
    /// The running wait's reached instant (`Generation::progress_handle`).
    progress: std::sync::RwLock<Arc<std::sync::atomic::AtomicU64>>,
    /// A request not begun yet whose waits are cancelled at an instant (a replay of a cancelled request).
    armed_cancel: Mutex<Option<(RequestId, frontcap_core::time::VirtualTime)>>,
    /// The generation's native dialog session; taking the browser again after a process image replaces it.
    dialog_session: std::sync::RwLock<Option<frontcap_chromium::generation::DialogSession>>,
    /// The held request the page's main thread waits for, answered while its operation runs.
    blocking_holds: std::sync::RwLock<frontcap_chromium::generation::BlockingHolds>,
    pub(crate) dialog_admission: tokio::sync::Mutex<()>,
    pub(crate) dependent_changed: tokio::sync::Notify,
    pub(crate) journal: Mutex<Journal>,
    pub(crate) control: Mutex<ControlState>,
    status: RwLock<StatusReport>,
    detached_completions: Mutex<Vec<RequestId>>,
    /// Controller changes waiting for the generation (busy with a request) to record them.
    controller_events: Mutex<Vec<ControllerChange>>,
    stopped: Mutex<bool>,
    failure: Mutex<Option<String>>,
    shutdown: tokio::sync::Notify,
    /// `inputs/commands.jsonl`; `None` once a write failed (the run is then not replayable).
    commands: Mutex<Option<frontcap_core::inputs::CommandLog>>,
    /// The trace as read so far (`read_trace`), shared with the generation that writes it.
    trace_tail: Mutex<Option<(PathBuf, Arc<Mutex<frontcap_core::log::BehaviorTail>>)>>,
}

/// Adds `what` to the inputs the run lacks, once.
pub(crate) fn missing_input(c: &mut ControlState, what: &str) {
    if !c.missing_inputs.iter().any(|m| m == what) {
        c.missing_inputs.push(what.to_string());
    }
}

/// An answer's outcome as re-execution compares it: the outcome tag, or `error:<code>`. What the trace
/// does not record of an answer is part of it: an evaluated value (by its hash) and a watch's sample.
pub fn outcome_tag(answer: &Answer) -> String {
    use frontcap_recorder_protocol::Outcome;
    match answer {
        Answer::Ok(frontcap_recorder_protocol::Reply::Done { outcome, .. }) => {
            let tag = serde_json::to_value(outcome).ok().and_then(|v| v.get("outcome").and_then(|t| t.as_str()).map(str::to_string)).unwrap_or_default();
            match outcome {
                Outcome::Evaluated { value } => format!("{tag}:{}", frontcap_core::canonical::sha256_hex(&frontcap_core::canonical::canonical_of(value))),
                Outcome::Sample { value } => format!("{tag}:{}", serde_json::to_value(value).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()),
                _ => tag,
            }
        }
        Answer::Ok(_) => "ok".into(),
        Answer::Err(e) => format!("error:{}", serde_json::to_value(e.code).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()),
    }
}

fn held_requests(g: &Generation) -> Vec<frontcap_recorder_protocol::HeldRequest> {
    use frontcap_recorder_protocol::HeldRequest;
    g.held_requests().into_iter().map(|(ordinal, h)| {
        let body = h.body.as_ref().map(|b| {
            let mut end = b.len().min(HeldRequest::BODY_LIMIT);
            while !b.is_char_boundary(end) { end -= 1; }
            b[..end].to_string()
        });
        HeldRequest {
            ordinal, method: h.method.clone(), url: h.url.clone(), rule: h.rule.clone(), rules_revision: h.rules_revision,
            body, body_bytes: h.body.as_ref().map(String::len), body_unavailable: h.body_unavailable, stream: None,
        }
    }).chain(g.held_streams().into_iter().map(|(ordinal, h)| HeldRequest {
        ordinal, method: "GET".into(), url: h.url.clone(), rule: h.rule.clone(), rules_revision: h.rules_revision,
        body: None, body_bytes: None, body_unavailable: false, stream: Some(if h.open { "open" } else { "connecting" }.into()),
    })).collect()
}

fn err(code: ErrorCode, message: impl Into<String>) -> Answer {
    Answer::Err(ErrorReply::new(code, message))
}

impl Recorder {
    pub fn new(dir: PathBuf, spec: LaunchSpec, g: Generation, journal: Journal, mut control: ControlState) -> Arc<Recorder> {
        let browser_identity = frontcap_core::process::ProcessIdentity::of(g.endpoint().browser_pid).ok().flatten();
        let dialogs = g.dialog_session();
        let holds = g.blocking_holds();
        let cancel_requested = g.cancel_handle();
        let progress = g.progress_handle();
        let status = StatusReport {
            suspension: None,
            input_cleanup_pending: false,
            pointer: g.held_pointer(),
            keys: g.held_keys().to_vec(),
            composing: g.may_compose(),
            launch_id: spec.launch_id.clone(),
            run_id: spec.run_id.clone(),
            t_us: g.now(),
            date_ms: 0,
            lease_held: false,
            running: None,
            running_t_us: None,
            held: g.held(),
            held_requests: held_requests(&g),
            watches: g.watches().list().into_iter().map(|(n, _)| n.clone()).collect(),
            watch_predicates: g.watches().list().into_iter().map(|(n, p)| (n.clone(), p.clone())).collect(),
            last_completed_request_id: journal.last_completed().cloned(),
            faults: control.faults.clone(),
            stopped: false,
            observation_seq: g.next_seq(),
            dom_mutations: g.dom_mutations(),
            bundle: g.trace_path().parent().map(|p| p.to_path_buf()).unwrap_or_default(),
        };
        let commands = frontcap_core::inputs::CommandLog::open(&status.bundle).ok();
        if commands.is_none() {
            missing_input(&mut control, "inputs/commands.jsonl");
        }
        Arc::new(Recorder {
            cleanup_pending: std::sync::atomic::AtomicBool::new(false),
            browser_identity,
            pointer_admission: std::sync::atomic::AtomicBool::new(false),
            dir,
            spec,
            generation: tokio::sync::Mutex::new(Some(g)),
            lease: Mutex::new(None),
            running: Mutex::new(None),
            probing: Mutex::new(None),
            cancel_requested: std::sync::RwLock::new(cancel_requested),
            progress: std::sync::RwLock::new(progress),
            armed_cancel: Mutex::new(None),
            dialog_session: std::sync::RwLock::new(dialogs),
            blocking_holds: std::sync::RwLock::new(holds),
            dialog_admission: tokio::sync::Mutex::new(()),
            dependent_changed: tokio::sync::Notify::new(),
            journal: Mutex::new(journal),
            control: Mutex::new(control),
            status: RwLock::new(status),
            detached_completions: Mutex::new(Vec::new()),
            controller_events: Mutex::new(Vec::new()),
            stopped: Mutex::new(false),
            failure: Mutex::new(None),
            shutdown: tokio::sync::Notify::new(),
            commands: Mutex::new(commands),
            trace_tail: Mutex::new(None),
        })
    }

    pub fn is_stopped(&self) -> bool {
        *self.stopped.lock().unwrap()
    }

    pub(crate) fn mark_stopped(&self) {
        *self.stopped.lock().unwrap() = true;
        self.status.write().unwrap().stopped = true;
    }

    pub(crate) fn mark_failed(&self, reason: String) {
        *self.failure.lock().unwrap() = Some(reason);
        self.mark_stopped();
    }

    pub(crate) fn failure_reason(&self) -> Option<String> { self.failure.lock().unwrap().clone() }

    pub(crate) fn dialogs(&self) -> Option<frontcap_chromium::generation::DialogSession> {
        self.dialog_session.read().unwrap().clone()
    }

    pub(crate) fn holds(&self) -> frontcap_chromium::generation::BlockingHolds {
        self.blocking_holds.read().unwrap().clone()
    }

    /// The handles of a generation the recorder took again (after a process image): the abandoned
    /// generation's would never report a dialog or held request of this one.
    pub(crate) fn adopt_handles(&self, g: &Generation) {
        *self.dialog_session.write().unwrap() = g.dialog_session();
        *self.blocking_holds.write().unwrap() = g.blocking_holds();
        *self.cancel_requested.write().unwrap() = g.cancel_handle();
        *self.progress.write().unwrap() = g.progress_handle();
    }

    fn cancel_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.cancel_requested.read().unwrap().clone()
    }

    /// The instant `id`'s waits are cancelled at, if a replay armed one for it.
    pub(crate) fn take_armed_cancel(&self, id: &RequestId) -> Option<frontcap_core::time::VirtualTime> {
        let mut armed = self.armed_cancel.lock().unwrap();
        match &*armed {
            Some((armed_id, _)) if armed_id == id => armed.take().map(|(_, at)| at),
            _ => None,
        }
    }

    /// `Call::Cancel`: asks the running request or probe to stop, or arms a request not begun yet.
    fn cancel(&self, lease: Option<LeaseToken>, id: RequestId, at_us: Option<frontcap_core::time::VirtualTime>) -> Answer {
        match (&*self.lease.lock().unwrap(), &lease) {
            (Some((t, _)), Some(l)) if t == l => {}
            (None, None) => {}
            _ => return err(ErrorCode::LeaseRequired, "the call does not present the live lease"),
        }
        if let Some(at) = at_us {
            if self.journal.lock().unwrap().state(&id).is_some() {
                return err(ErrorCode::InvalidArguments, format!("{id} has begun; an instant is armed only for a request not sent yet"));
            }
            *self.armed_cancel.lock().unwrap() = Some((id.clone(), at));
            return Answer::Ok(Reply::Cancelling { request_id: id });
        }
        let probe = self.probing.lock().unwrap().as_ref().map(|(probe, _)| probe.clone());
        let running = self.running.lock().unwrap().clone();
        if probe.as_ref() != Some(&id) && running.as_ref() != Some(&id) {
            let begun = self.journal.lock().unwrap().state(&id).is_some();
            let mut reply = ErrorReply::new(ErrorCode::NotFound, if begun { format!("{id} is not running") } else { format!("no request {id}") });
            reply.recovery = if begun { "read its answer with session result".into() } else { "status names the running request".into() };
            return Answer::Err(reply);
        }
        if let Some(step) = running.filter(|r| matches!(self.current_suspension(r), Ok(Some(_)))) {
            return err(ErrorCode::Busy, format!("{step} is suspended: answer its dialog or held request first"));
        }
        self.cancel_flag().store(true, std::sync::atomic::Ordering::SeqCst);
        Answer::Ok(Reply::Cancelling { request_id: id })
    }

    pub(crate) async fn fail_operation(&self, id: &RequestId, t_us: frontcap_core::time::VirtualTime, detail: String) -> String {
        self.fail_execution(vec![id.to_string()], t_us, detail).await
    }

    async fn fail_execution(&self, affected: Vec<String>, t_us: frontcap_core::time::VirtualTime, mut detail: String) -> String {
        self.mark_failed(detail.clone());
        self.dependent_changed.notify_waiters();
        let terminated = match self.browser_identity.clone() {
            Some(identity) => tokio::task::spawn_blocking(move || identity.kill()).await
                .map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| e.to_string())),
            None => Err("owned browser identity is unavailable".into()),
        };
        if let Err(e) = terminated { detail.push_str(&format!("; browser termination failed: {e}")); }
        if let Err(e) = self.push_fault(frontcap_recorder_protocol::Fault { at_us: t_us, affected, detail: detail.clone() }) {
            detail.push_str(&format!("; could not persist fault: {e}"));
        }
        self.mark_failed(detail.clone());
        self.shutdown.notify_one();
        detail
    }

    /// Applies a change to the control state and saves it atomically. Observations are saved by
    /// `update_observations` alone.
    pub(crate) fn update_control(&self, change: impl FnOnce(&mut ControlState)) -> std::io::Result<()> {
        let mut c = self.control.lock().unwrap();
        change(&mut c);
        frontcap_core::state::save_atomic(&crate::runtime::control_path(&self.dir), &*c)
    }

    /// Replaces the control state's observations and saves them atomically.
    pub(crate) fn update_observations(&self, state: frontcap_chromium::generation::ObservationState) -> std::io::Result<()> {
        let mut c = self.control.lock().unwrap();
        c.observations = state;
        frontcap_core::state::save_atomic(&crate::runtime::observations_path(&self.dir), &c.observations)
    }

    pub(crate) fn push_fault(&self, fault: frontcap_recorder_protocol::Fault) -> std::io::Result<()> {
        self.update_control(|c| c.faults.push(fault))
    }

    pub(crate) fn dispatched(&self, id: &RequestId) -> Result<(), frontcap_core::journal::JournalError> {
        self.journal.lock().unwrap().dispatched(id)
    }

    /// Writes the request's re-execution input and its completion, then clears what it kept for recovery.
    pub(crate) fn complete(&self, id: &RequestId, answer: &Answer) -> Result<(), crate::RecorderError> {
        // Keep dependency admission and parent publication under the same journal lock. Checking
        // after record_input would already publish an incomplete replay command before rejecting it.
        let mut journal = self.journal.lock().unwrap();
        journal.validate_completion(id)?;
        if self.control.lock().unwrap().predicate_watches.contains_key(id) {
            return Err(crate::RecorderError::Missing("predicate wait cleanup is unresolved".into()));
        }
        self.record_input(id, answer, &journal)?;
        journal.complete(id, serde_json::to_value(answer).expect("answers serialize"))?;
        drop(journal);
        self.update_control(|c| {
            if c.suspension.as_ref().is_some_and(|s| &s.request_id == id) { c.suspension = None; }
            c.pending_ops.remove(id);
            c.state_plans.remove(id);
            c.state_receipts.remove(id);
            c.pending_inputs.remove(id);
            if c.inflight.as_ref().is_some_and(|i| &i.request_id == id) {
                c.inflight = None;
            }
        })?;
        Ok(())
    }

    /// Appends the request's line to `inputs/commands.jsonl`. `stop` ends every replay and is not
    /// written: the bundle is finalized by then.
    fn record_input(&self, id: &RequestId, answer: &Answer, journal: &Journal) -> Result<(), crate::RecorderError> {
        let (op, start) = {
            let c = self.control.lock().unwrap();
            (c.pending_ops.get(id).cloned(), c.pending_inputs.get(id).cloned())
        };
        let Some(op) = op else { return Ok(()) };
        // A probe's steps are requests of their own, each with its line.
        if op == Op::Stop || matches!(op, Op::Probe { .. }) || self.is_stopped() {
            return Ok(());
        }
        let Some(start) = start else {
            // Interrupted before its start was saved: recovery completed it, but its inputs are lost.
            return Ok(self.update_control(|c| missing_input(c, &format!("inputs of {id}")))?);
        };
        if let Answer::Err(e) = answer
            && e.code == ErrorCode::Internal
        {
            // What an internal failure applied is not known, so the run cannot be executed again.
            return Ok(self.update_control(|c| missing_input(c, &format!("inputs of {id}")))?);
        }
        let mut op_value = serde_json::to_value(&op).expect("ops serialize");
        // An upload is re-executed from the bundle's copies, wherever the files were; one refused before
        // any copy names copies that do not exist, so its replay is refused the same way. Never a path
        // of the recording host.
        if let Op::Act { action: frontcap_core::action::Action::Upload { files, .. }, .. } = &op
            && let Some(slot) = op_value.pointer_mut("/action/files")
        {
            let named: Vec<String> = if start.uploads.len() == files.len() {
                start.uploads.clone()
            } else {
                let base = |f: &String| std::path::Path::new(f).file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
                files.iter().map(|f| format!("{}/unreadable/{}", frontcap_chromium::generation::UPLOADS, base(f))).collect()
            };
            *slot = serde_json::to_value(named).expect("paths serialize");
        }
        if let (Op::SetRules { .. } | Op::EditRules { .. }, Answer::Ok(frontcap_recorder_protocol::Reply::Done {
            outcome: frontcap_recorder_protocol::Outcome::RulesInstalled(receipt), ..
        })) = (&op, answer) {
            let path = self.spec.runs_root.join(self.spec.run_id.as_str()).join(format!("inputs/rules/{:04}.json", receipt.revision));
            let rules: frontcap_core::rules::RuleSet = frontcap_core::state::load(&path)?
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "published rule input is missing"))?;
            if let Op::EditRules { expected_revision, boundary, .. } = &op {
                // Successful convenience edits replay as their captured full installation.
                op_value = serde_json::to_value(Op::SetRules {
                    expected_revision: *expected_revision, boundary: boundary.clone(), rules,
                }).expect("op serializes");
            } else { op_value["rules"] = serde_json::to_value(rules).expect("rules serialize"); }
        }
        if let (Op::SetRules { rules, .. }, Answer::Err(_)) = (&op, answer) {
            let attempt = frontcap_core::canonical::sha256_hex(id.as_str());
            let path = self.spec.runs_root.join(self.spec.run_id.as_str()).join(format!("inputs/rule-attempts/{attempt}.json"));
            let prepared: Option<frontcap_core::rules::RuleSet> = frontcap_core::state::load(&path)?;
            let rules = prepared.unwrap_or_else(|| {
                // Refused before completing asset preparation. Preserve rule validation and the
                // early boundary checks, but never retry reading a recording host's file.
                let mut rules = rules.clone();
                for rule in &mut rules.rules {
                    if let frontcap_core::rules::Action::Replay { cassette, .. } = &mut rule.action {
                        *cassette = format!("inputs/unreadable-cassettes/{}.jsonl", frontcap_core::canonical::sha256_hex(cassette));
                    }
                }
                rules
            });
            op_value["rules"] = serde_json::to_value(rules).expect("rules serialize");
        }
        if let (Op::EditRules { edit: frontcap_core::rules::RuleEdit::Add { rule, .. }, .. }, Answer::Err(_)) = (&op, answer) {
            let attempt = frontcap_core::canonical::sha256_hex(id.as_str());
            let path = self.spec.runs_root.join(self.spec.run_id.as_str()).join(format!("inputs/rule-attempts/{attempt}.json"));
            let prepared: Option<frontcap_core::rules::RuleSet> = frontcap_core::state::load(&path)?;
            let saved = prepared.and_then(|rules| rules.rules.into_iter().find(|saved| saved.id == rule.id));
            let saved = saved.unwrap_or_else(|| {
                let mut rule = rule.as_ref().clone();
                if let frontcap_core::rules::Action::Replay { cassette, .. } = &mut rule.action {
                    *cassette = format!("inputs/unreadable-cassettes/{}.jsonl", frontcap_core::canonical::sha256_hex(cassette));
                }
                rule
            });
            op_value["edit"]["rule"] = serde_json::to_value(saved).expect("rule serializes");
        }
        let dialogs = match self.dialog_inputs(journal, id) {
            Ok(inputs) => inputs,
            Err(reason) => return Ok(self.update_control(|c| missing_input(c, &format!("dialog inputs of {id}: {reason}")))?),
        };
        let answers = match self.held_answer_inputs(journal, id) {
            Ok(inputs) => inputs,
            Err(reason) => return Ok(self.update_control(|c| missing_input(c, &format!("held-request answers of {id}: {reason}")))?),
        };
        let line = frontcap_core::inputs::CommandLine {
            request_id: id.clone(),
            t_us: start.t_us,
            op: op_value,
            outcome: outcome_tag(answer),
            refs: start.refs,
            dialogs,
            answers,
            cancelled_at_us: start.cancelled_at_us,
        };
        let mut commands = self.commands.lock().unwrap();
        let written = commands.as_mut().map(|log| log.append(&line));
        if !matches!(written, Some(Ok(()))) {
            *commands = None;
            drop(commands);
            self.update_control(|c| missing_input(c, "inputs/commands.jsonl"))?;
        }
        Ok(())
    }

    /// Records queued controller changes; the caller holds the generation.
    pub(crate) fn flush_controller(&self, g: &mut Generation) {
        let changes = std::mem::take(&mut *self.controller_events.lock().unwrap());
        for c in changes {
            let _ = g.record_controller(c);
        }
    }

    fn controller(&self, changes: &[ControllerChange]) {
        self.controller_events.lock().unwrap().extend_from_slice(changes);
        if let Ok(mut g) = self.generation.try_lock()
            && let Some(g) = g.as_mut()
        {
            self.flush_controller(g);
        }
    }

    /// Refreshes the status snapshot from the generation; the caller holds it.
    pub(crate) fn refresh(&self, g: &Generation) {
        let mut s = self.status.write().unwrap();
        s.pointer = g.held_pointer();
        s.keys = g.held_keys().to_vec();
        s.composing = g.may_compose();
        s.t_us = g.now();
        s.held = g.held();
        s.held_requests = held_requests(g);
        s.watches = g.watches().list().into_iter().map(|(n, _)| n.clone()).collect();
        s.watch_predicates = g.watches().list().into_iter().map(|(n, p)| (n.clone(), p.clone())).collect();
        s.last_completed_request_id = self.journal.lock().unwrap().last_completed().cloned();
        s.faults = self.control.lock().unwrap().faults.clone();
        s.observation_seq = g.next_seq();
        s.dom_mutations = g.dom_mutations();
    }

    fn snapshot(&self) -> StatusReport {
        let mut s = self.status.read().unwrap().clone();
        s.input_cleanup_pending = self.cleanup_pending.load(std::sync::atomic::Ordering::SeqCst);
        s.lease_held = self.lease.lock().unwrap().is_some();
        s.date_ms = self.spec.profile.initial_date_ms + s.t_us.0 / 1000;
        s.running = self.running.lock().unwrap().clone();
        s.running_t_us = s.running.is_some().then(|| {
            let reached = frontcap_core::time::VirtualTime(self.progress.read().unwrap().load(std::sync::atomic::Ordering::Relaxed));
            reached.max(s.t_us)
        });
        s.suspension = s.running.as_ref().and_then(|id| self.current_suspension(id).ok().flatten());
        s
    }

    fn connection_closed(self: &Arc<Self>, conn: u64) {
        let expired = {
            let mut lease = self.lease.lock().unwrap();
            if matches!(&*lease, Some((_, c)) if *c == conn) {
                let cleanup = self.running.lock().unwrap().is_some() || self.pointer_admission.load(std::sync::atomic::Ordering::SeqCst) || {
                    let s = self.status.read().unwrap();
                    s.pointer.is_some() || !s.keys.is_empty() || s.composing
                };
                self.cleanup_pending.store(cleanup, std::sync::atomic::Ordering::SeqCst);
                *lease = None;
                true
            } else {
                false
            }
        };
        if expired {
            self.controller(&[ControllerChange::LeaseExpired, ControllerChange::Detached]);
            if self.cleanup_pending.load(std::sync::atomic::Ordering::SeqCst) {
                let rec = self.clone();
                tokio::spawn(async move { let _ = rec.cleanup_inputs().await; });
            }
        }
    }

    /// Accepted input finishes first. No replacement lease is published before its held controls
    /// have been released and journaled: a composition cancelled, the pointer cancelled, then held
    /// keys released, other keys before modifiers and each last pressed first, so the other keys are
    /// released under the modifiers still held. A failed native cleanup terminates the generation.
    pub(crate) async fn cleanup_inputs(self: &Arc<Self>) -> Result<(), String> {
        use frontcap_core::action::Action;
        if let Err(detail) = self.finish_or_dismiss_dialogs().await {
            return Err(self.fail_dialog_cleanup(&detail).await);
        }
        let mut failure = None;
        // The composition first: cancelling it needs the focus the keys and pointer may still hold.
        let composing = match self.generation.lock().await.as_mut() {
            Some(g) if g.may_compose() => g.current_composition().await.map(|c| c.is_some()).unwrap_or(true),
            _ => false,
        };
        if composing {
            let seed = format!("{}:compose", self.journal.lock().unwrap().last_completed().map(ToString::to_string).unwrap_or_default());
            failure = self.cleanup_one("compose", &seed, Action::Compose { text: String::new() }, |s| s.composition.is_none()).await.err();
        }
        let held = self.generation.lock().await.as_ref().and_then(Generation::held_pointer);
        if let Some(held) = held.filter(|_| failure.is_none()) {
            let seed = held.handle.request_id.to_string();
            failure = self.cleanup_one("pointer", &seed, Action::PointerCancel { handle: held.handle }, |s| s.held.is_none()).await.err();
        }
        let keys = self.generation.lock().await.as_ref().map(Generation::key_release_order).unwrap_or_default();
        for key in keys {
            if failure.is_some() { break; }
            let seed = format!("{}:{key}", self.journal.lock().unwrap().last_completed().map(ToString::to_string).unwrap_or_default());
            let released = key.clone();
            failure = self.cleanup_one("key", &seed, Action::KeyUp { key }, move |s| !s.keys.contains(&released)).await.err();
        }
        // A dialog cleanup failure has already recorded termination evidence. Do not overwrite its
        // reason or wait on the blocked Generation again when termination itself failed.
        if let Some(detail) = &failure && self.failure_reason().is_none() {
            let mut guard = self.generation.lock().await;
            if let Some(mut g) = guard.take() {
                let _ = self.push_fault(frontcap_recorder_protocol::Fault { at_us: g.now(), affected: vec![], detail: detail.clone() });
                let _ = g.record_recovery_taint(detail.clone());
                g.set_missing_inputs(vec!["held_input_cleanup_failed".into()]);
                let _ = g.stop().await;
            }
            self.mark_failed(detail.clone());
        }
        self.cleanup_pending.store(false, std::sync::atomic::Ordering::SeqCst);
        if self.is_stopped() { self.shutdown.notify_one(); }
        match failure { Some(error) => Err(error), None => Ok(()) }
    }

    pub(crate) async fn fail_dialog_cleanup(&self, detail: &str) -> String {
        let t_us = self.dialogs().and_then(|dialogs| dialogs.pending().ok().flatten())
            .map(|dialog| dialog.t_us).unwrap_or_else(|| self.status.read().unwrap().t_us);
        let affected = self.running.lock().unwrap().iter().map(ToString::to_string).collect();
        let detail = self.fail_execution(affected, t_us, detail.into()).await;
        self.cleanup_pending.store(false, std::sync::atomic::Ordering::SeqCst);
        detail
    }

    /// Runs one journaled cleanup operation under a server request id derived from `seed`, and checks
    /// that its answer shows the input released.
    async fn cleanup_one(self: &Arc<Self>, what: &str, seed: &str, action: frontcap_core::action::Action, released: impl Fn(&frontcap_core::pointer::PointerState) -> bool) -> Result<(), String> {
        let digest = frontcap_core::canonical::sha256_hex(seed);
        let mut id = RequestId::new(format!("mcp-{what}-cleanup-{digest}")).expect("bounded server id");
        let op = Op::Act { action, then: None, context: None };
        let answer = loop {
            *self.running.lock().unwrap() = Some(id.clone());
            let answer = tokio::select! {
                answer = execute::run(self, &id, op.clone()) => answer,
                result = self.finish_or_dismiss_dialogs() => {
                    let detail = result.err().unwrap_or_else(|| "cleanup operation lost its running owner".into());
                    let detail = self.fail_dialog_cleanup(&detail).await;
                    *self.running.lock().unwrap() = None;
                    return Err(detail);
                }
            };
            if matches!(answer, Answer::Ok(Reply::Done { outcome: frontcap_recorder_protocol::Outcome::NotApplied, .. })) {
                // A crash before dispatch leaves a terminal no-effect request. Preserve that
                // result and start a distinct cleanup attempt; NotApplied is not released input.
                let digest = frontcap_core::canonical::sha256_hex(id.as_str());
                id = RequestId::new(format!("mcp-{what}-cleanup-{digest}")).expect("bounded server id");
                continue;
            }
            break answer;
        };
        *self.running.lock().unwrap() = None;
        let error = match answer {
            Answer::Err(error) => Some(error.message),
            Answer::Ok(Reply::Done { outcome: frontcap_recorder_protocol::Outcome::Pointer { state, .. }, .. }) if released(&state) => None,
            _ => Some("cleanup did not establish released input".to_string()),
        };
        match error {
            Some(error) => Err(format!("held {what} cleanup failed ({id}): {error}")),
            None => Ok(()),
        }
    }

    async fn handle(self: &Arc<Self>, conn: u64, call: Call) -> Answer {
        match call {
            Call::Native { .. } => err(ErrorCode::UnsupportedCapability, "this recorder hosts Chromium"),
            Call::Hello => Answer::Ok(Reply::Hello { launch_id: self.spec.launch_id.clone() }),
            Call::Status => Answer::Ok(Reply::Status { status: Box::new(self.snapshot()) }),
            Call::InspectControl { lease: supplied } => {
                use frontcap_recorder_protocol::ControlOwnership;
                let mut status = self.snapshot();
                let lease = self.lease.lock().unwrap();
                let ownership = match lease.as_ref() {
                    None => ControlOwnership::None,
                    Some((token, _)) if supplied.as_ref() == Some(token) => ControlOwnership::ThisController,
                    Some(_) => ControlOwnership::Other,
                };
                status.lease_held = lease.is_some();
                Answer::Ok(Reply::Control { status: Box::new(status), ownership })
            }
            Call::Result { request_id } => {
                use frontcap_core::journal::RequestState;
                use frontcap_recorder_protocol::RequestResult;
                let running = self.running.lock().unwrap();
                let journal = self.journal.lock().unwrap();
                let result = match journal.state(&request_id) {
                    None if running.as_ref() == Some(&request_id) => RequestResult::Pending { dispatched: false },
                    None => RequestResult::Unknown,
                    Some(RequestState::Intent) if running.as_ref() == Some(&request_id) || journal.parent(&request_id).is_some_and(|parent| running.as_ref() == Some(parent)) => RequestResult::Pending { dispatched: false },
                    Some(RequestState::Intent) => RequestResult::NotApplied,
                    Some(RequestState::Dispatched) => RequestResult::Pending { dispatched: true },
                    Some(RequestState::NotApplied) => RequestResult::NotApplied,
                    Some(RequestState::Completed(value)) => match serde_json::from_value(value.clone()) {
                        Ok(answer) => RequestResult::Completed { answer: Box::new(answer) },
                        Err(error) => return err(ErrorCode::RecorderFault, format!("invalid recorded answer for {request_id}: {error}")),
                    },
                };
                Answer::Ok(Reply::Result { request_id, result })
            }
            Call::Reattach { .. } => {
                let token = {
                    let mut lease = self.lease.lock().unwrap();
                    if self.is_stopped() { return err(ErrorCode::Stopped, "the generation is stopped"); }
                    if self.cleanup_pending.load(std::sync::atomic::Ordering::SeqCst) {
                        return err(ErrorCode::Busy, "held input cleanup is pending");
                    }
                    if lease.is_some() {
                        return err(ErrorCode::LeaseHeld, "another controller holds the lease");
                    }
                    let token = LeaseToken::generate();
                    *lease = Some((token.clone(), conn));
                    token
                };
                self.controller(&[ControllerChange::Attached, ControllerChange::LeaseAcquired]);
                let completed = std::mem::take(&mut *self.detached_completions.lock().unwrap());
                Answer::Ok(Reply::Attached { lease: token, status: Box::new(self.snapshot()), completed_while_detached: completed })
            }
            Call::Release { lease, .. } => {
                {
                    let mut held = self.lease.lock().unwrap();
                    if !matches!(&*held, Some((t, _)) if *t == lease) {
                        return err(ErrorCode::LeaseRequired, "the call does not present the live lease");
                    }
                    self.cleanup_pending.store(true, std::sync::atomic::Ordering::SeqCst);
                    *held = None;
                }
                self.controller(&[ControllerChange::LeaseReleased]);
                if let Err(error) = self.cleanup_inputs().await { return err(ErrorCode::RecorderFault, error); }
                Answer::Ok(Reply::Released)
            }
            Call::Mutate { lease, request_id, op } => self.mutate(lease, request_id, op).await,
            Call::Cancel { lease, request_id, at_us } => self.cancel(lease, request_id, at_us),
            Call::AnswerDialog { lease, request_id, response } => {
                let rec = self.clone();
                tokio::spawn(async move { rec.answer_dialog(lease, request_id, response).await }).await
                    .unwrap_or_else(|error| err(ErrorCode::Internal, format!("dialog answer task failed: {error}")))
            }
            Call::Effects { after_seq, before_seq } => {
                let holds = |rule: &str| self.spec.rules.rules.iter().any(|r| r.id.as_str() == rule && matches!(r.action, frontcap_core::rules::Action::Hold));
                match self.read_trace(|events| crate::observe::effects_with(events, after_seq, before_seq, holds)) {
                    Ok(effects) => Answer::Ok(Reply::Effects { effects: Box::new(effects) }),
                    Err(e) => err(ErrorCode::Internal, e.to_string()),
                }
            }
            Call::Observe { op, context } => self.observe(op, context).await,
            Call::BindRefs { lease, refs } => self.bind_refs(lease, refs).await,
            Call::RetireBoundRefs { lease } => self.retire_bound_refs(lease).await,
            Call::SavePrefix { dest } => self.save_prefix(dest).await,
            Call::Image { name } => self.image(&name).await,
            Call::WatchHistory { name } => match self.read_trace(|events| crate::observe::history(events, &name)) {
                Ok(samples) => Answer::Ok(Reply::History { samples }),
                Err(e) => err(ErrorCode::Internal, e.to_string()),
            },
        }
    }

    /// A process image at a frozen boundary (`crate::image`); `busy` while a request runs. Like a saved
    /// prefix it is no request: nothing is journaled, and the generation's boundary stays current.
    async fn image(&self, name: &str) -> Answer {
        if let Some(r) = self.running.lock().unwrap().clone() {
            return err(ErrorCode::Busy, format!("request {r} is running"));
        }
        let Ok(mut guard) = self.generation.try_lock() else {
            return err(ErrorCode::Busy, "a request is running");
        };
        if guard.is_none() {
            return err(ErrorCode::Stopped, "the generation is stopped");
        }
        crate::image::image(self, &mut guard, name).await
    }

    /// Runs a composed probe (`Op::Probe`): its steps in order, each as a separate call would run it,
    /// with nothing else changing the generation between them. The probe's own answer collects theirs.
    async fn probe(self: &Arc<Self>, lease: Option<LeaseToken>, id: RequestId, steps: Vec<frontcap_recorder_protocol::ProbeStep>) -> Answer {
        use frontcap_recorder_protocol::{Outcome, ProbeStep, MAX_PROBE_STEPS};
        if steps.is_empty() || steps.len() > MAX_PROBE_STEPS {
            return err(ErrorCode::InvalidArguments, format!("a probe has 1 to {MAX_PROBE_STEPS} steps"));
        }
        if steps.iter().any(|s| matches!(s, ProbeStep::Mutate { op: Op::Probe { .. } | Op::Stop })) {
            return err(ErrorCode::InvalidArguments, "a probe's steps are neither probes nor stop");
        }
        if steps.iter().enumerate().any(|(index, step)| matches!(step, ProbeStep::WaitFrom { step_index, .. } if *step_index >= index)) {
            return err(ErrorCode::InvalidArguments, "a relative wait names an earlier step's zero-based index");
        }
        if !matches!((&*self.lease.lock().unwrap(), &lease), (Some((t, _)), Some(l)) if t == l) {
            return err(ErrorCode::LeaseRequired, "state-changing calls present the live lease");
        }
        let op = Op::Probe { steps: steps.clone() };
        let args = frontcap_core::journal::args_sha256(&serde_json::to_value(&op).expect("ops serialize"));
        let begun = self.journal.lock().unwrap().begin(&id, &args);
        match begun {
            Err(e) => return err(ErrorCode::Internal, e.to_string()),
            Ok(frontcap_core::journal::Begin::Conflict) => return err(ErrorCode::RequestIdConflict, format!("{id} was used with other arguments")),
            Ok(frontcap_core::journal::Begin::Completed(v)) => return serde_json::from_value(v).unwrap_or_else(|e| err(ErrorCode::Internal, e.to_string())),
            Ok(frontcap_core::journal::Begin::NotApplied) => return crate::execute::done(&id, Outcome::NotApplied),
            Ok(frontcap_core::journal::Begin::Unresolved) => return err(ErrorCode::RecorderFault, format!("{id} was interrupted and is not resolved")),
            Ok(frontcap_core::journal::Begin::New) => {}
        }
        let step_ids = probe_steps(&id, steps.len());
        {
            let mut probing = self.probing.lock().unwrap();
            if probing.is_some() || self.running.lock().unwrap().is_some() {
                return err(ErrorCode::Busy, "a request is running");
            }
            // A cancellation of an earlier request does not reach this one.
            self.cancel_flag().store(false, std::sync::atomic::Ordering::SeqCst);
            *probing = Some((id.clone(), step_ids.clone()));
        }
        struct Probing(Arc<Recorder>);
        impl Drop for Probing {
            fn drop(&mut self) {
                *self.0.probing.lock().unwrap() = None;
            }
        }
        let _probing = Probing(self.clone());
        if let Err(e) = self.update_control(|c| { c.pending_ops.insert(id.clone(), op.clone()); }) {
            return err(ErrorCode::Internal, e.to_string());
        }
        if let Err(e) = self.dispatched(&id) {
            return err(ErrorCode::Internal, e.to_string());
        }
        let mut results = Vec::new();
        let mut step_ends = Vec::<frontcap_core::time::VirtualTime>::new();
        let mut cancelled = false;
        for (step, step_id) in steps.into_iter().zip(step_ids.iter().cloned()) {
            // A cancellation seen between steps ends the probe with the steps before it.
            if self.cancel_flag().load(std::sync::atomic::Ordering::SeqCst) {
                cancelled = true;
                break;
            }
            let answer = match step {
                ProbeStep::WaitFrom { step_index, after_us } => {
                    let target = step_ends[step_index].0.checked_add(after_us);
                    let now = self.generation.lock().await.as_ref().map(|g| g.now());
                    match (target, now) {
                        (Some(target), Some(now)) => Box::pin(self.mutate(lease.clone(), step_id.clone(),
                            Op::Advance { by_us: target.saturating_sub(now.0) })).await,
                        (None, _) => err(ErrorCode::InvalidArguments, "relative wait exceeds the virtual clock range"),
                        (_, None) => err(ErrorCode::Stopped, "the generation is stopped"),
                    }
                }
                ProbeStep::Mutate { op } => Box::pin(self.mutate(lease.clone(), step_id.clone(), op)).await,
                ProbeStep::Apply { changes, context, progress_us } => match self.boundary_in(context.clone()).await {
                    Ok(boundary) => Box::pin(self.mutate(lease.clone(), step_id.clone(), Op::StateApply { boundary, changes, context, progress_us })).await,
                    Err(e) => *e,
                },
                ProbeStep::Observe { op, context } => self.observe(op, context).await,
                ProbeStep::Input { op } => match self.held_input(op).await {
                    Ok(action) => Box::pin(self.mutate(lease.clone(), step_id.clone(), Op::Act { action, then: None, context: None })).await,
                    Err(e) => *e,
                },
            };
            // A read is no journaled request: its answer is kept until the probe completes, so a probe a
            // fault interrupts completes with it.
            let journaled = matches!(self.journal.lock().unwrap().state(&step_id), Some(frontcap_core::journal::RequestState::Completed(_)));
            if !journaled && let Err(e) = self.update_control(|c| { c.probe_reads.insert(step_id.clone(), answer.clone()); }) {
                return err(ErrorCode::Internal, e.to_string());
            }
            crate::fault::point(id.as_str(), &format!("after_step_{}", results.len()));
            let completed = step_completed(&answer);
            results.push(answer);
            if !completed {
                break;
            }
            // Only completed steps can be anchors. The resulting advance is a normal journaled
            // clock request: recovery uses its persisted goal, not a newly computed offset.
            if let Some(g) = self.generation.lock().await.as_ref() {
                step_ends.push(g.now());
            } else {
                break;
            }
        }
        let completed = results.iter().filter(|a| step_completed(a)).count();
        let free = self.running.lock().unwrap().is_none();
        let end = match self.generation.try_lock() {
            Ok(mut guard) if free => match guard.as_mut() {
                Some(g) => g.observation_boundary().await.ok(),
                None => None,
            },
            _ => None,
        };
        let answer = crate::execute::done(&id, Outcome::Probed { results, completed, end, cancelled, interrupted: false });
        if let Err(e) = self.complete(&id, &answer).and_then(|()| self.forget_probe_reads(&step_ids)) {
            return err(ErrorCode::Internal, e.to_string());
        }
        answer
    }

    /// A probe a recorder fault interrupted, completed from its steps' answers in order: each journaled
    /// one as its request completed or was recovered, each read as the probe kept it. No step runs again.
    pub(crate) async fn recovered_probe(&self, g: &mut Generation, id: &RequestId, steps: usize) -> Result<Answer, crate::RecorderError> {
        use frontcap_core::journal::RequestState;
        use frontcap_recorder_protocol::Outcome;
        let step_ids = probe_steps(id, steps);
        let mut results = Vec::new();
        for step_id in &step_ids {
            let journaled = match self.journal.lock().unwrap().state(step_id) {
                Some(RequestState::Completed(v)) => Some(serde_json::from_value(v.clone()).map_err(|e| crate::RecorderError::Missing(format!("{step_id}'s answer: {e}")))?),
                // An intent never dispatched did not reach the page, as the journal answers a repeat of it.
                Some(RequestState::Intent | RequestState::NotApplied) => Some(crate::execute::done(step_id, Outcome::NotApplied)),
                _ => None,
            };
            let Some(answer) = journaled.or_else(|| self.control.lock().unwrap().probe_reads.get(step_id).cloned()) else { break };
            let completed = step_completed(&answer);
            results.push(answer);
            if !completed {
                break;
            }
        }
        let completed = results.iter().filter(|a| step_completed(a)).count();
        let end = g.observation_boundary().await.ok();
        let answer = crate::execute::done(id, Outcome::Probed { results, completed, end, cancelled: false, interrupted: true });
        Ok(answer)
    }

    pub(crate) fn forget_probe_reads(&self, step_ids: &[RequestId]) -> Result<(), crate::RecorderError> {
        Ok(self.update_control(|c| {
            for step_id in step_ids {
                c.probe_reads.remove(step_id);
            }
        })?)
    }

    /// The observation boundary the generation stands at, in `context` (the launch page by default).
    /// A probe's pointer input as the action a separate `input` call would send now: on the held pointer,
    /// at the current boundary.
    async fn held_input(&self, op: frontcap_recorder_protocol::ProbeInput) -> Result<frontcap_core::action::Action, Box<Answer>> {
        let mut guard = self.generation.lock().await;
        let Some(g) = guard.as_mut() else { return Err(Box::new(err(ErrorCode::Stopped, "the generation is stopped"))) };
        let boundary = g.observation_boundary().await.map_err(|e| Box::new(crate::execute::failed(e)))?;
        op.action(g.held_pointer().map(|p| p.handle), boundary)
            .ok_or_else(|| Box::new(err(ErrorCode::InvalidArguments, "no pointer is held: begin one first")))
    }

    async fn boundary_in(&self, context: Option<String>) -> Result<frontcap_core::observation::ObservationBoundary, Box<Answer>> {
        let mut guard = self.generation.lock().await;
        let Some(g) = guard.as_mut() else { return Err(Box::new(err(ErrorCode::Stopped, "the generation is stopped"))) };
        if let Some(context) = &context {
            g.enter_context(context).map_err(|e| Box::new(crate::execute::failed(e)))?;
        }
        let boundary = g.observation_boundary().await.map_err(|e| Box::new(crate::execute::failed(e)));
        if context.is_some() {
            g.leave_context();
        }
        boundary
    }

    /// The evidence so far, copied and finalized at a frozen boundary; `busy` while a request runs, so
    /// the prefix ends with whole commands.
    async fn save_prefix(&self, dest: std::path::PathBuf) -> Answer {
        if let Some(r) = self.running.lock().unwrap().clone() {
            return err(ErrorCode::Busy, format!("request {r} is running"));
        }
        let Ok(mut guard) = self.generation.try_lock() else {
            return err(ErrorCode::Busy, "a request is running");
        };
        let Some(g) = guard.as_mut() else {
            return err(ErrorCode::Stopped, "the generation is stopped");
        };
        let missing = {
            let c = self.control.lock().unwrap();
            let mut missing = c.missing_inputs.clone();
            missing.extend(c.pending_inputs.keys().map(|id| format!("inputs of {id}")));
            if !c.faults.is_empty() {
                missing.push("recorder_fault".into());
            }
            missing
        };
        if self.commands.lock().unwrap().is_none() {
            return err(ErrorCode::Internal, "the command log failed earlier; the run cannot be re-executed");
        }
        let manifest = match g.copy_prefix(&dest, missing) {
            Ok(m) => m,
            Err(e) => return crate::execute::failed(e),
        };
        let root = dest.join(manifest.run.as_str());
        let (bytes, commands) = match (std::fs::read(root.join("manifest.json")), frontcap_core::inputs::read_commands(&root)) {
            (Ok(bytes), Ok(commands)) => (bytes, commands.len()),
            (Err(e), _) => return err(ErrorCode::Internal, e.to_string()),
            (_, Err(e)) => return err(ErrorCode::Internal, e.to_string()),
        };
        Answer::Ok(Reply::Prefix { run: manifest.run, manifest_sha256: frontcap_core::canonical::sha256_bytes_hex(&bytes), commands })
    }

    /// Observation of the frozen page, recorded in the acquisition log; `busy` while a request runs,
    /// since an observation is of a frozen state.
    async fn observe(&self, op: frontcap_recorder_protocol::ObserveOp, context: Option<String>) -> Answer {
        if let Some(r) = self.running.lock().unwrap().clone() {
            return err(ErrorCode::Busy, format!("request {r} is running"));
        }
        let Ok(mut guard) = self.generation.try_lock() else {
            return err(ErrorCode::Busy, "a request is running");
        };
        let Some(g) = guard.as_mut() else {
            return err(ErrorCode::Stopped, "the generation is stopped");
        };
        // The context the observation reads: the one named, or the one its references name.
        let mut addressed = context.clone();
        for id in observed_refs(&op) {
            let Some(of_ref) = g.context_of_ref(&id) else { continue };
            if context.as_ref().is_some_and(|c| *c != of_ref) {
                return err(ErrorCode::InvalidArguments, format!("{id} names a node in context {of_ref}, not {}", context.as_deref().unwrap_or_default()));
            }
            addressed = Some(of_ref);
        }
        if let Some(c) = &addressed
            && let Err(e) = g.enter_context(c)
        {
            return crate::execute::failed(e);
        }
        let answer = self.observe_in(g, op).await;
        g.leave_context();
        answer
    }

    async fn observe_in(&self, g: &mut Generation, op: frontcap_recorder_protocol::ObserveOp) -> Answer {
        use frontcap_chromium::generation::Prop;
        use frontcap_recorder_protocol::{ObserveOp, Observation};
        let call = serde_json::to_string(&op).unwrap_or_default();
        let (result, summary) = match op {
            ObserveOp::Contexts {} => match g.contexts().await {
                Ok(contexts) => (Observation::Contexts { contexts }, "listed contexts".into()),
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::NetworkRules => match g.network_rules().await {
                Ok(rules) => (Observation::NetworkRules(rules), "read effective network rules".into()),
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::Predicate { predicate } => match g.predicate_now(&predicate).await {
                Ok(value) => (Observation::Sample { value }, "read a predicate".into()),
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::RecipeCapture => {
                let (live, rules) = match (g.live_state().await, g.network_rules().await) {
                    (Ok(live), Ok(rules)) => (live, rules),
                    (Err(e), _) | (_, Err(e)) => return crate::execute::failed(e),
                };
                // The caller's rules in force; the launch defaults follow from the passthrough origins.
                let caller = frontcap_core::rules::RuleSet { rules: rules.rules.rules.into_iter().filter(|r| !r.id.as_str().starts_with("default-")).collect() };
                let capture = frontcap_core::basis::RecipeCapture {
                    live, run: self.spec.run_id.clone(), profile: self.spec.profile.clone(), rules: caller,
                    passthrough_origins: self.spec.passthrough_origins.clone(), dialog_rules: g.dialog_rules(),
                };
                (Observation::RecipeCapture(Box::new(capture)), "read the live state for a recipe".into())
            }
            ObserveOp::InputState => match g.pointer_state().await {
                Ok(state) => (Observation::Input(Box::new(state)), "read held input".into()),
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::StorageScope { target } => match g.storage_scope(&target).await {
                Ok(scope) => (Observation::StorageScope(scope), "read storage scope".into()),
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::StateRead { target, field } => match g.state_read(&target, &field).await {
                Ok(value) => (Observation::State(Box::new(value)), "read document state".into()),
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::Crop { image_id, rect } => match g.crop_image(&image_id, rect) {
                Ok(image) => {
                    let summary = format!("crop {image_id}: {rect:?}");
                    (Observation::Image(image), summary)
                }
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::LocateImage { image_id, point } => match g.locate_image(&image_id, point).await {
                Ok(located) => {
                    let summary = format!("locate image {image_id}: {}", located.target.r#ref);
                    (Observation::Located(Box::new(located)), summary)
                }
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::Locate { boundary, point } => match g.locate(&boundary, point).await {
                Ok(located) => {
                    let summary = format!("locate {:?}: {}", located.point, located.target.r#ref);
                    (Observation::Located(Box::new(located)), summary)
                }
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::Inspect { target, props } => {
                let parsed: Option<Vec<Prop>> = props.iter().map(|p| Prop::parse(p)).collect();
                let Some(parsed) = parsed else {
                    return err(ErrorCode::InvalidArguments, "unknown inspection property");
                };
                match g.inspect(&target, &parsed).await {
                    Ok(i) => {
                        let summary = format!("inspect {} at revision {}, click ready: {}", i.r#ref, i.boundary.state_revision, i.click.ready);
                        (Observation::Inspection(i), summary)
                    }
                    Err(e) => return crate::execute::failed(e),
                }
            }
            ObserveOp::Scene { scope, depth, max_nodes, image, unfold } => match g.scene(scope.as_ref(), depth, max_nodes.unwrap_or(400), image, unfold).await {
                Ok(s) => {
                    let summary = format!("scene revision {}: {} nodes, presentation {:?}", s.boundary.state_revision, s.snapshot.nodes.len(), s.presentation.relation);
                    (Observation::Scene(Box::new(s)), summary)
                }
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::Snapshot { scope, depth, max_nodes, geometry, unfold } => match g.snapshot_with_geometry(scope.as_ref(), depth, max_nodes.unwrap_or(400), geometry, unfold).await {
                Ok(mut s) => {
                    s.presentation = match g.unpresented().await { Ok(p) => p, Err(e) => return crate::execute::failed(e) };
                    let summary = format!("snapshot {} nodes ({} omitted), observation {}", s.nodes.len(), s.omitted, s.observation_id);
                    (Observation::Snapshot(s), summary)
                }
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::Diff { since, max_nodes, unfold } => match g.diff(since, max_nodes.unwrap_or(400), unfold).await {
                Ok(d) => {
                    let summary = format!("diff since {}: {} added, {} removed, {} changed, observation {}", d.since, d.added.len(), d.removed.len(), d.changed.len(), d.observation_id);
                    (Observation::Diff(d), summary)
                }
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::Count { target } => match g.count(&target).await {
                Ok(count) => (Observation::Count { count }, format!("count {count}")),
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::Query { target, props } => {
                let mut parsed = Vec::new();
                for p in &props {
                    match Prop::parse(p) {
                        Some(p) => parsed.push(p),
                        None => return err(ErrorCode::InvalidArguments, format!("no property {p:?}; use text, value, visible, bbox, interaction, scroll, selection, attr:<name>, style:<property>")),
                    }
                }
                match g.query(&target, &parsed).await {
                    Ok(mut q) => {
                        q.presentation = match g.unpresented().await { Ok(p) => p, Err(e) => return crate::execute::failed(e) };
                        let summary = format!("query {} of {}", props.join(","), q.r#ref);
                        (Observation::Query(q), summary)
                    }
                    Err(e) => return crate::execute::failed(e),
                }
            }
            ObserveOp::ScreenshotTarget { target } => match g.screenshot_target(&target).await {
                Ok(image) => (Observation::Image(image), "screenshot of checked element".into()),
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::Screenshot {} => match g.screenshot().await {
                Ok(c) => {
                    let summary = format!("screenshot of frame {} at {} us", c.frame, c.t_us.0);
                    (Observation::Screenshot(c), summary)
                }
                Err(e) => return crate::execute::failed(e),
            },
            ObserveOp::Console { filter } => match self.read_trace(|events| crate::observe::console(events, &filter)) {
                Ok(entries) => {
                    let summary = format!("console {} entries", entries.len());
                    (Observation::Console { entries }, summary)
                }
                Err(e) => return err(ErrorCode::Internal, e.to_string()),
            },
            ObserveOp::Network { filter } => {
                let held = g.held().into_iter().collect();
                match self.read_trace(|events| crate::observe::network(events, &filter, &held)) {
                    Ok(entries) => {
                        let summary = format!("network {} entries", entries.len());
                        (Observation::Network { entries }, summary)
                    }
                    Err(e) => return err(ErrorCode::Internal, e.to_string()),
                }
            }
        };
        if let Err(e) = g.record_observation(&call, summary) {
            return err(ErrorCode::Internal, e.to_string());
        }
        let state = g.observation_state();
        if let Err(e) = self.update_observations(state) {
            return err(ErrorCode::Internal, e.to_string());
        }
        Answer::Ok(Reply::Observed { result })
    }

    /// Reads the trace so far; complete lines only, so a concurrent append is never half read. Only
    /// lines appended since the last read are parsed.
    fn read_trace<R>(&self, read: impl FnOnce(&[frontcap_core::event::BehaviorEvent]) -> R) -> Result<R, frontcap_core::log::LogError> {
        let path = self.status.read().unwrap().bundle.join("trace.jsonl");
        let tail = {
            let mut tail = self.trace_tail.lock().unwrap();
            match &*tail {
                Some((of, tail)) if *of == path => tail.clone(),
                _ => tail.insert((path.clone(), frontcap_core::log::BehaviorTail::shared(&path))).1.clone(),
            }
        };
        let mut tail = tail.lock().unwrap();
        Ok(read(tail.events()?))
    }

    async fn bind_refs(&self, lease: LeaseToken, refs: std::collections::BTreeMap<String, frontcap_core::inputs::Resolution>) -> Answer {
        if !matches!(&*self.lease.lock().unwrap(), Some((t, _)) if *t == lease) {
            return err(ErrorCode::LeaseRequired, "binding references presents the live lease");
        }
        if let Some(r) = self.running.lock().unwrap().clone() {
            return err(ErrorCode::Busy, format!("request {r} is running"));
        }
        let Ok(mut guard) = self.generation.try_lock() else {
            return err(ErrorCode::Busy, "a request is running");
        };
        let Some(g) = guard.as_mut() else {
            return err(ErrorCode::Stopped, "the generation is stopped");
        };
        for (id, resolution) in &refs {
            // In the context the recorded run resolved it in.
            if let Some(context) = &resolution.context
                && let Err(e) = g.enter_context(context)
            {
                return crate::execute::failed(e);
            }
            let bound = g.bind_ref(id, resolution).await;
            g.leave_context();
            if let Err(e) = bound {
                return crate::execute::failed(e);
            }
        }
        let state = g.observation_state();
        if let Err(e) = self.update_observations(state) {
            return err(ErrorCode::Internal, e.to_string());
        }
        Answer::Ok(Reply::Bound)
    }

    async fn retire_bound_refs(&self, lease: LeaseToken) -> Answer {
        if !matches!(&*self.lease.lock().unwrap(), Some((t, _)) if *t == lease) {
            return err(ErrorCode::LeaseRequired, "retiring bound references presents the live lease");
        }
        if let Some(r) = self.running.lock().unwrap().clone() {
            return err(ErrorCode::Busy, format!("request {r} is running"));
        }
        let Ok(mut guard) = self.generation.try_lock() else {
            return err(ErrorCode::Busy, "a request is running");
        };
        let Some(g) = guard.as_mut() else {
            return err(ErrorCode::Stopped, "the generation is stopped");
        };
        g.retire_bound_refs();
        let state = g.observation_state();
        if let Err(e) = self.update_observations(state) {
            return err(ErrorCode::Internal, e.to_string());
        }
        Answer::Ok(Reply::Bound)
    }

    async fn mutate(self: &Arc<Self>, lease: Option<LeaseToken>, request_id: RequestId, op: Op) -> Answer {
        if self.is_stopped() {
            return err(ErrorCode::Stopped, "the generation is stopped");
        }
        if let Op::Probe { steps } = op {
            return self.probe(lease, request_id, steps).await;
        }
        let probe_step = match &*self.probing.lock().unwrap() {
            Some((_, steps)) if !steps.contains(&request_id) => return err(ErrorCode::Busy, "a probe is running"),
            probing => probing.is_some(),
        };
        // An answer to the held request the running operation's page waits for is that operation's
        // dependent, not a request of its own.
        let answered_parent = match &op {
            Op::Respond { ordinal, .. } | Op::FailHeld { ordinal, .. } => {
                let parent = self.running.lock().unwrap().clone();
                parent.filter(|p| self.holds().pending().is_some_and(|h| &h.ordinal == ordinal)
                    || self.journal.lock().unwrap().dependents(p).contains(&request_id))
            }
            _ => None,
        };
        if let Some(parent) = answered_parent {
            return self.answer_blocking_hold(lease, request_id, parent, op).await;
        }
        if self.cleanup_pending.load(std::sync::atomic::Ordering::SeqCst) {
            return err(ErrorCode::Busy, "held input cleanup is pending");
        }
        let live = {
            let held_lease = self.lease.lock().unwrap();
            if self.cleanup_pending.load(std::sync::atomic::Ordering::SeqCst) {
                return err(ErrorCode::Busy, "held input cleanup is pending");
            }
            let live = held_lease.clone();
            match (&live, &lease) {
                (Some((t, _)), Some(l)) if t == l => {}
                (None, _) if op == Op::Stop => {}
                _ => return err(ErrorCode::LeaseRequired, "state-changing calls present the live lease"),
            }
            {
                let mut running = self.running.lock().unwrap();
                if let Some(r) = &*running {
                    return err(ErrorCode::Busy, format!("request {r} is running"));
                }
                // A cancellation of an earlier request does not reach this one; a probe's steps share
                // the probe's.
                if !probe_step {
                    self.cancel_flag().store(false, std::sync::atomic::Ordering::SeqCst);
                }
                *running = Some(request_id.clone());
                self.pointer_admission.store(matches!(&op, Op::Act { action, .. } if action.is_held_input()), std::sync::atomic::Ordering::SeqCst);
            }
            live
        };
        // The request runs to completion in its own task even if this connection goes away.
        let rec = self.clone();
        let lease = lease.or_else(|| live.map(|(t, _)| t));
        let id = request_id.clone();
        let mut task = tokio::spawn(async move {
            // Clears `running` however the request ends, a panic included, so the generation never stays busy.
            struct Running(Arc<Recorder>);
            impl Drop for Running {
                fn drop(&mut self) {
                    let mut running = self.0.running.lock().unwrap();
                    self.0.pointer_admission.store(false, std::sync::atomic::Ordering::SeqCst);
                    *running = None;
                }
            }
            let _running = Running(rec.clone());
            let answer = execute::run(&rec, &id, op).await;
            if let Some(g) = rec.generation.lock().await.as_ref() {
                rec.refresh(g);
            }
            // Recorded as completed-while-detached before `running` clears, so a controller that sees the
            // request finished and reattaches always receives it.
            // Completed while detached: the lease the request presented is no longer the live one.
            let lease_guard = rec.lease.lock().unwrap();
            let running = rec.running.lock().unwrap();
            let live = lease_guard.as_ref().map(|(t, _)| t.clone());
            if live.is_none() || live != lease {
                rec.detached_completions.lock().unwrap().push(id);
            }
            drop(running);
            drop(lease_guard);
            answer
        });
        let suspended = async {
            let dialog = async {
                match self.dialogs() {
                    Some(dialogs) => dialogs.wait_pending().await.map(Some),
                    None => std::future::pending().await,
                }
            };
            // A held request the page waits for suspends the operation as a held dialog does.
            let holds = self.holds();
            let blocked = tokio::select! {
                d = dialog => d.map(|dialog| (dialog, None)),
                hold = holds.wait_pending() => Ok((None, Some(Box::new(crate::blocking::request_of(hold))))),
            };
            match blocked {
                Ok((dialog, request)) => {
                    let operation = frontcap_recorder_protocol::SuspendedOperation { request_id, dialog, request };
                    match self.persist_suspension(&operation) {
                        Ok(true) => Answer::Ok(Reply::Suspended { operation }),
                        Ok(false) => std::future::pending::<Answer>().await,
                        Err(error) => err(ErrorCode::Internal, error.to_string()),
                    }
                }
                // Closing the generation also closes its dialog channel. The operation's own result
                // remains authoritative (including a successful stop or its recorded failure).
                Err(_) => std::future::pending::<Answer>().await,
            }
        };
        tokio::select! {
            biased;
            result = &mut task => result.unwrap_or_else(|e| err(ErrorCode::Internal, format!("request task failed: {e}"))),
            answer = suspended => answer,
        }
    }
}

/// A probe's step request ids, derived from its own (`probe-<n>`).
pub(crate) fn probe_steps(id: &RequestId, steps: usize) -> Vec<RequestId> {
    (0..steps).map(|n| id.derived(&format!("probe-{n}"))).collect()
}

/// Whether a probe step completed, so the next one may run.
fn step_completed(answer: &Answer) -> bool {
    matches!(answer, Answer::Ok(Reply::Done { outcome, .. }) if *outcome != frontcap_recorder_protocol::Outcome::NotApplied)
        || matches!(answer, Answer::Ok(Reply::Observed { .. }))
}

pub async fn serve(rec: Arc<Recorder>, listener: UnixListener) -> std::io::Result<()> {
    let mut next_conn = 0u64;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                next_conn += 1;
                tokio::spawn(connection(rec.clone(), stream, next_conn));
            }
            _ = rec.shutdown.notified() => return Ok(()),
        }
    }
}

async fn connection(rec: Arc<Recorder>, stream: UnixStream, conn: u64) {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let mut open = true;
    while open {
        let frame: ClientFrame = match read_frame(&mut r).await {
            Ok(Some(f)) => f,
            _ => break,
        };
        let handling = rec.handle(conn, frame.call);
        tokio::pin!(handling);
        // A controller that disconnects while its request runs loses its lease at once. The call is polled
        // first: its first poll checks the lease and starts the request, so a request that reached the
        // recorder runs even when the connection closes right after it.
        let answer = tokio::select! {
            biased;
            a = &mut handling => a,
            _ = r.fill_buf() => {
                if r.buffer().is_empty() {
                    open = false;
                    rec.connection_closed(conn);
                }
                handling.await
            }
        };
        if open && write_frame(&mut w, &ServerFrame { id: frame.id, answer }).await.is_err() {
            open = false;
        }
        if rec.is_stopped() {
            rec.shutdown.notify_one();
            break;
        }
    }
    rec.connection_closed(conn);
}

/// The references an observation names.
fn observed_refs(op: &frontcap_recorder_protocol::ObserveOp) -> Vec<String> {
    use frontcap_recorder_protocol::ObserveOp;
    let locators: Vec<&frontcap_core::predicate::Locator> = match op {
        ObserveOp::ScreenshotTarget { target } | ObserveOp::StorageScope { target } | ObserveOp::StateRead { target, .. } | ObserveOp::Inspect { target, .. } | ObserveOp::Query { target, .. } | ObserveOp::Count { target } => vec![target],
        ObserveOp::Scene { scope: Some(scope), .. } | ObserveOp::Snapshot { scope: Some(scope), .. } => vec![scope],
        _ => Vec::new(),
    };
    locators.into_iter().flat_map(|l| l.refs()).map(str::to_string).collect()
}
