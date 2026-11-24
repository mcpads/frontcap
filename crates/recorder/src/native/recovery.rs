//! Read-only result reconciliation. Target ownership and retirement remain separate evidence.
use super::LaunchSpec;
use crate::runtime;
use frontcap_core::{
    ids::{LaunchId, RequestId},
    journal::{Journal, RequestState, args_sha256},
    state::{load, save_atomic},
};
use frontcap_native::{receipt, session, store::RequestStore};
use frontcap_recorder_protocol::{
    Answer, Reply, RequestResult,
    native::{Op, Outcome, Reply as NativeReply},
};
use serde::{Deserialize, Serialize};
use std::{io, path::Path};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    op: Op,
    budget_ms: u64,
}

pub(super) fn record_bytes(op: &Op, budget_ms: u64) -> u64 {
    frontcap_core::canonical::canonical_of(&Intent { op: op.clone(), budget_ms }).len() as u64
}

pub(super) fn record(dir: &Path, id: &RequestId, op: &Op, budget_ms: u64) -> io::Result<()> {
    std::fs::create_dir_all(dir.join("native-requests"))?;
    let path = dir.join("native-requests").join(format!("{id}.json"));
    if path.exists() {
        return Err(io::Error::other("native admission evidence already exists"));
    }
    save_atomic(
        &path,
        &Intent {
            op: op.clone(),
            budget_ms,
        },
    )
}
fn required<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    load(path)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("missing native evidence: {}", path.display()),
        )
    })
}

