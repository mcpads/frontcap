//! Experimental native recorder. Its session worker owns the target; no saved PID is adopted.
pub mod launcher;
pub mod preparation;
pub mod preparation_control;
pub mod recovery;
pub mod target;
mod webview_comparison;
mod storage;
pub use storage::{declared_android_scratch_image, declared_simulator_image};
mod storage_watch;
mod session_volume;
pub use session_volume::artifact::retained_ios_png;

use crate::{
    RecorderError,
    runtime::{self, RuntimeRecord, RuntimeState},
};
use frontcap_core::{
    ids::{LaunchId, RequestId, RunId},
    journal::{Begin, Journal, RequestState, args_sha256},
    process::ProcessIdentity,
    state::{load, save_atomic},
    write_budget::{Class, WriteBudget},
};
use frontcap_recorder_protocol::{
    Answer, Call, ClientFrame, ErrorCode, ErrorReply, LeaseToken, Reply, RequestResult,
    ServerFrame,
    frame::{read_frame, write_frame},
    native::{Capability, Op, Outcome, Reply as NativeReply, Status},
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use target::Session;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::{UnixListener, UnixStream},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceMode {
    ControlledHttp,
    ControlledHttpAt(frontcap_core::native::service::RequestEndpoint),
    ControlledAssets {
        assets: Vec<frontcap_service_fixture::assets::AssetInput>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        endpoint: Option<frontcap_core::native::service::RequestEndpoint>,
    },
}
impl ServiceMode {
    fn validate(&self) -> io::Result<()> {
        if let Self::ControlledHttpAt(endpoint) = self { endpoint.validate().map_err(io::Error::other)?; }
        if let Self::ControlledAssets { endpoint: Some(endpoint), .. } = self { endpoint.validate().map_err(io::Error::other)?; }
        if let Self::ControlledAssets { assets, .. } = self
            && (assets.is_empty() || assets.iter().any(|asset| !asset.source.is_absolute())) {
            return Err(io::Error::other("controlled assets require nonempty mappings and absolute source paths"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchSpec {
    pub request_id: RequestId,
    pub launch_id: LaunchId,
    pub run_id: RunId,
    pub target: target::Launch,
    pub session_executable: PathBuf,
    pub acquisition_budget_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_free_bytes: Option<u64>,
    /// Shared cooperating-writer allowance; currently enforced by recorder and controlled-service payload writers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write_budget: Option<frontcap_core::write_budget::Limits>,
    /// Optional existing parent for per-launch target evidence; not cleanup authority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_root: Option<PathBuf>,
    /// Automatically owned and retained UIKit session filesystem; conflicts with session_root.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_volume: Option<frontcap_native::storage_volume::Config>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<ServiceMode>,
}

fn validate_write_budget(limits: Option<frontcap_core::write_budget::Limits>) -> io::Result<()> {
    if let Some(limits) = limits
        && (limits.ordinary == 0 || limits.retirement == 0) {
        return Err(io::Error::other("write allowances must both be positive"));
    }
    Ok(())
}

/// Resolve original target evidence without starting or adopting a target.
/// External roots must still match their pre-acquisition registration.
pub fn session_directory(runtime: &Path) -> io::Result<PathBuf> {
    storage::session_directory(runtime)
}

/// Retained diagnostics never confer request completion or resource ownership.
#[derive(Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum WorkerTerminalDiagnostics {
    Recorded { inspection: frontcap_native::worker_control::Inspection },
    Unavailable { error: String },
}
pub fn worker_terminal_diagnostics(runtime: &Path) -> Option<WorkerTerminalDiagnostics> {
    match frontcap_native::worker_control::inspect_root(runtime) {
        Ok(Some(inspection)) => Some(WorkerTerminalDiagnostics::Recorded { inspection }),
        Ok(None) => None,
        Err(error) => Some(WorkerTerminalDiagnostics::Unavailable { error: error.to_string() }),
    }
}

pub fn spec_path(dir: &Path) -> PathBuf {
    dir.join("native-spec.json")
}

struct State {
    journal: Journal,
    status: Status,
    lease: Option<(LeaseToken, u64)>,
    detached: Vec<RequestId>,
    retiring: bool,
}
struct Recorder {
    dir: PathBuf,
    storage_roots: storage::Roots,
    min_free_bytes: u64,
    write_budget: Option<WriteBudget>,
    state: Mutex<State>,
    session: Mutex<Option<Session>>,
    session_volume: Mutex<Option<session_volume::SessionVolume>>,
    service: Option<frontcap_service_fixture::owned::Controller>,
    shutdown: tokio::sync::Notify,
}
fn error(code: ErrorCode, detail: impl Into<String>) -> Answer {
    Answer::Err(ErrorReply::new(code, detail))
}
fn reply(reply: NativeReply) -> Answer {
    Answer::Ok(Reply::Native { body: reply })
}
fn result(state: Option<&RequestState>) -> Result<RequestResult, serde_json::Error> {
    Ok(match state {
        None => RequestResult::Unknown,
        Some(RequestState::Intent) => RequestResult::Pending { dispatched: false },
        Some(RequestState::Dispatched) => RequestResult::Pending { dispatched: true },
        Some(RequestState::NotApplied) => RequestResult::NotApplied,
        Some(RequestState::Completed(value)) => RequestResult::Completed {
            answer: Box::new(serde_json::from_value(value.clone())?),
        },
    })
}
fn save_budgeted<T: Serialize>(budget: Option<&WriteBudget>, class: Class, path: &Path, value: &T) -> io::Result<()> {
    match budget {
        Some(budget) => budget.save_atomic(class, path, value),
        None => save_atomic(path, value),
    }
}
impl Recorder {
    fn charge_admission(&self, id: &RequestId, hash: &str, op: &Op, budget_ms: u64) -> io::Result<()> {
        if let Some(budget) = &self.write_budget {
            budget.charge(if matches!(op, Op::Stop) { Class::Retirement } else { Class::Ordinary },
                Journal::admission_bytes(id, hash) + recovery::record_bytes(op, budget_ms))?;
        }
        Ok(())
    }
    fn save<T: Serialize>(&self, class: Class, path: &Path, value: &T) -> io::Result<()> {
        save_budgeted(self.write_budget.as_ref(), class, path, value)
    }

    fn snapshot(state: &State) -> Status {
        let mut status = state.status.clone();
        status.lease_held = state.lease.is_some();
        status.last_completed_request_id = state.journal.last_completed().cloned();
        status
    }
    fn closed(&self, conn: u64) {
        let mut state = self.state.lock().unwrap();
        if state
            .lease
            .as_ref()
            .is_some_and(|(_, owner)| *owner == conn)
        {
            state.lease = None;
        }
    }
    fn fail(&self, detail: String) {
        let mut state = self.state.lock().unwrap();
        state.status.failure.get_or_insert(detail);
        // Failure to publish does not un-fence this live owner. The durable request stays pending.
        let _ = self.save(Class::Retirement,
            &self.dir.join("native-status.json"),
            &Self::snapshot(&state),
        );
    }
    async fn handle(self: &Arc<Self>, conn: u64, call: Call) -> Answer {
        match call {
            Call::Hello => Answer::Ok(Reply::Hello {
                launch_id: self.state.lock().unwrap().status.launch_id.clone(),
            }),
            Call::Status => reply(NativeReply::Status {
                status: Box::new(Self::snapshot(&self.state.lock().unwrap())),
            }),
            Call::Result { request_id } => {
                match result(self.state.lock().unwrap().journal.state(&request_id)) {
                    Ok(result) => Answer::Ok(Reply::Result { request_id, result }),
                    Err(e) => error(ErrorCode::RecorderFault, e.to_string()),
                }
            }
            Call::Reattach { .. } => {
                let mut state = self.state.lock().unwrap();
                if state.status.stopped {
                    return error(ErrorCode::Stopped, "native recorder has stopped");
                }
                if state.lease.is_some() {
                    return error(
                        ErrorCode::LeaseHeld,
                        "native recorder already has a controller",
                    );
                }
                let lease = LeaseToken::generate();
                state.lease = Some((lease.clone(), conn));
                reply(NativeReply::Attached {
                    lease,
                    status: Box::new(Self::snapshot(&state)),
                    completed_while_detached: std::mem::take(&mut state.detached),
                })
            }
            Call::Release { lease, .. } => {
                let mut state = self.state.lock().unwrap();
                if !state.lease.as_ref().is_some_and(|(held, _)| held == &lease) {
                    return error(
                        ErrorCode::LeaseRequired,
                        "native controller lease does not match",
                    );
                }
                state.lease = None;
                Answer::Ok(Reply::Released)
            }
            Call::Native {
                lease,
                request_id,
                op,
                budget_ms,
            } => self.mutate(lease, request_id, op, budget_ms).await,
            _ => error(
                ErrorCode::UnsupportedCapability,
                "operation is not supported by the native recorder",
            ),
        }
    }
    async fn mutate(
        self: &Arc<Self>,
        lease: LeaseToken,
        id: RequestId,
        op: Op,
        budget_ms: u64,
    ) -> Answer {
        if !(1..=60_000).contains(&budget_ms) {
            return error(
                ErrorCode::InvalidArguments,
                "native operation budget must be 1..60000 ms",
            );
        }
        let deadline = Instant::now() + Duration::from_millis(budget_ms);
        let hash = args_sha256(
            &serde_json::to_value((&op, budget_ms)).expect("native operation serializes"),
        );
        {
            let mut state = self.state.lock().unwrap();
            if !state.lease.as_ref().is_some_and(|(held, _)| held == &lease) {
                return error(
                    ErrorCode::LeaseRequired,
                    "native controller lease does not match",
                );
            }
            // Results are available even after a later operation fails. Check identity before busy/failure.
            if let Some(previous) = state.journal.argument_hash(&id) {
                if previous != hash {
                    return error(
                        ErrorCode::RequestIdConflict,
                        "native request ID has different arguments",
                    );
                }
                return match result(state.journal.state(&id)) {
                    Ok(RequestResult::Completed { answer }) => *answer,
                    Ok(result) => Answer::Ok(Reply::Result {
                        request_id: id,
                        result,
                    }),
                    Err(e) => error(ErrorCode::RecorderFault, e.to_string()),
                };
            }
            if state.status.running.is_some() {
                return error(ErrorCode::Busy, "a native operation is running");
            }
            if state.status.stopped {
                return error(ErrorCode::Stopped, "native target retired");
            }
            if !matches!(op, Op::Stop) {
                if let Some(failure) = &state.status.failure {
                    return error(ErrorCode::RecorderFault, failure.clone());
                }
                if let Err(e) = state.status.ready.validate_operation(&op) {
                    return error(ErrorCode::InvalidArguments, e.to_string());
                }
                if let Op::CompareWebviewProgress { before, after, before_capture, after_capture } = &op {
                    if let Err(e) = webview_comparison::compare_for_ready(&state.status.ready, &state.journal, before, after, before_capture.as_ref(), after_capture.as_ref()) {
                        return error(ErrorCode::InvalidArguments, e.to_string());
                    }
                } else if matches!(op, Op::InspectService | Op::ReleaseService { .. } | Op::ReleaseAsset { .. }) {
                    if matches!(op, Op::ReleaseAsset { .. }) && !state.status.capabilities.contains(&Capability::ControlledResources) {
                        return error(ErrorCode::UnsupportedCapability, "no controlled resources for this launch");
                    }
                    if self.service.is_none() {
                        return error(ErrorCode::UnsupportedCapability, "no controlled service for this launch");
                    }
                } else if let Some(session) = self.session.lock().unwrap().as_ref()
                    && let Err(e) = session.validate_operation(&id, &op) {
                        return error(ErrorCode::InvalidArguments, e.to_string());
                    }
            }
            // Existing receipts were resolved above. Refusal is before intent and dispatch;
            // retirement must remain possible even when ordinary work cannot be admitted.
            if !matches!(op, Op::Stop)
                && let Err(e) = self.storage_roots.admit(self.min_free_bytes) {
                return error(ErrorCode::RecorderFault, format!("native storage admission refused before dispatch: {e}; inspect storage or stop the target"));
            }
            if let Err(e) = self.charge_admission(&id, &hash, &op, budget_ms) {
                return error(ErrorCode::RecorderFault, format!("native write admission refused before dispatch: {e}; inspect results or stop the target"));
            }
            if matches!(op, Op::Stop) { self.storage_roots.release_control(); }
            let admitted = (|| {
                if !matches!(
                    state.journal.begin(&id, &hash).map_err(io::Error::other)?,
                    Begin::New
                ) {
                    unreachable!("new native request was checked under the journal lock");
                }
                recovery::record(&self.dir, &id, &op, budget_ms)?;
                crate::fault::point(id.as_str(), "native_before_dispatch");
                state.journal.dispatched(&id).map_err(io::Error::other)?;
                crate::fault::point(id.as_str(), "native_after_dispatch");
                Ok::<_, io::Error>(())
            })();
            if let Err(e) = admitted {
                state.status.failure = Some(format!("native journal admission: {e}"));
                return error(ErrorCode::RecorderFault, e.to_string());
            }
            state.status.running = Some(id.clone());
            state.retiring = matches!(op, Op::Stop);
        }
        // The blocking task owns completion even if the socket controller disappears.
        let rec = self.clone();
        match tokio::task::spawn_blocking(move || rec.execute(id, lease, op, deadline)).await {
            Ok(answer) => answer,
            Err(e) => {
                self.fail(format!("native request task failed: {e}"));
                error(ErrorCode::RecorderFault, e.to_string())
            }
        }
    }
    fn execute(&self, id: RequestId, lease: LeaseToken, op: Op, deadline: Instant) -> Answer {
        let stopped = matches!(op, Op::Stop);
        let outcome = self.apply(&id, op, deadline).and_then(|outcome| {
            if let Outcome::AndroidViewsRestarted {replacement} = &outcome {
                replacement.validate().map_err(io::Error::other)?;
                let state = self.state.lock().unwrap();
                if !matches!(&state.status.ready, frontcap_recorder_protocol::native::Ready::AndroidViews {owner, restart: true, ..} if owner == &replacement.previous) {
                    return Err(io::Error::other("Android replacement differs from current recorder owner"));
                }
            }
            if let Outcome::AndroidViewsStateChanged {transition} = &outcome {
                transition.validate().map_err(io::Error::other)?;
                let state = self.state.lock().unwrap();
                if !matches!(&state.status.ready, frontcap_recorder_protocol::native::Ready::AndroidViews {owner, app_lifecycle: true, ..} if owner == &transition.owner) {
                    return Err(io::Error::other("Android host transition differs from current recorder owner"));
                }
            }
            if let Outcome::UikitRestarted { replacement } = &outcome {
                replacement.validate().map_err(io::Error::other)?;
                let state = self.state.lock().unwrap();
                if !matches!(&state.status.ready, frontcap_recorder_protocol::native::Ready::Ios { owner, restart: true, .. } if owner == &replacement.previous) {
                    return Err(io::Error::other("UIKit replacement differs from current recorder owner"));
                }
            }
            if let Outcome::FlutterRestarted { replacement } = &outcome {
                replacement.validate().map_err(io::Error::other)?;
                let state = self.state.lock().unwrap();
                if !matches!(&state.status.ready, frontcap_recorder_protocol::native::Ready::Flutter { owner, restart: true, .. }
                    if owner == replacement.previous()) {
                    return Err(io::Error::other("Flutter replacement differs from current recorder owner"));
                }
            }
            if let Outcome::WebviewCompleted { completion } = &outcome
                && let frontcap_core::native::webview::Completion::Restarted { previous, owner, .. } = completion.as_ref() {
                owner.validate().map_err(io::Error::other)?;
                let state = self.state.lock().unwrap();
                let frontcap_recorder_protocol::native::Ready::Webview { owner: current, .. } = &state.status.ready else {
                    return Err(io::Error::other("replacement returned for a non-WebView target"));
                };
                if current != previous || previous.process == owner.process || previous.endpoint.generation == owner.endpoint.generation
                    || previous.simulator != owner.simulator || previous.attachment_sha256 != owner.attachment_sha256 {
                    return Err(io::Error::other("replacement owner lineage differs"));
                }
            }
            if let Outcome::AndroidHostCompleted { completion: frontcap_core::native::android_host::Completion::Restarted { replacement } } = &outcome {
                replacement.validate()?;
                let state = self.state.lock().unwrap();
                if !matches!(&state.status.ready, frontcap_recorder_protocol::native::Ready::AndroidHost { owner, contacts }
                    if owner == &replacement.previous.owner && *contacts == replacement.ready.contacts.is_some()) {
                    return Err(io::Error::other("Android host replacement differs from current recorder owner"));
                }
            }
            if let Outcome::IosHostCompleted { completion: frontcap_core::native::ios_host::Completion::Restarted { previous, ready } } = &outcome {
                ready.owner.validate().map_err(io::Error::other)?;
                let state = self.state.lock().unwrap();
                let frontcap_recorder_protocol::native::Ready::IosHost { owner, contacts, pages, system_ui, app_lifecycle, .. } = &state.status.ready else {
                    return Err(io::Error::other("host replacement returned for another target"));
                };
                if owner != previous.as_ref() || owner.process == ready.owner.process
                    || owner.simulator != ready.owner.simulator || owner.attachment_sha256 != ready.owner.attachment_sha256
                    || *app_lifecycle != ready.app_lifecycle || *contacts != ready.contacts.is_some() || *pages != ready.pages || system_ui.is_some() != ready.system_ui.is_some() {
                    return Err(io::Error::other("host replacement differs from current owner"));
                }
                if let Some(endpoint) = &ready.contacts { endpoint.validate(ready.owner.process.pid).map_err(io::Error::other)?; }
            }
            if let Outcome::IosHostCompleted {completion:frontcap_core::native::ios_host::Completion::EntryBegun {entry}}=&outcome
                && entry.begin!=id {return Err(io::Error::other("native cold entry request ID differs"));}
            if let Outcome::FlutterEntryStarted { entry } = &outcome
                && entry.request_id != id { return Err(io::Error::other("cold entry request ID differs")); }
            let ready = &self.state.lock().unwrap().status.ready;
            let next = ready.entry_transition(&outcome).map_err(io::Error::other)?
                .or(ready.visibility_transition(&outcome).map_err(io::Error::other)?);
            Ok((outcome, next))
        });
        if outcome.is_ok() {
            crate::fault::point(id.as_str(), "native_after_effect");
        }
        if let Err(e) = &outcome {
            self.fail(e.to_string());
            // Closing authority starts worker-owned retirement; never signal a saved process ID.
            if let Some(session) = self.session.lock().unwrap().take() {
                let _ = session.stop(Instant::now() + Duration::from_secs(5));
            }
        }
        let mut state = self.state.lock().unwrap();
        let answer = match outcome {
            Ok((outcome, next)) => {
                if let Some(ready) = next { state.status.ready = ready; }
                if let Outcome::AndroidViewsRestarted {replacement} = &outcome
                    && let frontcap_recorder_protocol::native::Ready::AndroidViews {owner, ..} = &mut state.status.ready {
                    *owner = replacement.owner.clone();
                }
                if let Outcome::UikitRestarted { replacement } = &outcome
                    && let frontcap_recorder_protocol::native::Ready::Ios { owner, .. } = &mut state.status.ready {
                    *owner = replacement.owner.clone();
                }
                if let Outcome::FlutterRestarted { replacement } = &outcome
                    && let frontcap_recorder_protocol::native::Ready::Flutter { owner, .. } = &mut state.status.ready {
                    *owner = replacement.owner().clone();
                }
                if let Outcome::WebviewCompleted { completion } = &outcome
                    && let frontcap_core::native::webview::Completion::Restarted { owner, .. } = completion.as_ref()
                    && let frontcap_recorder_protocol::native::Ready::Webview { owner: current, .. } = &mut state.status.ready {
                    *current = owner.clone();
                }
                if let Outcome::AndroidHostCompleted { completion: frontcap_core::native::android_host::Completion::Restarted { replacement } } = &outcome
                    && let frontcap_recorder_protocol::native::Ready::AndroidHost { owner, .. } = &mut state.status.ready {
                    *owner = replacement.ready.owner.clone();
                }
                if let Outcome::IosHostCompleted { completion: frontcap_core::native::ios_host::Completion::Restarted { ready, .. } } = &outcome
                    && let frontcap_recorder_protocol::native::Ready::IosHost { owner, system_ui, app_lifecycle, visibility, .. } = &mut state.status.ready {
                    *owner = ready.owner.clone();
                    *system_ui = ready.system_ui.clone();
                    *app_lifecycle = ready.app_lifecycle; *visibility = None;
                }
                let answer = reply(NativeReply::Done {
                    request_id: id.clone(),
                    outcome: Box::new(outcome),
                });
                let value = serde_json::to_value(&answer).expect("native answer serializes");
                let publication = (|| -> io::Result<()> {
                    if let Some(budget) = &self.write_budget {
                        budget.charge(if stopped { Class::Retirement } else { Class::Ordinary },
                            Journal::completion_bytes(&id, &value))?;
                    }
                    state.journal.complete(&id, value).map_err(io::Error::other)
                })();
                match publication {
                    Ok(()) => {
                        crate::fault::point(id.as_str(), "native_after_complete");
                        if !state.lease.as_ref().is_some_and(|(held, _)| held == &lease) {
                            state.detached.push(id.clone());
                        }
                        answer
                    }
                    Err(e) => {
                        state.status.failure = Some(format!("native completion publication: {e}"));
                        error(ErrorCode::RecorderFault, e.to_string())
                    }
                }
            }
            Err(e) => error(ErrorCode::RecorderFault, e.to_string()),
        };
        state.status.running = None;
        // An explicit stop closes this recorder even if retirement could not be confirmed.
        // Its result stays pending and the host/session fence survives.
        state.status.stopped = stopped;
        if stopped {
            state.lease = None;
        }
        if let Err(e) = self.save(if stopped { Class::Retirement } else { Class::Ordinary },
            &self.dir.join("native-status.json"),
            &Self::snapshot(&state),
        ) {
            state.status.failure = Some(format!("native status publication: {e}"));
        }
        answer
    }
    fn apply(&self, id: &RequestId, op: Op, deadline: Instant) -> io::Result<Outcome> {
        if let Op::CompareWebviewProgress { before, after, before_capture, after_capture } = &op {
            let state = self.state.lock().unwrap();
            return webview_comparison::compare_for_ready(&state.status.ready, &state.journal,
                before, after, before_capture.as_ref(), after_capture.as_ref());
        }
        if matches!(op, Op::InspectService | Op::ReleaseService { .. } | Op::ReleaseAsset { .. }) {
            let service = self.service.as_ref().ok_or_else(|| io::Error::other("service authority absent"))?;
            let owner = self.state.lock().unwrap().status.launch_id.clone();
            let command = match op {
                Op::InspectService => frontcap_service_fixture::Control::Inspect { generation: service.binding().service.clone() },
                Op::ReleaseService { generation, request, response } => frontcap_service_fixture::Control::Release { generation, id: id.clone(), request, response },
                Op::ReleaseAsset { generation, request, sha256 } => frontcap_service_fixture::Control::ReleaseAsset { generation, id: id.clone(), request, sha256 },
                _ => unreachable!(),
            };
            let reply = tokio::runtime::Handle::current().block_on(service.exchange(&owner, command));
            if let frontcap_service_fixture::Reply::Uncertain { reason } = reply {
                return Err(io::Error::other(reason));
            }
            return Ok(Outcome::Service { reply });
        }
        let mut owner = self.session.lock().unwrap();
        if matches!(op, Op::Stop) {
            let retirement = match owner.take() {
                Some(session) => session.stop(deadline)?,
                None => {
                    let ready = self.state.lock().unwrap().status.ready.clone();
                    target::inspect_stop(&session_directory(&self.dir)?, &ready, deadline)?
                },
            };
            if let Some(volume) = self.session_volume.lock().unwrap().take() {
                volume.retire(&self.dir, id, &retirement, deadline, self.write_budget.as_ref())?;
            }
            return Ok(Outcome::Stopped { retirement });
        }
        let mut outcome = owner
            .as_mut()
            .ok_or_else(|| io::Error::other("native session authority closed"))?
            .apply(id, op, deadline)?;
        let progress_service = match &mut outcome {
            Outcome::WebviewCompleted { completion } => match completion.as_mut() {
                frontcap_core::native::webview::Completion::Progress { observation } => Some(&mut observation.service),
                _ => None,
            },
            Outcome::AndroidWebviewCompleted { completion: frontcap_core::native::android_webview::Completion::Progress { observation } } => Some(&mut observation.service),
            Outcome::AndroidHostCompleted { completion: frontcap_core::native::android_host::Completion::Page { completion, .. } } => {
                match completion.as_mut() {
                    frontcap_core::native::android_webview::Completion::Progress { observation } => Some(&mut observation.service),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(progress_service) = progress_service {
            use frontcap_core::native::{service, webview_progress::Service};
            *progress_service = match &self.service {
                None => Service::Unconfigured,
                Some(controller) => {
                    let launch = self.state.lock().unwrap().status.launch_id.clone();
                    let result = tokio::runtime::Handle::current().block_on(async {
                        tokio::time::timeout_at(deadline.into(), controller.exchange(&launch,
                            service::Control::Inspect { generation: controller.binding().service.clone() })).await
                    });
                    match result {
                        Ok(service::Reply::Snapshot { record }) => Service::from_record(&record),
                        Ok(service::Reply::Uncertain { reason } | service::Reply::Rejected { reason }) => Service::Uncertain { reason },
                        Ok(_) => Service::Uncertain { reason: "unexpected service inspection reply".into() },
                        Err(_) => Service::Uncertain { reason: "service inspection deadline exceeded".into() },
                    }
                }
            };
        }
        Ok(outcome)
    }
}

/// Failure publication may reopen an existing matching allowance, never recreate one.
pub(crate) fn mark_failed(dir: &Path, reason: &str, error: Option<ErrorReply>) -> io::Result<()> {
    let spec: LaunchSpec = load(&spec_path(dir))?
        .ok_or_else(|| io::Error::other("native failure lacks launch specification"))?;
    if runtime::load_runtime(dir)?.is_some_and(|record| record.launch_id != spec.launch_id) {
        return Err(io::Error::other("native failure launch identity differs"));
    }
    let budget = spec.write_budget.map(|limits| {
        WriteBudget::open(&dir.join("native-write-budget"), spec.launch_id, limits)
    }).transpose()?;
    runtime::mark_failed_with_budget(dir, reason, error, budget.as_ref())
}

pub(crate) async fn launch_owned(dir: &Path) -> Result<(), RecorderError> {
    if runtime::spec_path(dir).exists() {
        return Err(RecorderError::Missing(
            "ambiguous native and Chromium launch specs".into(),
        ));
    }
    let spec: LaunchSpec = load(&spec_path(dir))?
        .ok_or_else(|| RecorderError::Missing("no native-spec.json".into()))?;
    if !(1..=240_000).contains(&spec.acquisition_budget_ms) {
        return Err(RecorderError::Missing(
            "native acquisition budget must be 1..240000 ms".into(),
        ));
    }
    // Never reopen a previous target implicitly. Session::start requires a fresh directory too.
    if runtime::load_runtime(dir)?.is_some()
        || spec.session_root.as_ref().map(|root| root.join(spec.launch_id.as_str()))
            .unwrap_or_else(|| dir.join("native-session")).exists()
        || runtime::journal_path(dir).exists()
        || dir.join("native-status.json").exists()
    {
        return Err(RecorderError::Missing(
            "existing native execution requires evidence recovery, not launch".into(),
        ));
    }
    validate_write_budget(spec.write_budget)?;
    session_volume::validate(spec.session_volume.as_ref(), spec.session_root.as_deref(), &spec.target,
        spec.min_free_bytes.unwrap_or(storage::DEFAULT_MIN_FREE_BYTES))?;
    let direct_spec = spec.write_budget.is_some() && !dir.join("native-write-budget").try_exists()?;
    if direct_spec && dir.join("native-spec.lock").try_exists()? {
        return Err(RecorderError::Missing("launcher-created native specification lost its write allowance".into()));
    }
    let write_budget = spec.write_budget.map(|limits| {
        let path = dir.join("native-write-budget");
        if path.try_exists()? {
            WriteBudget::open(&path, spec.launch_id.clone(), limits)
        } else {
            WriteBudget::create(&path, spec.launch_id.clone(), limits)
        }
    }).transpose()?;
    let min_free_bytes = spec.min_free_bytes.unwrap_or(storage::DEFAULT_MIN_FREE_BYTES);
    let mut record = RuntimeRecord {
        launch_id: spec.launch_id.clone(),
        state: RuntimeState::Starting,
        recorder: ProcessIdentity::current(),
        browser: None,
        endpoint: None,
        socket: runtime::socket_path(dir),
    };
    save_budgeted(write_budget.as_ref(), Class::Retirement, &runtime::runtime_path(dir), &record)?;
    // Retain the owner first so a direct caller's oversized spec still has terminal failure evidence.
    if direct_spec && let Some(budget) = &write_budget {
        budget.charge(Class::Ordinary, std::fs::metadata(spec_path(dir))?.len())?;
    }
    // Publish the empty operation history before acquisition can outlive its recorder.
    // Missing journals remain errors; readers never infer an empty history from missing evidence.
    let journal = Journal::open(&runtime::journal_path(dir))?;
    // Publish the owner first so a refused acquisition has a durable terminal reason.
    let mut storage_roots = storage::Roots::for_launch(dir, &spec.target)?;
    let deadline = Instant::now() + Duration::from_millis(spec.acquisition_budget_ms);
    let mut routing = spec.clone();
    let mut volume = if let Some(config) = spec.session_volume.clone() {
        save_budgeted(write_budget.as_ref(), Class::Ordinary, &dir.join("native-storage-initial-roots.json"), &storage_roots)?;
        let root = dir.to_owned(); let launch = spec.launch_id.clone(); let budget = write_budget.clone();
        Some(tokio::task::spawn_blocking(move || session_volume::SessionVolume::start(
            &root, &launch, &config, deadline, budget.as_ref(), min_free_bytes)).await.map_err(io::Error::other)??)
    } else { None };
    if let Some(volume) = &volume { routing.session_root = Some(volume.root().to_owned()); }
    let preparation = (|| -> io::Result<()> {
        if let Some(parent) = &routing.session_root { storage_roots.route_session(parent, &spec.launch_id)?; }
        save_budgeted(write_budget.as_ref(), Class::Ordinary, &dir.join("native-storage-roots.json"), &storage_roots)?;
        storage::record_session_location(dir, &routing, write_budget.as_ref())?;
        storage_roots.admit(min_free_bytes)?;
        if let Some(mode) = &spec.service { mode.validate()?; }
        Ok(())
    })();
    if let Err(error) = preparation {
        storage_roots.release_control();
        return Err(session_volume::retire_after_preparation_failure(volume, dir, error, write_budget.as_ref()).await.into());
    }
    let controlled_resources = matches!(spec.service, Some(ServiceMode::ControlledAssets { .. }));
    let started_service = async {
        let service = if let Some(mode) = spec.service {
            let (assets, endpoint) = match mode {
                ServiceMode::ControlledHttp => (Vec::new(), None),
                ServiceMode::ControlledHttpAt(endpoint) => (Vec::new(), Some(endpoint)),
                ServiceMode::ControlledAssets { assets, endpoint } => (assets, endpoint),
            };
            Some(if let Some(budget) = write_budget.clone() {
                frontcap_service_fixture::owned::OwnedFixture::start_budgeted(
                    dir.join("service"), spec.launch_id.clone(), assets, endpoint, budget,
                ).await?
            } else if let Some(endpoint) = endpoint {
                frontcap_service_fixture::owned::OwnedFixture::start_with_assets_at(
                    dir.join("service"), spec.launch_id.clone(), assets, endpoint,
                ).await?
            } else {
                frontcap_service_fixture::owned::OwnedFixture::start_with_assets(
                    dir.join("service"), spec.launch_id.clone(), assets,
                ).await?
            })
        } else { None };
        Ok::<_, io::Error>(service)
    }.await;
    let mut service = match started_service {
        Ok(service) => service,
        Err(error) => {
            storage_roots.release_control();
            return Err(session_volume::retire_after_preparation_failure(volume, dir, error, write_budget.as_ref()).await.into());
        },
    };
    let mut target_dispatched = false;
    let mut acquired_retirement = None;
    let mut ios_startup_cleanup = frontcap_native::ios_session::StartupCleanup::Unproven;
    let acquisition = async {
        let deadline = if volume.is_some() { deadline } else {
            Instant::now() + Duration::from_millis(spec.acquisition_budget_ms)
        };
        let session_dir = session_directory(dir)?;
        let mut target = spec.target;
        if controlled_resources {
            target.configure_resources(service.as_ref().map(|service| service.initial()))?;
        } else {
            target.configure_service(service.as_ref().map(|service| service.initial()))?;
        }
        let endpoint = preparation_control::Endpoint::bind(dir).await?;
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let worker_budget = write_budget.clone();
        target_dispatched = true;
        let mut acquiring_volume = volume.take();
        let acquisition = tokio::task::spawn_blocking(move || {
            let acquired = if let Some(volume) = acquiring_volume.as_mut() {
                Session::acquire_with_cancellation(&spec.session_executable, target, &session_dir, deadline,
                    worker_budget.as_ref(), Some(&mut |session| volume.watch_acquisition(session, deadline)), &worker_cancelled)
            } else {
                Session::acquire_with_cancellation(&spec.session_executable, target, &session_dir, deadline, worker_budget.as_ref(), None, &worker_cancelled)
            };
            Ok::<_, io::Error>((acquired, acquiring_volume))
        });
        let acquired = endpoint.acquire(acquisition, service.as_ref().map(|service| service.controller()),
            &spec.launch_id, &cancelled, write_budget.as_ref()).await;
        drop(endpoint);
        let (acquired, acquired_volume) = acquired?;
        volume = acquired_volume;
        acquired_retirement = acquired.retirement;
        ios_startup_cleanup = acquired.ios_startup_cleanup;
        let (session, ready) = acquired.result?;
        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
            storage_roots.release_control();
            acquired_retirement = Some(tokio::task::spawn_blocking(move ||
                session.stop(Instant::now() + Duration::from_secs(60))).await.map_err(io::Error::other)??);
            return Err(io::Error::other("native preparation cancelled before readiness publication"));
        }
        let registration = (|| -> io::Result<()> {
            if let Some(volume) = volume.as_mut() { volume.watch_uikit(&session, deadline)?; }
            storage_roots.activate_generated()?;
            if routing.session_root.is_some() { storage_roots.register_session(&session_directory(dir)?)?; }
            save_budgeted(write_budget.as_ref(), Class::Ordinary, &dir.join("native-storage-acquired-roots.json"), &storage_roots)?;
            storage_roots.admit(min_free_bytes)
        })();
        if let Err(error) = registration {
            storage_roots.release_control();
            let retirement = tokio::task::spawn_blocking(move || session.stop(Instant::now() + Duration::from_secs(60))).await;
            let reason = format!("acquired storage registration: {error}; target retirement: {retirement:?}");
            if let Ok(Ok(retirement)) = retirement { acquired_retirement = Some(retirement); }
            return Err(io::Error::other(reason));
        }
        Ok((session, ready))
    }.await;
    let (session, ready) = match acquisition {
        Ok(acquired) => acquired,
        Err(error) => {
            storage_roots.release_control();
            // A returned acquisition failure can retire normally. Dropping the owner is reserved
            // for cancellation, where this future has no opportunity to await finalization.
            let error = if let Some(service) = service.take()
                && let Err(retirement) = service.shutdown().await {
                io::Error::other(format!("target acquisition: {error}; service retirement: {retirement}"))
            } else { error };
            if !target_dispatched {
                return Err(session_volume::retire_after_preparation_failure(volume, dir, error, write_budget.as_ref()).await.into());
            }
            if matches!(ios_startup_cleanup, frontcap_native::ios_session::StartupCleanup::NoWritersStarted) {
                let Some(volume) = volume else { return Err(error.into()); };
                let root = dir.to_owned(); let budget = write_budget.clone();
                let cleanup = tokio::task::spawn_blocking(move || volume.retire_metadata_failure(
                    &root, Instant::now() + Duration::from_secs(60), budget.as_ref())).await;
                return Err(io::Error::other(format!("{error}; session metadata failure volume retirement: {cleanup:?}")).into());
            }
            if matches!(ios_startup_cleanup, frontcap_native::ios_session::StartupCleanup::WorkerNotStarted) {
                let Some(volume) = volume else { return Err(error.into()); };
                let root = dir.to_owned(); let budget = write_budget.clone();
                let cleanup = tokio::task::spawn_blocking(move || volume.retire_failed_start(
                    &root, Instant::now() + Duration::from_secs(60), budget.as_ref())).await;
                return Err(io::Error::other(format!("{error}; unstarted worker volume retirement: {cleanup:?}")).into());
            }
            if let frontcap_native::ios_session::StartupCleanup::DiagnosticFailed { collector_pid } = ios_startup_cleanup {
                let Some(volume) = volume else { return Err(error.into()); };
                let root = dir.to_owned(); let budget = write_budget.clone();
                let cleanup = tokio::task::spawn_blocking(move || volume.retire_diagnostic_failure(
                    &root, collector_pid, Instant::now() + Duration::from_secs(60), budget.as_ref())).await;
                return Err(io::Error::other(format!("{error}; diagnostic failure volume retirement: {cleanup:?}")).into());
            }
            if let frontcap_native::ios_session::StartupCleanup::AcquisitionNotStarted { worker } = ios_startup_cleanup {
                let Some(volume) = volume else { return Err(error.into()); };
                let root = dir.to_owned(); let budget = write_budget.clone();
                let cleanup = tokio::task::spawn_blocking(move || volume.retire_before_acquisition(
                    &root, worker, Instant::now() + Duration::from_secs(60), budget.as_ref())).await;
                return Err(io::Error::other(format!("{error}; pre-acquisition volume retirement: {cleanup:?}")).into());
            }
            if let (Some(volume), Some(retirement)) = (volume, acquired_retirement) {
                let root = dir.to_owned(); let budget = write_budget.clone();
                let cleanup = tokio::task::spawn_blocking(move || volume.retire_failed_acquisition(
                    &root, retirement, Instant::now() + Duration::from_secs(60), budget.as_ref())).await;
                return Err(io::Error::other(format!("{error}; acquired session volume retirement: {cleanup:?}")).into());
            }
            return Err(error.into());
        }
    };
    let listener = crate::bind(&record.socket).await?;
    let mut capabilities = vec![
        Capability::Observe,
        Capability::ReadClock,
        Capability::Advance,
    ];
    if matches!(&session, Session::Ios(session)
        if session.surface() == frontcap_native::ios_execution::UiSurface::Uikit)
    {
        capabilities = vec![Capability::Observe, Capability::ReadClock,
            Capability::Advance, Capability::ObservedUikitTap, Capability::ObservedUikitPress, Capability::ObservedUikitText, Capability::ObservedUikitScroll, Capability::ExternalUrlDispatch];
    }
    if matches!(&ready, frontcap_recorder_protocol::native::Ready::Flutter { .. }) {
        capabilities = vec![Capability::Observe, Capability::ReadClock, Capability::Advance,
            Capability::Hold, Capability::ObservedSemanticTap, Capability::ObservedSemanticScroll, Capability::ObservedCommittedText, Capability::FrameworkRasterCapture];
        if matches!(&ready, frontcap_recorder_protocol::native::Ready::Flutter { external_url_target: Some(_), .. }) {
            capabilities.push(Capability::ExternalUrlDispatch);
        }
        if matches!(&ready, frontcap_recorder_protocol::native::Ready::Flutter { restart: true, .. }) {
            capabilities.push(Capability::FlutterRestart);
        }
        if matches!(&ready, frontcap_recorder_protocol::native::Ready::Flutter {
            owner: frontcap_core::native::flutter::Owner { platform: frontcap_core::native::flutter::Platform::Ios, .. },
            restart: true, external_url_target: Some(_), system_ui: Some(_), ..
        }) { capabilities.push(Capability::FlutterEntry); }
    }
    if matches!(&ready, frontcap_recorder_protocol::native::Ready::Ios { app_lifecycle: true, .. }
        | frontcap_recorder_protocol::native::Ready::Flutter { app_lifecycle: true, .. }) {
        capabilities.push(Capability::AppLifecycle);
    }
    if matches!(&ready, frontcap_recorder_protocol::native::Ready::Ios { restart: true, .. }) {
        capabilities.push(Capability::UikitRestart);
        if matches!(&ready, frontcap_recorder_protocol::native::Ready::Ios { cold_entry: true, .. }) {
            capabilities.push(Capability::UikitEntry);
        }
    }
    if matches!(&ready, frontcap_recorder_protocol::native::Ready::AndroidViews { .. }) {
        capabilities = vec![Capability::Observe, Capability::ReadClock, Capability::Advance, Capability::ObservedViewText, Capability::ObservedViewTap, Capability::ObservedViewScroll, Capability::ExternalUrlDispatch];
    }
    if matches!(&ready, frontcap_recorder_protocol::native::Ready::AndroidViews {restart: true, ..}) {
        capabilities.push(Capability::AndroidViewsRestart);
    }
    if matches!(&ready, frontcap_recorder_protocol::native::Ready::AndroidViews {app_lifecycle: true, ..}) {
        capabilities.push(Capability::AppLifecycle);
    }
    if matches!(&ready, frontcap_recorder_protocol::native::Ready::Ios { system_alerts: true, .. }
        | frontcap_recorder_protocol::native::Ready::Flutter { system_ui: Some(_), .. }) {
        capabilities.push(Capability::ObservedSystemAlerts);
    }
    if matches!(&ready, frontcap_recorder_protocol::native::Ready::Webview { .. }) {
        capabilities = vec![Capability::Observe, Capability::ReadClock, Capability::Advance,
            Capability::WebviewClock, Capability::WebviewRestart, Capability::WebviewProgress, Capability::WebviewComparison, Capability::NativeRasterCapture, Capability::ReconcileWebview];
        if matches!(&ready, frontcap_recorder_protocol::native::Ready::Webview { lifecycle: true, .. }) { capabilities.push(Capability::WebviewLifecycle); }
        if matches!(&ready, frontcap_recorder_protocol::native::Ready::Webview { contacts: true, .. }) { capabilities.push(Capability::ObservedWebviewContacts); }
    }
    if let frontcap_recorder_protocol::native::Ready::AndroidWebview { contacts, .. } = &ready {
        capabilities = vec![Capability::Observe, Capability::ReadClock, Capability::Advance, Capability::WebviewClock, Capability::NativeRasterCapture, Capability::WebviewLifecycle, Capability::WebviewComparison, Capability::WebviewProgress];
        if *contacts { capabilities.push(Capability::ObservedWebviewContacts); }
    }
    if let frontcap_recorder_protocol::native::Ready::IosHost { contacts, pages, system_ui, cold_entry, app_lifecycle, .. } = &ready {
        capabilities = vec![Capability::Observe, Capability::NativeRasterCapture, Capability::IosHostRestart, Capability::IosHostUrlDispatch];
        if *cold_entry {capabilities.push(Capability::IosHostColdEntry);}
        if *app_lifecycle {capabilities.push(Capability::IosHostLifecycle);}
        if *contacts { capabilities.push(Capability::ObservedNativeContacts); }
        if *pages { capabilities.push(Capability::WebviewAttachment); }
        if system_ui.is_some() { capabilities.push(Capability::ObservedSystemAlerts); }
    }
    if let frontcap_recorder_protocol::native::Ready::AndroidHost { contacts, .. } = &ready {
        capabilities = vec![Capability::Observe, Capability::NativeRasterCapture, Capability::WebviewAttachment, Capability::AndroidHostRestart, Capability::AndroidHostVisibility, Capability::AndroidHostUrlDispatch, Capability::AndroidHostEntry, Capability::AndroidSpeechObservation];
        if *contacts { capabilities.push(Capability::ObservedNativeContacts); }
    }
    if service.is_some() { capabilities.push(Capability::ControlledService); }
    if controlled_resources { capabilities.push(Capability::ControlledResources); }
    let status = Status {
        launch_id: spec.launch_id,
        run_id: spec.run_id,
        ready,
        capabilities,
        lease_held: false,
        running: None,
        last_completed_request_id: None,
        failure: None,
        stopped: false,
        evidence: session_directory(dir)?,
    };
    save_budgeted(write_budget.as_ref(), Class::Ordinary, &dir.join("native-status.json"), &status)?;
    let rec = Arc::new(Recorder {
        dir: dir.to_path_buf(),
        storage_roots,
        min_free_bytes,
        write_budget,
        state: Mutex::new(State {
            journal,
            status,
            lease: None,
            detached: vec![],
            retiring: false,
        }),
        session: Mutex::new(Some(session)),
        session_volume: Mutex::new(volume),
        service: service.as_ref().map(|service| service.controller()),
        shutdown: tokio::sync::Notify::new(),
    });
    record.state = RuntimeState::Ready;
    rec.save(Class::Ordinary, &runtime::runtime_path(dir), &record)?;
    let serving = serve(rec.clone(), listener, service.as_mut()).await;
    rec.storage_roots.release_control();
    // Keep the service owner in this launch future, not detached connection tasks. Cancellation
    // drops it; a normal exit waits for HTTP retirement before publishing terminal runtime state.
    let service_shutdown = match service {
        Some(service) => service.shutdown().await,
        None => Ok(()),
    };
    if let Err(error) = &serving {
        rec.state.lock().unwrap().status.failure = Some(format!("native server: {error}"));
    }
    if let Err(error) = &service_shutdown {
        rec.state.lock().unwrap().status.failure = Some(format!("service retirement: {error}"));
    }
    rec.save(Class::Retirement, &dir.join("native-status.json"), &Recorder::snapshot(&rec.state.lock().unwrap()))?;
    let _ = std::fs::remove_file(&record.socket);
    record.state = rec
        .state
        .lock()
        .unwrap()
        .status
        .failure
        .clone()
        .map_or(RuntimeState::Stopped, |reason| RuntimeState::Failed {
            reason,
            error: None,
        });
    rec.save(Class::Retirement, &runtime::runtime_path(dir), &record)?;
    serving?;
    service_shutdown?;
    Ok(())
}
async fn serve(
    rec: Arc<Recorder>,
    listener: UnixListener,
    mut service: Option<&mut frontcap_service_fixture::owned::OwnedFixture>,
) -> io::Result<()> {
    let mut next = 0u64;
    let mut service_ended = false;
    let mut supervision_tick = tokio::time::interval(Duration::from_secs(1));
    supervision_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut storage_pressure = None;
    let mut worker_lost = false;
    let mut retirement_task: Option<tokio::task::JoinHandle<io::Result<()>>> = None;
    loop {
        tokio::select! {
            _ = supervision_tick.tick(), if retirement_task.is_none() => {
                // Normal Stop may intentionally remove a registered owned volume before the
                // final answer. Its original retirement proof remains the authority.
                if rec.state.lock().unwrap().retiring { continue; }
                // Independent cleanup after worker exit may remove roots before this tick.
                // Keep the first observed storage failure if it already preceded worker loss.
                if storage_pressure.is_none() && (worker_lost || storage_watch::worker_exited(&rec)?) {
                    worker_lost = true;
                    retirement_task = storage_watch::retire_worker_loss(
                        &rec, "native session worker exited; retiring target from original evidence")?;
                    continue;
                }
                if storage_pressure.is_none()
                    && let Err(error) = rec.storage_roots.admit(rec.min_free_bytes) {
                    // Exit can race the first sample and remove a root before admission reads it.
                    if storage_watch::worker_exited(&rec)? {
                        worker_lost = true;
                        retirement_task = storage_watch::retire_worker_loss(
                            &rec, "native session worker exited; retiring target from original evidence")?;
                        continue;
                    }
                    storage_pressure = Some(format!("native storage pressure: {error}; retiring target"));
                }
                if let Some(reason) = &storage_pressure {
                    retirement_task = storage_watch::retire(&rec, reason)?;
                }
            }
            retired = async {
                match retirement_task.as_mut() {
                    Some(task) => task.await,
                    None => std::future::pending().await,
                }
            } => {
                retired.map_err(io::Error::other)??;
                return Ok(());
            }
            result = async {
                match service.as_mut() {
                    Some(service) => service.wait_for_exit().await,
                    None => std::future::pending().await,
                }
            }, if !service_ended => {
                service_ended = true;
                let reason = match result {
                    Ok(()) => "controlled service exited before recorder retirement".to_owned(),
                    Err(error) => format!("controlled service failed: {error}"),
                };
                // Keep result lookup and explicit Stop available. Never replace the service or
                // overwrite the outcome of an already dispatched native operation.
                rec.fail(reason);
            }
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                next += 1;
                tokio::spawn(connection(rec.clone(), stream, next));
            }
            _ = rec.shutdown.notified() => {
                // A connection can notice stopped status before the background task publishes
                // its retained answer. Keep the owner alive until that publication settles.
                if let Some(task) = retirement_task.take() {
                    task.await.map_err(io::Error::other)??;
                }
                return Ok(());
            }
        }
    }
}
async fn connection(rec: Arc<Recorder>, stream: UnixStream, conn: u64) {
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);
    let mut open = true;
    while open {
        let frame: ClientFrame = match read_frame(&mut r).await {
            Ok(Some(frame)) => frame,
            _ => break,
        };
        let handling = rec.handle(conn, frame.call);
        tokio::pin!(handling);
        let answer = tokio::select! {
            biased;
            answer = &mut handling => answer,
            _ = r.fill_buf() => {
                if r.buffer().is_empty() { open = false; rec.closed(conn); }
                handling.await
            }
        };
        if open
            && write_frame(
                &mut w,
                &ServerFrame {
                    id: frame.id,
                    answer,
                },
            )
            .await
            .is_err()
        {
            open = false;
        }
        if rec.state.lock().unwrap().status.stopped {
            rec.shutdown.notify_one();
            break;
        }
    }
    rec.closed(conn);
}

#[cfg(test)]
mod tests;

/// Resolve an already completed UIKit observation to its original storage epoch; no target work.
pub fn ios_observation_directory(runtime:&Path,id:&RequestId)->io::Result<std::path::PathBuf> {
    let root=session_directory(runtime)?.join("target");
    if !root.join("restarts/history.json").exists() {return Ok(root.join("ui-client"));}
    let (_,epoch)=frontcap_native::ios_restarts::request_epoch(&root,id)?
        .ok_or_else(||io::Error::other("UIKit observation absent from lineage"))?;
    Ok(epoch.join("ui-client"))
}

/// Locate a retained Android View capture without acquiring the original app or worker.
pub fn android_view_observation_directory(runtime: &Path, id: &RequestId) -> io::Result<PathBuf> {
    frontcap_native::session::view_observation_directory(&session_directory(runtime)?, id)?
        .ok_or_else(|| io::Error::other("missing Android View observation epoch"))
}