/// Only used for a dispatched native journal entry. Never changes a request's durable history.
pub fn saved_result(dir: &Path, launch_id: &LaunchId, id: &RequestId) -> io::Result<RequestResult> {
    let pending = RequestResult::Pending { dispatched: true };
    let record =
        runtime::load_runtime(dir)?.ok_or_else(|| io::Error::other("missing native runtime"))?;
    if &record.launch_id != launch_id {
        return Err(io::Error::other("native runtime launch identity differs"));
    }
    // A live recorder remains the authority for in-flight operations. Failed/stopped snapshots do
    // not grant permission to bypass a live writer or target. Wait for process exit before deriving.
    if !record.recorder.has_exited()? {
        return Ok(pending);
    }
    let Some((hash, RequestState::Dispatched)) =
        Journal::read_record(&runtime::journal_path(dir), id).map_err(io::Error::other)?
    else {
        return Err(io::Error::other(
            "native recovery requires an unchanged dispatched journal entry",
        ));
    };
    let spec: LaunchSpec = required(&super::spec_path(dir))?;
    if &spec.launch_id != launch_id {
        return Err(io::Error::other("native spec launch identity differs"));
    }
    let plan_path = dir.join("native-requests").join(format!("{id}.json"));
    // Older recorder revisions lack an operation record. Hash-only history cannot be guessed.
    let Some(plan): Option<Intent> = load(&plan_path)? else {
        return Ok(pending);
    };
    if !(1..=60_000).contains(&plan.budget_ms)
        || args_sha256(&serde_json::to_value((&plan.op, plan.budget_ms))?) != hash
    {
        return Err(io::Error::other(
            "native operation evidence differs from journal fingerprint",
        ));
    }
    // Service requests use a separate evidence boundary. An unpublished result remains uncertain;
    // never derive mobile/application completion or automatically resend a response.
    if matches!(plan.op, Op::InspectService | Op::ReleaseService { .. } | Op::ReleaseAsset { .. }) { return Ok(pending); }
    if matches!(plan.op, Op::Stop) && spec.session_volume.is_some() {
        return Ok(match super::session_volume::saved_stop(dir, &spec, id)? {
            Some(retirement) => completed(id, Outcome::Stopped { retirement }),
            None => pending,
        });
    }
    let session_dir = super::session_directory(dir)?;
    let outcome = match &spec.target {
        super::target::Launch::IosHost(config) => {
            use frontcap_native::webview_session as worker;
            if matches!(plan.op,Op::Stop) {
                worker::inspect(&session_dir)?.retirement().filter(|r| r.passed)
                    .map(|retirement| Outcome::Stopped { retirement })
            } else {
                let operation=match plan.op {
                    Op::Observe=>frontcap_core::native::ios_host::Operation::Observe {input_basis:false},
                    Op::ExecuteIosHost {operation}=>operation,
                    _=>return Err(io::Error::other("operation differs from iOS native host history")),
                };
                frontcap_native::ios_host_session::read_result(&session_dir,config,id,&operation)?.map(super::target::ios_host_reply).transpose()?
            }
        }
        super::target::Launch::AndroidHost(config) => {
            use frontcap_native::android_webview_session as worker;
            if matches!(plan.op, Op::Stop) {
                worker::inspect(&session_dir)?.retirement().filter(|r| r.passed)
                    .map(|retirement| Outcome::Stopped { retirement })
            } else {
                let operation = match plan.op {
                    Op::Observe => frontcap_core::native::android_host::Operation::Observe { input_basis: false },
                    Op::ExecuteAndroidHost { operation } => operation,
                    _ => return Err(io::Error::other("operation differs from native host history")),
                };
                let Some(ready) = load::<frontcap_core::native::android_host::Ready>(&session_dir.join("host-ready.json"))? else { return Ok(pending); };
                config.validate_host_ready(&ready)?;
                worker::read_host_result(&session_dir, config, id, &operation)?.map(super::target::host_reply).transpose()?
            }
        }
        super::target::Launch::AndroidWebview(config) => {
            use frontcap_native::android_webview_session::{self as worker, Operation};
            if matches!(plan.op, Op::Stop) {
                worker::inspect(&session_dir)?.retirement().filter(|r| r.passed)
                    .map(|retirement| Outcome::Stopped { retirement })
            } else {
                let operation = match plan.op {
                    Op::Observe => Operation::Observe { input_basis: false },
                    Op::ExecuteAndroidWebview { operation } => operation,
                    _ => return Err(io::Error::other("operation differs from Android WebView history")),
                };
                let Some(ready) = load::<worker::Ready>(&session_dir.join("ready.json"))? else { return Ok(pending); };
                config.validate_ready(&ready)?;
                worker::read_result(&session_dir, id, &operation)?.map(|reply| match reply {
                    worker::Reply::Completed { completion } => Outcome::AndroidWebviewCompleted { completion: *completion },
                    worker::Reply::Failed { error } => Outcome::AndroidWebviewUnresolved { error, lifecycle: None },
                    worker::Reply::HomeReturnFailed { error, lifecycle } => Outcome::AndroidWebviewUnresolved { error, lifecycle: lifecycle.map(|value| *value) },
                })
            }
        }
        super::target::Launch::Webview(config) => {
            // A dead recorder does not regain ownership or replay an uncertain page operation.
            if matches!(plan.op, Op::Stop) {
                frontcap_native::webview_session::inspect(&session_dir)?.retirement().filter(|r| r.passed)
                    .map(|retirement| Outcome::Stopped { retirement })
            } else if let Op::ExecuteWebview { operation } = &plan.op {
                frontcap_native::webview_session::read_clock_result(&session_dir, config, id, operation)?
                    .map(|reply| match reply {
                        frontcap_native::webview_session::Reply::Completed { completion } =>
                            Outcome::WebviewCompleted { completion },
                        frontcap_native::webview_session::Reply::Failed { error } => Outcome::WebviewUnresolved { error },
                    })
            } else { None }
        }
        super::target::Launch::Android(config) => {
            android_result(&session_dir, config, id, plan.op)?
        }
        super::target::Launch::Ios(config) => ios_result(&session_dir, config, id, plan.op)?,
        super::target::Launch::Flutter(config) => {
            flutter_result(&session_dir, config, id, plan.op)?
        }
    };
    let Some(outcome) = outcome else {
        return Ok(pending);
    };
    Ok(completed(id, outcome))
}
fn completed(id: &RequestId, outcome: Outcome) -> RequestResult {
    RequestResult::Completed {
        answer: Box::new(Answer::Ok(Reply::Native {
            body: NativeReply::Done { request_id: id.clone(), outcome: Box::new(outcome) },
        })),
    }
}

#[cfg(test)]
#[path = "recovery_tests.rs"]
mod tests;

fn android_result(
    session_dir: &Path,
    config: &frontcap_native::device::AndroidLaunch,
    id: &RequestId,
    op: Op,
) -> io::Result<Option<Outcome>> {
    if let Some(app) = &config.view_app {
        let Some(owner): Option<frontcap_core::native::android_app::Owner> = load(&session_dir.join("view-ready.json"))? else {return Ok(None);};
        owner.validate().map_err(io::Error::other)?;
        if owner.package != app.package || owner.activity != app.activity || owner.source_sha256 != config.source_sha256
            || owner.apk_sha256 != config.apk_sha256 || owner.serial != format!("emulator-{}",config.port) {
            return Err(io::Error::other("Android View owner differs from frozen launch inputs"));
        }
        return match op {
            Op::SetAppState {generation,state} => frontcap_native::android_app_state::read_completion(session_dir,id,&generation,state)
                .map(|r|r.map(|transition|Outcome::AndroidViewsStateChanged {transition})),
            Op::RestartAndroidViews {generation, url} => frontcap_native::android_restarts::read_completion(session_dir,id,&generation,url.as_deref())
                .map(|r|r.map(|replacement|Outcome::AndroidViewsRestarted {replacement})),
            Op::DispatchUrl { request } => {
                let (owner, _) = frontcap_native::android_restarts::epochs(session_dir)?.into_iter().find(|(o,_)|o.generation==request.generation)
                    .ok_or_else(||io::Error::other("Android URL generation absent from lineage"))?;
                let directory = frontcap_native::android_restarts::device_epoch(session_dir,&owner)?.join("external-entry");
                frontcap_recorder_protocol::native::Ready::AndroidViews { owner, restart: false, app_lifecycle: false }
                    .validate_operation(&Op::DispatchUrl { request: request.clone() }).map_err(io::Error::other)?;
                // A recorder can exit after admission but before the session creates its journal.
                if !directory.join("owner.json").exists() { return Ok(None); }
                frontcap_native::external_entry::read_accepted(&directory, id, &request)
                    .map(|result| result.map(|acceptance| Outcome::ExternalUrlAccepted { acceptance }))
            },
            Op::Execute {operation} => {
                let Some((owner,epoch))=frontcap_native::android_restarts::request_epoch(session_dir,id)? else {return Ok(None);};
                let device=frontcap_native::android_restarts::device_epoch(session_dir,&owner)?;
                operation.validate(&owner.identity()).map_err(io::Error::other)?;
                if !epoch.join("requests/requests.json").exists() {return Ok(None);}
                let result=RequestStore::read_completion(&epoch.join("requests"),&owner.identity(),&owner.generation,&owner.source_sha256,id,&operation).map_err(io::Error::other)?;
                if let Some(completion)=&result {let channel=match completion {
                    frontcap_core::native::request::Completion::ViewText {..} => "view-edit",
                    frontcap_core::native::request::Completion::ViewScroll {..} => "view-scroll",
                    frontcap_core::native::request::Completion::ViewTap {..} => "view-tap",
                    _=>"view-clock",
                };
                    receipt::verify(&device.join(channel).join("receipts"),completion)?;}
                Ok(result.map(|completion|Outcome::Completed {completion}))
            },
            Op::Observe => Ok(session::read_view_result(session_dir,id)?.map(|result|match result {
                frontcap_core::native::android_capture::Collection::Observed(capture)=>Outcome::AndroidViewsObserved {capture},
                frontcap_core::native::android_capture::Collection::Unavailable(observation)=>Outcome::AndroidViewsUnavailable {observation},
            })),
            Op::Stop => match session::inspect(session_dir)? {
                session::Recovery::Finished {result,..} => Ok(result.retirement.filter(|r|r.passed).map(|retirement|Outcome::Stopped {retirement})),
                _ => Ok(None),
            },
            _ => Err(io::Error::other("unsupported Android View operation in history")),
        };
    }
    Err(io::Error::other("missing Android View profile"))
}
fn ios_result(
    session_dir: &Path,
    config: &frontcap_native::ios_target::IosLaunch,
    id: &RequestId,
    op: Op,
) -> io::Result<Option<Outcome>> {
    use frontcap_core::native::ios_request::{Operation, Owner};
    use frontcap_native::{ios_execution::Executor, ios_session};
    let Some(owner): Option<Owner> = load(&session_dir.join("ready.json"))? else {
        return Ok(None);
    };
    owner.validate().map_err(io::Error::other)?;
    let bound: Owner = required(&session_dir.join("target/request-owner.json"))?;
    let acquired: frontcap_native::xctest::XctestLaunch =
        required(&session_dir.join("target/launch.json"))?;
    if owner != bound
        || owner.source_sha256 != config.xctest.simulator.source_sha256
        || owner.controller_id != config.controller_id
        || serde_json::to_value(acquired)? != serde_json::to_value(&config.xctest)?
    {
        return Err(io::Error::other(
            "iOS acquired owner differs from frozen launch inputs",
        ));
    }
    if matches!(op, Op::Stop) {
        return Ok(ios_session::inspect(session_dir)?.retirement()
            .filter(|r| r.passed).map(|retirement| Outcome::Stopped { retirement }));
    }
    let target=session_dir.join("target");
    if let Op::SetAppState {generation,state} = op {
        let Some((owner,epoch))=frontcap_native::ios_restarts::request_epoch(&target,id)? else {return Ok(None)};
        if generation!=owner.app.generation {return Err(io::Error::other("UIKit host transition generation differs"));}
        return frontcap_native::ios_app_state::read_completion(&epoch,id,&owner,state)
            .map(|result|result.map(|transition|Outcome::UikitStateChanged {transition}));
    }
    if let Op::RestartUikit { generation, url } = op {
        if config.surface!=frontcap_native::ios_execution::UiSurface::Uikit {return Err(io::Error::other("restart requires UIKit"));}
        return frontcap_native::ios_restarts::read_completion(&target,id,&generation,url.as_deref())
            .map(|r|r.map(|replacement|Outcome::UikitRestarted {replacement: Box::new(replacement)}));
    }
    if let Op::DispatchUrl { request } = op {
        let (owner,epoch)=frontcap_native::ios_restarts::epochs(&target)?.into_iter().find(|(o,_)|o.app.generation==request.generation)
            .ok_or_else(||io::Error::other("UIKit URL generation absent from lineage"))?;
        let external_url_bundle = (config.surface == frontcap_native::ios_execution::UiSurface::Uikit)
            .then(|| config.xctest.app.bundle.clone());
        frontcap_recorder_protocol::native::Ready::Ios { owner, external_url_bundle, system_alerts: false, restart: false, cold_entry: false, app_lifecycle: false }
            .validate_operation(&Op::DispatchUrl { request: request.clone() }).map_err(io::Error::other)?;
        let directory = epoch.join("external-entry");
        if !directory.join("owner.json").exists() { return Ok(None); }
        return frontcap_native::external_entry::read_accepted(&directory, id, &request)
            .map(|result| result.map(|acceptance| Outcome::ExternalUrlAccepted { acceptance }));
    }
    let operation = match op {
        Op::Observe => config.surface.observation(),
        Op::ExecuteIos { operation } if !matches!(operation, Operation::Finish) => operation,
        _ => {
            return Err(io::Error::other(
                "invalid operation in iOS recorder history",
            ));
        }
    };
    let Some((owner,epoch))=frontcap_native::ios_restarts::request_epoch(&target,id)? else {return Ok(None)};
    Executor::read_completion(&epoch, &owner, id, &operation)
        .map(|result| result.map(|completion| Outcome::IosCompleted { completion }))
        .map_err(io::Error::other)
}

fn flutter_result(
    dir: &Path,
    config: &frontcap_native::flutter_target::Launch,
    id: &RequestId,
    op: Op,
) -> io::Result<Option<Outcome>> {
    use frontcap_core::native::flutter::{Operation, Owner};
    let Some(initial): Option<Owner> = load(&dir.join("ready.json"))? else { return Ok(None); };
    initial.validate().map_err(io::Error::other)?;
    let acquired: frontcap_native::flutter_target::Launch = required(&dir.join("launch.json"))?;
    if initial.source_sha256 != config.worker.config.source_sha256 || initial.platform != config.worker.config.platform
        || serde_json::to_value(acquired)? != serde_json::to_value(config)? {
        return Err(io::Error::other("Flutter acquired owner differs from frozen launch"));
    }
    if matches!(op, Op::Stop) {
        return match frontcap_native::flutter_session::inspect(&dir.join("session"))? {
            frontcap_native::flutter_session::Recovery::Finished(result) if result.retirement.passed =>
                Ok(Some(Outcome::Stopped { retirement: result.retirement })),
            _ => Ok(None),
        };
    }
    if let Op::BeginFlutterEntry { generation, url } = op {
        return frontcap_native::flutter_restarts::read_entry(dir, id, &generation, &url)
            .map(|r| r.map(|entry| Outcome::FlutterEntryStarted { entry }));
    }
    if let Op::PollFlutterEntry { entry_request_id, generation } = op {
        return frontcap_native::flutter_restarts::read_poll(dir, id, &entry_request_id, &generation)
            .map(|r| r.map(|progress| Outcome::FlutterEntryPolled { progress }));
    }
    if let Op::RestartFlutter { generation, url } = op {
        if config.worker.config.installed_app.is_none() { return Err(io::Error::other("Flutter restart was not configured")); }
        return frontcap_native::flutter_restarts::read_completion(dir, id, &generation, url.as_deref())
            .map(|r|r.map(|replacement| Outcome::FlutterRestarted { replacement }));
    }
    let epochs = frontcap_native::flutter_restarts::epochs(dir)?;
    if let Op::SetAppState {generation,state} = op {
        let (ready, _) = epochs.iter().find(|(ready,_)|ready.owner.generation == generation)
            .ok_or_else(||io::Error::other("Flutter visibility generation absent from lineage"))?;
        return frontcap_native::flutter_app_state::read_completion(dir, &ready.owner, id, state)
            .map(|result|result.map(|transition|Outcome::FlutterStateChanged {transition}));
    }
    if let Op::ExecuteIos { operation } = op {
        let ready = &epochs[0].0; // The independent system runner retains its original session owner.
        frontcap_native::flutter_system_ui::validate_ready(&config.worker.config, ready)?;
        let runner = ready.system_ui.as_ref().ok_or_else(|| io::Error::other("no acquired Flutter system runner"))?;
        return frontcap_native::flutter_system_ui::read_completion(dir, &initial, runner, id, &operation)
            .map(|result| result.map(|completion| Outcome::IosCompleted {
                completion: frontcap_core::native::ios_request::Completion::Ui(Box::new(completion)) }));
    }
    if let Op::DispatchUrl { request } = op {
        let (ready, epoch) = epochs.iter().find(|(r,_)| r.owner.generation == request.generation)
            .ok_or_else(||io::Error::other("URL generation absent from Flutter lineage"))?;
        frontcap_recorder_protocol::native::Ready::Flutter { owner: ready.owner.clone(), restart: config.worker.config.installed_app.is_some(), app_lifecycle: false,
            external_url_target: ready.external_url_target.clone(), system_ui: ready.system_ui.clone() }
            .validate_operation(&Op::DispatchUrl { request: request.clone() }).map_err(io::Error::other)?;
        let entries = epoch.join("external-entry");
        if !entries.join("owner.json").exists() { return Ok(None); }
        return frontcap_native::external_entry::read_accepted(&entries, id, &request)
            .map(|saved| saved.map(|acceptance| Outcome::ExternalUrlAccepted { acceptance }));
    }
    let (operation, generation) = match op {
        Op::Observe => (Operation::Observe {}, None),
        Op::ExecuteFlutter { generation, operation } if !matches!(operation, Operation::Stop {}) => (operation, Some(generation)),
        _ => return Err(io::Error::other("invalid operation in Flutter history")),
    };
    let mut owner = None;
    for (ready, epoch) in &epochs {
        let requests = epoch.join("requests");
        if requests.join("requests.json").exists()
            && frontcap_native::flutter_store::RequestStore::has_request(&requests, &ready.owner, id).map_err(io::Error::other)?
            && owner.replace(&ready.owner).is_some() {
            return Err(io::Error::other("Flutter request ID appears in multiple generations"));
        }
    }
    let Some(owner) = owner else { return Ok(None); };
    if generation.is_some_and(|g| g != owner.generation) { return Err(io::Error::other("Flutter request generation differs from its original epoch")); }
    frontcap_native::flutter_target::read_completion(dir, owner, id, &operation)
        .map(|result| result.map(|completion| Outcome::FlutterCompleted { completion }))
}
