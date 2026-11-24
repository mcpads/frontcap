//! Native MCP operations delegate to the recorder; no device execution lives in this server.
use crate::{dispatch::Frontcap, server::CallToolResult, tools::ToolError};
use frontcap_core::{
    canonical::sha256_bytes_hex,
    ids::{LaunchId, RequestId},
    native::{
        ios_request::{Completion as IosCompletion, Operation as IosOperation},
        ios_ui::UiOutcome,
        request::Operation,
    },
    state::load,
};
use frontcap_recorder::{
    native::launcher::{self, HostProfile},
    runtime::{self, RuntimeRecord, RuntimeState},
};
use frontcap_recorder_protocol::{
    Answer, Call, Reply, RequestResult,
    native::{Op as RecorderOp, Outcome, Ready, Reply as NativeReply, Status},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[path = "native_storage.rs"]
mod storage;

type Result<T> = std::result::Result<T, ToolError>;
#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    #[default]
    Android,
    Ios,
}
#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppRuntime {
    Host,
    #[default]
    Native,
    Flutter,
    Webview,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Args {
    pub op: Op,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    /// Bounded, non-atomic logical size of this runtime; does not contact the target or authorize deletion.
    Storage { launch_id: LaunchId, max_entries: Option<u32> },
    /// Observe SpringBoard alerts through the owned UIKit runner; does not sample the app clock.
    ObserveSystemAlerts { launch_id: LaunchId, request_id: RequestId, budget_ms: Option<u64>, inline: Option<bool> },
    /// Tap a chosen button from the latest stable system-alert catalog, revalidated before delivery.
    TapSystemButton { launch_id: LaunchId, request_id: RequestId, observation: String, reference: String, budget_ms: Option<u64> },
    /// Deliver an ordinary OS URL to the current View/UIKit owner. Acceptance is not destination proof.
    DispatchUrl { launch_id: LaunchId, request_id: RequestId, generation: String, url: String, budget_ms: Option<u64> },
    IosHost { launch_id: LaunchId, request_id: RequestId, operation: frontcap_core::native::ios_host::Operation, budget_ms: Option<u64>, inline: Option<bool> },
    AndroidHost {
        launch_id: LaunchId,
        request_id: RequestId,
        operation: frontcap_core::native::android_host::Operation,
        budget_ms: Option<u64>,
        inline: Option<bool>,
    },
    AndroidWebview {
        launch_id: LaunchId,
        request_id: RequestId,
        operation: frontcap_core::native::android_webview::Operation,
        budget_ms: Option<u64>,
        /// Include the verified PNG for observe operations (default true); ignored for other operations.
        inline: Option<bool>,
    },
    RestartWebview { launch_id: LaunchId, request_id: RequestId, generation: String, budget_ms: Option<u64> },
    CompareWebviewProgress { launch_id: LaunchId, request_id: RequestId, before: RequestId, after: RequestId, before_capture: Option<RequestId>, after_capture: Option<RequestId>, budget_ms: Option<u64> },
    InspectWebviewProgress { launch_id: LaunchId, request_id: RequestId, budget_ms: Option<u64> },
    WebviewBackground { launch_id: LaunchId, request_id: RequestId, budget_ms: Option<u64> },
    WebviewForeground { launch_id: LaunchId, request_id: RequestId, budget_ms: Option<u64> },
    ReconcileWebviewLifecycle { launch_id: LaunchId, request_id: RequestId, original_request_id: RequestId, budget_ms: Option<u64> },
    WebviewContacts { launch_id: LaunchId, request_id: RequestId, observation: RequestId, operation: frontcap_core::native::contacts::Operation, budget_ms: Option<u64> },
    ReconcileWebviewContacts { launch_id: LaunchId, request_id: RequestId, original_request_id: RequestId, budget_ms: Option<u64> },
    ReconcileWebview { launch_id: LaunchId, request_id: RequestId, original_request_id: RequestId, budget_ms: Option<u64> },
    InspectService {
        launch_id: LaunchId, request_id: RequestId, budget_ms: Option<u64>,
    },
    ReleaseService {
        launch_id: LaunchId, request_id: RequestId, generation: LaunchId,
        request: RequestId, response: frontcap_core::native::service::ResponseSpec,
        budget_ms: Option<u64>,
    },
    ReleaseAsset {
        launch_id: LaunchId, request_id: RequestId, generation: LaunchId,
        request: RequestId, sha256: String, budget_ms: Option<u64>,
    },
    Describe {},
    Launch {
        request_id: RequestId,
        #[serde(default)]
        platform: Platform,
        #[serde(default)]
        runtime: AppRuntime,
    },
    BeginLaunch {
        request_id: RequestId,
        #[serde(default)]
        platform: Platform,
        #[serde(default)]
        runtime: AppRuntime,
    },
    Preparation { launch_id: LaunchId },
    /// Request cancellation from the original acquisition owner; admission is not retirement proof.
    CancelPreparation { launch_id: LaunchId, request_id: RequestId },
    ReleasePreparationAsset {
        launch_id: LaunchId, generation: LaunchId, release_id: RequestId,
        request: RequestId, sha256: String,
    },
    Flutter {
        launch_id: LaunchId,
        request_id: RequestId,
        generation: String,
        operation: frontcap_core::native::flutter::Operation,
        budget_ms: Option<u64>,
        inline: Option<bool>,
    },
    RestartAndroidViews { launch_id: LaunchId, request_id: RequestId, generation: String, url: Option<String>, budget_ms: Option<u64> },
    SetAppState { launch_id: LaunchId, request_id: RequestId, generation: String, state: frontcap_core::native::app_state::State, budget_ms: Option<u64> },
    RestartUikit { launch_id: LaunchId, request_id: RequestId, generation: String, #[serde(default, skip_serializing_if = "Option::is_none")] url: Option<String>, budget_ms: Option<u64> },
    RestartFlutter { launch_id: LaunchId, request_id: RequestId, generation: String, url: Option<String>, budget_ms: Option<u64> },
    BeginFlutterEntry { launch_id: LaunchId, request_id: RequestId, generation: String, url: String, budget_ms: Option<u64> },
    PollFlutterEntry { launch_id: LaunchId, request_id: RequestId, entry_request_id: RequestId, generation: String, budget_ms: Option<u64> },
    Status {
        launch_id: LaunchId,
    },
    Reattach {
        launch_id: LaunchId,
        request_id: RequestId,
    },
    Release {
        launch_id: LaunchId,
        request_id: RequestId,
    },
    Result {
        launch_id: LaunchId,
        request_id: RequestId,
        /// Explicitly read the original observation PNG; defaults false and never repeats target work.
        inline: Option<bool>,
    },
    Observe {
        launch_id: LaunchId,
        request_id: RequestId,
        budget_ms: Option<u64>,
        inline: Option<bool>,
    },
    ReadClock {
        launch_id: LaunchId,
        request_id: RequestId,
        budget_ms: Option<u64>,
    },
    Advance {
        launch_id: LaunchId,
        request_id: RequestId,
        by_ms: u64,
        budget_ms: Option<u64>,
    },
    TapTarget {
        launch_id: LaunchId,
        request_id: RequestId,
        reference: String,
        observation: String,
        budget_ms: Option<u64>,
    },
    PressTarget {
        launch_id: LaunchId,
        request_id: RequestId,
        reference: String,
        observation: String,
        advance_ms: u64,
        budget_ms: Option<u64>,
    },
    ScrollTarget {
        launch_id:LaunchId, request_id:RequestId, reference:String, observation:String,
        direction:frontcap_core::native::ios_scroll::Direction, budget_ms:Option<u64>,
    },
    ReplaceText {
        launch_id:LaunchId, request_id:RequestId, reference:String, observation:String,
        start:usize, end:usize, text:String, budget_ms:Option<u64>,
    },
    Stop {
        launch_id: LaunchId,
        request_id: RequestId,
        budget_ms: Option<u64>,
    },
}
#[derive(Serialize)]
#[serde(tag = "response", rename_all = "snake_case")]
pub enum Response {
    Storage { launch_id: LaunchId, usage: Box<storage::Usage> },
    PreparationCancellation { launch_id: LaunchId, result: frontcap_recorder::native::preparation_control::Cancellation },
    PreparationRelease { launch_id: LaunchId, result: frontcap_core::native::service::Reply },
    Preparation { snapshot: Box<frontcap_recorder::native::preparation::Snapshot> },
    Description {
        operations: serde_json::Value,
        guidance: &'static str,
    },
    Recorder {
        launch_id: LaunchId,
        result: Box<Reply>,
    },
    /// Last persisted status is evidence, not a claim that a pending request is still running.
    RecordedStatus {
        launch_id: LaunchId,
        runtime: Box<RuntimeRecord>,
        status: Option<Box<Status>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        worker_terminal: Option<frontcap_recorder::native::WorkerTerminalDiagnostics>,
    },
}
fn fault(detail: impl ToString) -> ToolError {
    ToolError::new(
        "recorder_fault",
        detail.to_string(),
        "use native result with the original request_id; inspect the existing execution before replacing it",
    )
}
fn native_error(mut error: ToolError) -> ToolError {
    error.recovery = match error.code.as_str() {
        "lease_required" | "transport_lost" => "use native result for any pending request, then native reattach to the same live recorder",
        "lease_held" => "release the lease from its controller with native release, or wait for that controller to disconnect",
        "busy" => "read native status and native result for the running request",
        "recorder_fault" => "read native result with the original request_id and native status; inspect failure and retirement evidence before further work",
        "stopped" => "read native result for saved outcomes; launch a new generation only after confirmed retirement",
        "stale_ref" | "observation_changed" => "take a fresh native observation before another tap",
        _ => return error,
    }.into();
    error
}
fn from_launch(error: frontcap_recorder::launcher::LaunchFailure) -> ToolError {
    native_error(crate::connections::from_reply(error.0))
}
impl Frontcap {
    fn native_lease(&self, id: &LaunchId) -> Result<frontcap_recorder_protocol::LeaseToken> {
        self.connections.lease_of(id).ok_or_else(|| {
            native_error(ToolError::new(
                "lease_required",
                "this MCP server holds no native lease",
                "",
            ))
        })
    }
    fn native_runtime(&self, id: &LaunchId) -> Result<RuntimeRecord> {
        let dir = self.config.workspace.runtime_dir(id);
        if !frontcap_recorder::native::spec_path(&dir).is_file() {
            return Err(ToolError::new(
                "unsupported_capability",
                "generation is not a native recorder",
                "use this generation's advertised control surface",
            ));
        }
        let record: RuntimeRecord = load(&runtime::runtime_path(&dir))
            .map_err(fault)?
            .ok_or_else(|| fault("native runtime record missing"))?;
        if &record.launch_id != id {
            return Err(fault("native runtime launch identity differs"));
        }
        Ok(record)
    }
    fn native_saved_status(&self, id: &LaunchId) -> Result<Option<Status>> {
        let saved: Option<Status> = load(
            &self
                .config
                .workspace
                .runtime_dir(id)
                .join("native-status.json"),
        )
        .map_err(fault)?;
        if saved.as_ref().is_some_and(|status| &status.launch_id != id) {
            return Err(fault("saved native status launch identity differs"));
        }
        Ok(saved)
    }
    async fn native_call(&self, id: &LaunchId, call: Call) -> Result<Reply> {
        self.native_runtime(id)?;
        if let Call::Result { request_id } = &call {
            // Stop publishes this terminal snapshot before replying/unlinking the socket. The
            // common runtime record is finalized afterwards, so it can still say Ready here.
            let status = self.native_saved_status(id)?;
            if status.is_some_and(|s| s.stopped) {
                let result = frontcap_recorder::launcher::saved_result(
                    &self.config.workspace,
                    id,
                    request_id,
                )
                .map_err(from_launch)?;
                return Ok(Reply::Result {
                    request_id: request_id.clone(),
                    result,
                });
            }
        }
        self.connections
            .call(&self.config.workspace, id, call)
            .await
            .inspect_err(|e| {
                if e.code == "lease_required" {
                    self.connections.forget(id);
                }
            })
            .map_err(native_error)
    }
    async fn native_command(
        &self,
        id: &LaunchId,
        request_id: RequestId,
        op: RecorderOp,
        budget_ms: Option<u64>,
    ) -> Result<Reply> {
        let lease = self.native_lease(id)?;
        self.native_call(
            id,
            Call::Native {
                lease,
                request_id,
                op,
                budget_ms: budget_ms.unwrap_or(20_000),
            },
        )
        .await
    }
    async fn native_ready(&self, id: &LaunchId) -> Result<Ready> {
        let Reply::Native {
            body: NativeReply::Status { status },
        } = self.native_call(id, Call::Status).await?
        else {
            return Err(fault("missing native status"));
        };
        Ok(status.ready)
    }
    async fn native_view_basis(&self, launch: &LaunchId, observation: String) -> Result<(RequestId, frontcap_core::native::android_capture::Capture)> {
        let id=RequestId::new(observation).map_err(|e|ToolError::new(
            "invalid_arguments",e.to_string(),"use the recorder observation request ID"))?;
        let reply=self.native_call(launch,Call::Result {request_id:id.clone()}).await?;
        let Reply::Result {result:RequestResult::Completed {answer},..}=reply else {return Err(fault("input observation is incomplete"))};
        let Answer::Ok(Reply::Native {body:NativeReply::Done {outcome,..}})=*answer else {return Err(fault("input observation has no native evidence"))};
        match *outcome {
            Outcome::AndroidViewsObserved {capture}=>Ok((id,capture)),
            Outcome::AndroidViewsUnavailable {..}=>Err(ToolError::new("not_actionable","observation has no actionable capture","observe again with a new request ID within your readiness budget")),
            _=>Err(fault("input requires an Android View capture")),
        }
    }
    async fn native_run(&self, args: Args) -> Result<Response> {
        let begin_only = matches!(&args.op, Op::BeginLaunch { .. });
        let (launch_id, answer) = match args.op {
            Op::Storage { launch_id, max_entries } => {
                self.native_runtime(&launch_id)?;
                let max_entries = max_entries.unwrap_or(10_000);
                if !(1..=100_000).contains(&max_entries) {
                    return Err(ToolError::new("invalid_arguments", "storage max_entries must be 1..100000", "choose a bounded inventory size"));
                }
                let root = self.config.workspace.runtime_dir(&launch_id);
                let usage = tokio::task::spawn_blocking(move || storage::measure(&root, max_entries))
                    .await.map_err(fault)?.map_err(fault)?;
                return Ok(Response::Storage { launch_id, usage: Box::new(usage) });
            }
            Op::ReleasePreparationAsset {launch_id, generation, release_id, request, sha256} => {
                let result = frontcap_recorder::native::preparation_control::release(
                    &self.config.workspace.runtime_dir(&launch_id),
                    frontcap_recorder::native::preparation_control::Release {launch_id:launch_id.clone(), generation, release_id, request, sha256},
                ).await.map_err(|error| ToolError::new("preparation_outcome_unknown", error.to_string(), "inspect preparation and repeat only the same release_id and arguments; never replace an unknown release"))?;
                return Ok(Response::PreparationRelease {launch_id, result});
            }
            Op::CancelPreparation { launch_id, request_id } => {
                let result = frontcap_recorder::native::preparation_control::cancel(
                    &self.config.workspace.runtime_dir(&launch_id),
                    frontcap_recorder::native::preparation_control::Cancel { launch_id: launch_id.clone(), request_id },
                ).await.map_err(|error| ToolError::new("preparation_cancellation_unknown", error.to_string(),
                    "inspect preparation and repeat only the original cancellation ID; admission is not retirement proof"))?;
                return Ok(Response::PreparationCancellation { launch_id, result });
            }
            Op::Preparation { launch_id } => {
                let snapshot = frontcap_recorder::native::preparation::inspect(&self.config.workspace.runtime_dir(&launch_id), &launch_id).map_err(fault)?;
                return Ok(Response::Preparation {snapshot:Box::new(snapshot)});
            }
            Op::Describe {} => {
                return Ok(Response::Description {
                    operations: serde_json::to_value(schemars::schema_for!(Op))
                        .expect("native operation schema"),
                    guidance: include_str!("native_guidance.txt"),
                });
            }
            Op::Launch { request_id, platform, runtime }
            | Op::BeginLaunch { request_id, platform, runtime } => {
                let name = match (runtime, platform) {
                    (AppRuntime::Host, Platform::Android) => "host-android.json",
                    (AppRuntime::Host, Platform::Ios) => "host-ios.json",
                    (AppRuntime::Webview, Platform::Ios) => "webview-ios.json",
                    (AppRuntime::Webview, Platform::Android) => "webview-android.json",
                    (AppRuntime::Native, Platform::Android) => "native-android.json",
                    (AppRuntime::Native, Platform::Ios) => "native-ios.json",
                    (AppRuntime::Flutter, Platform::Android) => "flutter-android.json",
                    (AppRuntime::Flutter, Platform::Ios) => "flutter-ios.json",
                };
                let profile: HostProfile = load(&self.config.workspace.root.join(name)).map_err(fault)?.ok_or_else(|| ToolError::new("not_configured", format!("native host profile {name} is missing"), "configure the selected platform with frozen launch inputs and its session worker"))?;
                let matches = match (&profile.target, runtime, platform) {
                    (frontcap_recorder::native::target::Launch::IosHost(_), AppRuntime::Host, Platform::Ios) => true,
                    (frontcap_recorder::native::target::Launch::AndroidHost(_), AppRuntime::Host, Platform::Android) => true,
                    (frontcap_recorder::native::target::Launch::AndroidWebview(_), AppRuntime::Webview, Platform::Android) => true,
                    (frontcap_recorder::native::target::Launch::Webview(_), AppRuntime::Webview, Platform::Ios) => true,
                    (
                        frontcap_recorder::native::target::Launch::Android(_),
                        AppRuntime::Native,
                        Platform::Android,
                    )
                    | (
                        frontcap_recorder::native::target::Launch::Ios(_),
                        AppRuntime::Native,
                        Platform::Ios,
                    ) => true,
                    (
                        frontcap_recorder::native::target::Launch::Flutter(config),
                        AppRuntime::Flutter,
                        platform,
                    ) => matches!(
                        (&config.worker.config.platform, platform),
                        (
                            frontcap_core::native::flutter::Platform::Android,
                            Platform::Android
                        ) | (frontcap_core::native::flutter::Platform::Ios, Platform::Ios)
                    ),
                    _ => false,
                };
                if !matches {
                    return Err(fault(
                        "host profile belongs to another platform or app runtime",
                    ));
                }
                let request = profile.request(request_id);
                let id =
                    launcher::register(&self.config.workspace, &request).map_err(from_launch)?;
                if begin_only {
                    let id = launcher::begin(&self.config.workspace, request, &self.config.recorder_bin, &[]).map_err(from_launch)?;
                    let snapshot = frontcap_recorder::native::preparation::inspect(&self.config.workspace.runtime_dir(&id), &id).map_err(fault)?;
                    return Ok(Response::Preparation {snapshot:Box::new(snapshot)});
                }
                if self.connections.lease_of(&id).is_some() {
                    let status = self.native_call(&id, Call::Status).await?;
                    (id, status)
                } else {
                    let attached = launcher::launch(
                        &self.config.workspace,
                        request,
                        &self.config.recorder_bin,
                        &[],
                    )
                    .await
                    .map_err(from_launch)?;
                    let answer = Reply::Native {
                        body: NativeReply::Attached {
                            lease: attached.lease.clone(),
                            status: Box::new(attached.status),
                            completed_while_detached: attached.completed_while_detached,
                        },
                    };
                    self.connections
                        .hold(id.clone(), attached.lease, attached.client);
                    (id, answer)
                }
            }
            Op::Status { launch_id } => {
                let record = self.native_runtime(&launch_id)?;
                let saved = self.native_saved_status(&launch_id)?;
                if record.state == RuntimeState::Stopped
                    || !record.recorder.is_alive()
                    || saved.as_ref().is_some_and(|s| s.stopped)
                {
                    let worker_terminal = frontcap_recorder::native::worker_terminal_diagnostics(
                        &self.config.workspace.runtime_dir(&launch_id));
                    return Ok(Response::RecordedStatus {
                        worker_terminal,
                        launch_id,
                        runtime: Box::new(record),
                        status: saved.map(Box::new),
                    });
                }
                let answer = self.native_call(&launch_id, Call::Status).await?;
                (launch_id, answer)
            }
            Op::Reattach {
                launch_id,
                request_id,
            } => {
                let record = self.native_runtime(&launch_id)?;
                let saved = self.native_saved_status(&launch_id)?;
                // Stop publishes its terminal snapshot before unlinking the socket and
                // finalizing the runtime record. Neither window permits reconnection.
                if record.state == RuntimeState::Stopped || saved.is_some_and(|s| s.stopped) {
                    return Err(native_error(ToolError::new(
                        "stopped",
                        "the native generation has stopped; it cannot resume",
                        "",
                    )));
                }
                if self.connections.lease_of(&launch_id).is_some() {
                    let answer = self.native_call(&launch_id, Call::Status).await?;
                    (launch_id, answer)
                } else {
                    let attached =
                        launcher::reattach(&self.config.workspace, &launch_id, request_id)
                            .await
                            .map_err(from_launch)?;
                    let answer = Reply::Native {
                        body: NativeReply::Attached {
                            lease: attached.lease.clone(),
                            status: Box::new(attached.status),
                            completed_while_detached: attached.completed_while_detached,
                        },
                    };
                    self.connections
                        .hold(launch_id.clone(), attached.lease, attached.client);
                    (launch_id, answer)
                }
            }
            Op::Release {
                launch_id,
                request_id,
            } => {
                let lease = self.native_lease(&launch_id)?;
                let answer = self
                    .native_call(&launch_id, Call::Release { lease, request_id })
                    .await?;
                self.connections.forget(&launch_id);
                (launch_id, answer)
            }
            Op::Result {
                launch_id,
                request_id,
                ..
            } => {
                let answer = self
                    .native_call(&launch_id, Call::Result { request_id })
                    .await?;
                (launch_id, answer)
            }
            Op::Observe {
                launch_id,
                request_id,
                budget_ms,
                ..
            } => {
                let answer = self
                    .native_command(&launch_id, request_id, RecorderOp::Observe, budget_ms)
                    .await?;
                (launch_id, answer)
            }
            Op::Flutter {
                launch_id,
                request_id,
                generation,
                operation,
                budget_ms,
                ..
            } => {
                if !matches!(self.native_ready(&launch_id).await?, Ready::Flutter { .. }) {
                    return Err(ToolError::new(
                        "unsupported_operation",
                        "generation is not Flutter",
                        "use advertised target operations",
                    ));
                }
                let answer = self
                    .native_command(
                        &launch_id,
                        request_id,
                        RecorderOp::ExecuteFlutter {
                            generation,
                            operation,
                        },
                        budget_ms,
                    )
                    .await?;
                (launch_id, answer)
            }
            Op::IosHost { launch_id, request_id, operation, budget_ms, .. } => {
                let answer=self.native_command(&launch_id,request_id,RecorderOp::ExecuteIosHost {operation},budget_ms).await?;
                (launch_id,answer)
            }
            Op::AndroidHost { launch_id, request_id, operation, budget_ms, .. } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::ExecuteAndroidHost { operation }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::AndroidWebview { launch_id, request_id, operation, budget_ms, .. } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::ExecuteAndroidWebview { operation }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::CompareWebviewProgress { launch_id, request_id, before, after, before_capture, after_capture, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::CompareWebviewProgress {
                    before, after, before_capture, after_capture
                }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::RestartWebview { launch_id, request_id, generation, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::ExecuteWebview {
                    operation: frontcap_core::native::webview::Operation::Restart { generation }
                }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::RestartAndroidViews { launch_id, request_id, generation, url, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::RestartAndroidViews {generation, url}, budget_ms).await?;
                (launch_id, answer)
            }
            Op::SetAppState {launch_id,request_id,generation,state,budget_ms} => {
                let answer=self.native_command(&launch_id,request_id,RecorderOp::SetAppState {generation,state},budget_ms).await?;
                (launch_id, answer)
            }
            Op::RestartUikit { launch_id, request_id, generation, url, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::RestartUikit { generation, url }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::RestartFlutter { launch_id, request_id, generation, url, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::RestartFlutter { generation, url }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::BeginFlutterEntry { launch_id, request_id, generation, url, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::BeginFlutterEntry { generation, url }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::PollFlutterEntry { launch_id, request_id, entry_request_id, generation, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::PollFlutterEntry { entry_request_id, generation }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::ObserveSystemAlerts { launch_id, request_id, budget_ms, .. } => {
                let operation = match self.native_ready(&launch_id).await? {
                    Ready::IosHost { .. } => RecorderOp::ExecuteIosHost { operation: frontcap_core::native::ios_host::Operation::ObserveSystemAlerts },
                    _ => RecorderOp::ExecuteIos { operation: IosOperation::ObserveSystemAlerts },
                };
                let answer = self.native_command(&launch_id, request_id, operation, budget_ms).await?;
                (launch_id, answer)
            }
            Op::TapSystemButton { launch_id, request_id, observation, reference, budget_ms } => {
                let operation = match self.native_ready(&launch_id).await? {
                    Ready::IosHost { .. } => RecorderOp::ExecuteIosHost { operation: frontcap_core::native::ios_host::Operation::TapSystemButton { observation, reference } },
                    _ => RecorderOp::ExecuteIos { operation: IosOperation::TapSystemButton { observation, reference } },
                };
                let answer = self.native_command(&launch_id, request_id, operation, budget_ms).await?;
                (launch_id, answer)
            }
            Op::DispatchUrl { launch_id, request_id, generation, url, budget_ms } => {
                let target = match self.native_ready(&launch_id).await? {
                    Ready::AndroidViews { owner, .. } => frontcap_core::native::external_entry::Target::Android {
                        serial: owner.serial, package: owner.package,
                    },
                    Ready::Ios { owner, external_url_bundle: Some(bundle), .. } => frontcap_core::native::external_entry::Target::Ios {
                        simulator: owner.app.simulator.to_ascii_lowercase(), bundle,
                    },
                    Ready::Flutter { external_url_target: Some(target), .. } => target,
                    _ => return Err(ToolError::new("unsupported_operation", "external URL dispatch requires an advertised View, UIKit or Flutter owner", "use advertised target operations")),
                };
                let request = frontcap_core::native::external_entry::Request {
                    target, generation, url,
                };
                let answer = self.native_command(&launch_id, request_id, RecorderOp::DispatchUrl { request }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::InspectWebviewProgress { launch_id, request_id, budget_ms } => {
                let op = match self.native_ready(&launch_id).await? {
                    Ready::AndroidWebview { .. } => RecorderOp::ExecuteAndroidWebview { operation: frontcap_core::native::android_webview::Operation::InspectProgress },
                    _ => RecorderOp::ExecuteWebview { operation: frontcap_core::native::webview::Operation::InspectProgress },
                };
                let answer = self.native_command(&launch_id, request_id, op, budget_ms).await?;
                (launch_id, answer)
            }
            Op::WebviewBackground { launch_id, request_id, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::ExecuteWebview {
                    operation: frontcap_core::native::webview::Operation::Background
                }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::WebviewForeground { launch_id, request_id, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::ExecuteWebview {
                    operation: frontcap_core::native::webview::Operation::Foreground
                }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::ReconcileWebviewLifecycle { launch_id, request_id, original_request_id, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::ExecuteWebview {
                    operation: frontcap_core::native::webview::Operation::ReconcileLifecycle { request_id: original_request_id }
                }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::WebviewContacts { launch_id, request_id, observation, operation, budget_ms } => {
                let op = match self.native_ready(&launch_id).await? {
                    Ready::AndroidWebview { .. } => RecorderOp::ExecuteAndroidWebview { operation: frontcap_core::native::android_webview::Operation::Contacts { observation, input: operation } },
                    _ => RecorderOp::ExecuteWebview { operation: frontcap_core::native::webview::Operation::Contacts { observation, input: operation } },
                };
                let answer = self.native_command(&launch_id, request_id, op, budget_ms).await?;
                (launch_id, answer)
            }
            Op::ReconcileWebviewContacts { launch_id, request_id, original_request_id, budget_ms } => {
                let op = match self.native_ready(&launch_id).await? {
                    Ready::AndroidWebview { .. } => RecorderOp::ExecuteAndroidWebview { operation: frontcap_core::native::android_webview::Operation::ReconcileContacts { request_id: original_request_id } },
                    _ => RecorderOp::ExecuteWebview { operation: frontcap_core::native::webview::Operation::ReconcileContacts { request_id: original_request_id } },
                };
                let answer = self.native_command(&launch_id, request_id, op, budget_ms).await?;
                (launch_id, answer)
            }
            Op::ReconcileWebview { launch_id, request_id, original_request_id, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::ExecuteWebview {
                    operation: frontcap_core::native::webview::Operation::Reconcile { request_id: original_request_id }
                }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::ReadClock {
                launch_id,
                request_id,
                budget_ms,
            } => {
                let answer = self
                    .native_command(
                        &launch_id,
                        request_id,
                        match self.native_ready(&launch_id).await? {
                            Ready::FlutterEntering { .. } => return Err(native_error(ToolError::new("entry_pending", "Flutter app is awaiting cold-entry readiness", "observe system alerts or poll the original entry"))),
                            Ready::IosHost { .. } | Ready::AndroidHost { .. } => return Err(ToolError::new("unsupported_operation", "native host has no controlled page clock", "use advertised native host operations")),
                            Ready::AndroidWebview { .. } => RecorderOp::ExecuteAndroidWebview { operation: frontcap_core::native::android_webview::Operation::Read },
                            Ready::Webview { .. } => RecorderOp::ExecuteWebview { operation: frontcap_core::native::webview::Operation::Clock { kind: frontcap_core::native::request::ClockOperation::Read, delta_ns: 0 } },
                            Ready::Flutter { owner, .. } => RecorderOp::ExecuteFlutter {
                                generation: owner.generation,
                                operation: frontcap_core::native::flutter::Operation::Read {},
                            },
                            Ready::AndroidViews { .. } => RecorderOp::Execute {
                                operation: Operation::ReadClock {},
                            },
                            Ready::Ios { .. } => RecorderOp::ExecuteIos {
                                operation: IosOperation::ReadClock,
                            },
                        },
                        budget_ms,
                    )
                    .await?;
                (launch_id, answer)
            }
            Op::Advance {
                launch_id,
                request_id,
                by_ms,
                budget_ms,
            } => {
                let op = match self.native_ready(&launch_id).await? {
                    Ready::FlutterEntering { .. } => return Err(native_error(ToolError::new("entry_pending", "Flutter app is awaiting cold-entry readiness", "observe system alerts or poll the original entry"))),
                    Ready::IosHost { .. } | Ready::AndroidHost { .. } => return Err(ToolError::new("unsupported_operation", "native host has no controlled page clock", "use advertised native host operations")),
                    Ready::AndroidWebview { .. } => RecorderOp::ExecuteAndroidWebview { operation: frontcap_core::native::android_webview::Operation::Timers { action: frontcap_core::native::android_timers::Action::Advance { milliseconds: u32::try_from(by_ms).map_err(fault)? } } },
                    Ready::Webview { .. } => RecorderOp::ExecuteWebview { operation: frontcap_core::native::webview::Operation::Clock { kind: frontcap_core::native::request::ClockOperation::Advance, delta_ns: by_ms.checked_mul(1_000_000).ok_or_else(|| fault("WebView delta overflows"))? } },
                    Ready::Flutter { owner, .. } => RecorderOp::ExecuteFlutter {
                        generation: owner.generation,
                        operation: frontcap_core::native::flutter::Operation::Advance {
                            delta_us: by_ms.checked_mul(1000).ok_or_else(|| {
                                ToolError::new(
                                    "invalid_arguments",
                                    "Flutter delta overflows",
                                    "use a representable delta",
                                )
                            })?,
                        },
                    },
                    Ready::AndroidViews { .. } => RecorderOp::Execute {
                        operation: Operation::Advance {
                            by_us: by_ms.checked_mul(1000).ok_or_else(|| {
                                ToolError::new(
                                    "invalid_arguments",
                                    "native millisecond delta overflows",
                                    "use a representable virtual-time delta",
                                )
                            })?,
                        },
                    },
                    Ready::Ios { .. } => RecorderOp::ExecuteIos {
                        operation: IosOperation::AdvanceClock {
                            delta_ns: by_ms.checked_mul(1_000_000).ok_or_else(|| {
                                ToolError::new(
                                    "invalid_arguments",
                                    "native millisecond delta overflows",
                                    "use a representable virtual-time delta",
                                )
                            })?,
                        },
                    },
                };
                let answer = self
                    .native_command(&launch_id, request_id, op, budget_ms)
                    .await?;
                (launch_id, answer)
            }
            Op::TapTarget { launch_id, request_id, reference, observation, budget_ms } => {
                let operation=if matches!(self.native_ready(&launch_id).await?,Ready::AndroidViews {..}) {
                    let (observation,capture)=self.native_view_basis(&launch_id,observation).await?;
                    let point=capture.catalog.targets.iter().find(|v|v.token==reference).and_then(|v|v.tap_point_screen_px)
                        .ok_or_else(||ToolError::new("not_actionable","target has no observed tap point","observe a visible supported target"))?;
                    let tap=frontcap_core::native::android_tap::Tap {observation,source_observation:capture.after_request.clone(),
                        reference,now_ns:capture.now_ns,point};
                    tap.validate_target(&capture).map_err(|e|ToolError::new("not_actionable",e.to_string(),"observe a supported target again"))?;
                    RecorderOp::Execute {operation:Operation::TapView {tap}}
                } else {RecorderOp::ExecuteIos {operation:IosOperation::TapTarget {reference,observation}}};
                let answer=self.native_command(&launch_id,request_id,operation,budget_ms).await?;
                (launch_id, answer)
            }
            Op::PressTarget { launch_id, request_id, reference, observation, advance_ms, budget_ms } => {
                let delta_ns=advance_ms.checked_mul(1_000_000).ok_or_else(|| ToolError::new(
                    "invalid_arguments", "press clock delta overflows", "use a representable advance_ms"))?;
                let answer=self.native_command(&launch_id,request_id,RecorderOp::ExecuteIos {
                    operation:IosOperation::PressTarget { reference, observation, delta_ns },
                },budget_ms).await?;
                (launch_id,answer)
            }
            Op::ScrollTarget { launch_id,request_id,reference,observation,direction,budget_ms } => {
                let operation=if matches!(self.native_ready(&launch_id).await?,Ready::AndroidViews {..}) {
                    use frontcap_core::native::{android_scroll,ios_scroll};
                    let direction=match direction {
                        ios_scroll::Direction::Up=>android_scroll::Direction::Up,ios_scroll::Direction::Down=>android_scroll::Direction::Down,
                        ios_scroll::Direction::Left=>android_scroll::Direction::Left,ios_scroll::Direction::Right=>android_scroll::Direction::Right,
                    };
                    let (observation,capture)=self.native_view_basis(&launch_id,observation).await?;
                    let geometry=capture.catalog.targets.iter().find(|v|v.token==reference).and_then(|v|v.scroll.as_ref())
                        .ok_or_else(||ToolError::new("not_actionable","target has no supported scroll geometry","observe a supported stock scroll view"))?;
                    let gesture=geometry.gestures.iter().find(|g|g.direction==direction).cloned()
                        .ok_or_else(||ToolError::new("not_actionable","scroll direction is not advertised","inspect source offset/range to distinguish current end from unavailable input"))?;
                    let scroll=android_scroll::Scroll {observation,source_observation:capture.after_request.clone(),reference,
                        now_ns:capture.now_ns,gesture,before_offset_px:geometry.offset_px};
                    scroll.validate_target(&capture).map_err(|e|ToolError::new("not_actionable",e.to_string(),"observe a supported trajectory again"))?;
                    RecorderOp::Execute {operation:Operation::ScrollView {scroll}}
                } else {RecorderOp::ExecuteIos {operation:IosOperation::ScrollTarget {reference,observation,direction}}};
                let answer=self.native_command(&launch_id,request_id,operation,budget_ms).await?;
                (launch_id,answer)
            }
            Op::ReplaceText { launch_id,request_id,reference,observation,start,end,text,budget_ms } => {
                let operation=if matches!(self.native_ready(&launch_id).await?, Ready::AndroidViews {..}) {
                    let (observed_id,capture)=self.native_view_basis(&launch_id,observation).await?;
                    let before=capture.catalog.targets.iter().find(|v|v.token==reference).and_then(|v|v.text.clone())
                        .ok_or_else(||ToolError::new("not_actionable","edit target has no source text","focus an editable field and observe again"))?;
                    let offset=|value:usize|u32::try_from(value).map_err(|_|ToolError::new("invalid_arguments","UTF-16 offset exceeds range","use an observed text range"));
                    let edit=frontcap_core::native::android_edit::Edit {observation:observed_id,
                        source_observation:capture.after_request.clone(),reference,now_ns:capture.now_ns,before,
                        start:offset(start)?,end:offset(end)?,text};
                    edit.validate_target(&capture).map_err(|e|ToolError::new("not_actionable",e.to_string(),"observe a focused field and use its UTF-16 range/limit"))?;
                    RecorderOp::Execute {operation:Operation::ReplaceViewText {edit}}
                } else {RecorderOp::ExecuteIos {operation:IosOperation::ReplaceText {reference,observation,start,end,text}}};
                let answer=self.native_command(&launch_id,request_id,operation,budget_ms).await?;
                (launch_id,answer)
            }
            Op::InspectService { launch_id, request_id, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::InspectService, budget_ms).await?;
                (launch_id, answer)
            }
            Op::ReleaseService { launch_id, request_id, generation, request, response, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::ReleaseService { generation, request, response }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::ReleaseAsset { launch_id, request_id, generation, request, sha256, budget_ms } => {
                let answer = self.native_command(&launch_id, request_id, RecorderOp::ReleaseAsset { generation, request, sha256 }, budget_ms).await?;
                (launch_id, answer)
            }
            Op::Stop {
                launch_id,
                request_id,
                budget_ms,
            } => {
                let answer = self
                    .native_command(&launch_id, request_id, RecorderOp::Stop, budget_ms)
                    .await?;
                if matches!(&answer, Reply::Native { body: NativeReply::Done { outcome, .. } } if matches!(outcome.as_ref(), Outcome::Stopped { .. }))
                {
                    self.connections.forget(&launch_id);
                }
                (launch_id, answer)
            }
        };
        Ok(Response::Recorder {
            launch_id,
            result: Box::new(answer),
        })
    }
    pub(crate) async fn native_result(&self, args: Args) -> CallToolResult {
        let inline = if let Op::Result { inline, .. } = &args.op { inline.unwrap_or(false) } else { matches!(&args.op,
            Op::ObserveSystemAlerts { inline, .. } | Op::Observe { inline, .. } | Op::Flutter { inline, .. }
            | Op::IosHost {operation:frontcap_core::native::ios_host::Operation::Observe {..} | frontcap_core::native::ios_host::Operation::ObserveSystemAlerts,inline,..}
            | Op::AndroidHost { operation: frontcap_core::native::android_host::Operation::Observe { .. }, inline, .. }
            | Op::AndroidWebview { operation: frontcap_core::native::android_webview::Operation::Observe { .. }, inline, .. }
            if inline.unwrap_or(true)) };
        match self.native_run(args).await {
            Ok(response)=>self.native_render_response(response,inline),
            Err(error)=>error.result(),
        }
    }
    fn native_render_response(&self, response: Response, inline: bool) -> CallToolResult {
        if let Response::Recorder { result, .. } = &response
            && let Reply::Native { body: NativeReply::Done { outcome, .. } } = result.as_ref()
            && matches!(outcome.as_ref(),
                Outcome::IosHostUnresolved { .. }
                | Outcome::AndroidHostUnresolved { .. }
                | Outcome::AndroidWebviewUnresolved { .. }
                | Outcome::WebviewUnresolved { .. })
        {
            // Preserve the original uncertainty; an error does not prove that no effect occurred.
            return CallToolResult::error(&response);
        }
        let completed = match &response {
            Response::Recorder { launch_id, result } => match result.as_ref() {
                Reply::Native { body: NativeReply::Done { outcome, request_id } } => Some((launch_id, request_id, outcome)),
                Reply::Result { request_id: original, result: RequestResult::Completed { answer } } => match answer.as_ref() {
                    Answer::Ok(Reply::Native { body: NativeReply::Done { outcome, request_id } }) if original == request_id => Some((launch_id, request_id, outcome)),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        if inline && let Some((launch_id, request_id, outcome)) = completed {
            if matches!(outcome.as_ref(),Outcome::AndroidViewsUnavailable {..})
                || matches!(outcome.as_ref(), Outcome::FlutterCompleted { completion }
                if !matches!(completion.capture, Some(frontcap_core::native::flutter::Capture::Complete { .. })))
                || matches!(outcome.as_ref(), Outcome::IosCompleted { completion: IosCompletion::Ui(c) }
                    if matches!(c.outcome, UiOutcome::ObservationUnavailable { .. }))
            {
                return CallToolResult::structured(&response);
            }
            let png = match outcome.as_ref() {
                Outcome::IosHostCompleted {completion:frontcap_core::native::ios_host::Completion::SystemUi {completion,directory}} => self.native_host_system_png(launch_id,directory,&completion.outcome),
                Outcome::IosHostCompleted {completion:frontcap_core::native::ios_host::Completion::Observed {capture}} => self.native_page_png(launch_id,request_id,&capture.path,capture.bytes,&capture.sha256,false),
                Outcome::AndroidHostCompleted { completion: frontcap_core::native::android_host::Completion::Observed { screen, bytes, sha256, .. } } => self.native_page_png(launch_id, request_id, screen, *bytes, sha256, true),
                Outcome::AndroidWebviewCompleted { completion: frontcap_core::native::android_webview::Completion::Observed { screen, bytes, sha256, .. } } => self.native_page_png(launch_id, request_id, screen, *bytes, sha256, true),
                Outcome::WebviewCompleted { completion } => match completion.as_ref() {
                    frontcap_core::native::webview::Completion::Captured { path, bytes, sha256, .. } => self.native_webview_png(launch_id, request_id, path, *bytes, sha256),
                    _ => Err(fault("observation has no image")),
                },
                Outcome::FlutterCompleted { completion } => self.native_flutter_png(launch_id, request_id, completion),
                Outcome::AndroidViewsObserved { capture } => self.native_view_png(launch_id, request_id, capture),
                Outcome::IosCompleted { completion: IosCompletion::Ui(completion) } => self.native_ios_png(launch_id, request_id, &completion.outcome),
                _ => Err(fault("observation has no image")),
            };
            return match png {
                Ok(png) => CallToolResult::with_png(&response, &png),
                Err(error) => error.result(),
            };
        }
        CallToolResult::structured(&response)
    }
    fn native_webview_png(&self, launch: &LaunchId, request: &RequestId, recorded: &std::path::Path, size: u64, digest: &str) -> Result<Vec<u8>> {
        self.native_page_png(launch, request, recorded, size, digest, false)
    }
    fn native_page_png(&self, launch: &LaunchId, request: &RequestId, recorded: &std::path::Path, size: u64, digest: &str, android: bool) -> Result<Vec<u8>> {
        // Completed captures remain readable in their original epoch after replacement.
        // Accept only known producer layouts, never an arbitrary recorded filesystem path.
        let root = frontcap_recorder::native::session_directory(&self.config.workspace.runtime_dir(launch)).map_err(fault)?;
        if std::fs::symlink_metadata(&root).map_err(fault)?.file_type().is_symlink() { return Err(fault("linked native session")); }
        let root = root.canonicalize().map_err(fault)?;
        let relative = recorded.strip_prefix(&root).map_err(fault)?;
        let parts = relative.components().map(|part| match part {
            std::path::Component::Normal(value) => value.to_str().ok_or_else(|| fault("invalid WebView image path")),
            _ => Err(fault("invalid WebView image path")),
        }).collect::<Result<Vec<_>>>()?;
        let capture = format!("capture-{}", request.as_str());
        let request_hash = sha256_bytes_hex(request.as_str().as_bytes());
        let screen = format!("request-{}-screen", &request_hash[..32]);
        let basis = format!("capture-request-{}-basis", &request_hash[..32]);
        let valid = if android { match parts.as_slice() {
            ["requests", name, "capture", "screen.png"] => *name == request.as_str(),
            ["page", name, "screen.png"] => *name == screen,
            ["target", "contact-driver", name, "screen.png"] => *name == basis,
            ["target", "host-restarts", epoch, "contact-driver", name, "screen.png"] =>
                RequestId::new(*epoch).is_ok() && *name == basis,
            _ => false,
        }} else { match parts.as_slice() {
            ["target", "webview" | "host", name, "screen.png"] => *name == capture,
            ["host-epochs", epoch, "host", name, "screen.png"] =>
                RequestId::new(*epoch).is_ok() && *name == capture,
            ["epochs", epoch, name, "screen.png"] =>
                RequestId::new(*epoch).is_ok() && *name == capture,
            _ => false,
        }
        };
        if !valid { return Err(fault("WebView capture belongs to another request or producer")); }
        let mut path = root;
        for part in parts {
            path.push(part);
            if std::fs::symlink_metadata(&path).map_err(fault)?.file_type().is_symlink() { return Err(fault("linked WebView image path")); }
        }
        if path.canonicalize().map_err(fault)? != recorded { return Err(fault("WebView capture belongs to another request")); }
        let meta = std::fs::symlink_metadata(&path).map_err(fault)?;
        if !meta.is_file() || meta.len()!=size || size>16*1024*1024 { return Err(fault("invalid WebView image size/type")); }
        let bytes = std::fs::read(path).map_err(fault)?;
        if bytes.len() as u64 != size || sha256_bytes_hex(&bytes)!=digest || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") { return Err(fault("WebView image differs from completed result")); }
        Ok(bytes)
    }
    fn native_view_png(&self, launch: &LaunchId, request: &RequestId, capture: &frontcap_core::native::android_capture::Capture) -> Result<Vec<u8>> {
        capture.validate().map_err(fault)?;
        let artifact = capture.artifacts.iter().find(|a| a.path == "screen.png").ok_or_else(||fault("missing Android View screen"))?;
        let root = self.config.workspace.runtime_dir(launch);
        let mut path = root.clone();
        let epoch = frontcap_recorder::native::android_view_observation_directory(&root,request).map_err(fault)?;
        let image = epoch.join("views").join(request.as_str()).join("collection/capture/screen.png");
        let relative = image.strip_prefix(&root).map_err(fault)?;
        for part in relative.components() {
            path.push(part);
            if std::fs::symlink_metadata(&path).map_err(fault)?.file_type().is_symlink() {return Err(fault("linked Android View image path"));}
        }
        let metadata=std::fs::symlink_metadata(&path).map_err(fault)?;
        if !metadata.is_file() || metadata.len()!=artifact.bytes || metadata.len()>16*1024*1024 {return Err(fault("invalid Android View image size/type"));}
        let bytes=std::fs::read(path).map_err(fault)?;
        if bytes.len() as u64 != artifact.bytes || sha256_bytes_hex(&bytes)!=artifact.sha256 {return Err(fault("Android View screen changed"));}
        Ok(bytes)
    }
    fn native_flutter_png(
        &self,
        launch_id: &LaunchId,
        request_id: &RequestId,
        completion: &frontcap_core::native::flutter::Completion,
    ) -> Result<Vec<u8>> {
        let Some(frontcap_core::native::flutter::Capture::Complete { png_base64, .. }) =
            &completion.capture
        else {
            return Err(fault("Flutter capture is partial or absent"));
        };
        let expected = frontcap_core::encode::unbase64(png_base64)
            .ok_or_else(|| fault("invalid Flutter PNG encoding"))?;
        let path = frontcap_recorder::native::session_directory(&self.config.workspace.runtime_dir(launch_id)).map_err(fault)?
            .join("captures")
            .join(format!("{request_id}.png"));
        let meta = std::fs::symlink_metadata(&path).map_err(fault)?;
        if !meta.is_file() || meta.len() != expected.len() as u64 || meta.len() > 8 * 1024 * 1024 {
            return Err(fault("invalid Flutter capture artifact"));
        }
        let data = std::fs::read(path).map_err(fault)?;
        if data != expected {
            return Err(fault("Flutter capture bytes differ from completed result"));
        }
        Ok(data)
    }
    fn native_host_system_png(&self, id: &LaunchId, client: &std::path::Path, outcome: &UiOutcome) -> Result<Vec<u8>> {
        let UiOutcome::SystemAlertsObserved { artifacts, catalog } = outcome else { return Err(fault("not a system observation")); };
        catalog.validate().map_err(fault)?;
        if !frontcap_core::native::ios_ui::valid_observation_artifacts(artifacts)
            || artifacts[0].path != format!("observations/{}/screen.png",catalog.observation) {
            return Err(fault("invalid system observation artifacts"));
        }
        let root = frontcap_recorder::native::session_directory(&self.config.workspace.runtime_dir(id)).map_err(fault)?;
        if std::fs::symlink_metadata(&root).map_err(fault)?.file_type().is_symlink() { return Err(fault("linked native session")); }
        let root = root.canonicalize().map_err(fault)?;
        let parts = client.strip_prefix(&root).map_err(fault)?.components().map(|part| match part {
            std::path::Component::Normal(value) => value.to_str().ok_or_else(|| fault("invalid system image root")),
            _ => Err(fault("invalid system image root")),
        }).collect::<Result<Vec<_>>>()?;
        let valid = match parts.as_slice() {
            ["target","host","system-ui","client"] => true,
            ["host-epochs",epoch,"host","system-ui","client"] => RequestId::new(*epoch).is_ok(),
            _ => false,
        };
        if !valid { return Err(fault("system image belongs to another producer")); }
        let artifact = &artifacts[0];
        let recorded = client.join(&artifact.path);
        let mut path = root.clone();
        for part in recorded.strip_prefix(&root).map_err(fault)?.components() {
            if !matches!(part,std::path::Component::Normal(_)) { return Err(fault("invalid system image path")); }
            path.push(part);
            if std::fs::symlink_metadata(&path).map_err(fault)?.file_type().is_symlink() { return Err(fault("linked system image")); }
        }
        let metadata = std::fs::metadata(&path).map_err(fault)?;
        if !metadata.is_file() || metadata.len()!=artifact.bytes || metadata.len()>16*1024*1024 { return Err(fault("invalid system image size")); }
        let bytes=std::fs::read(path).map_err(fault)?;
        if sha256_bytes_hex(&bytes)!=artifact.sha256 || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") { return Err(fault("system image differs from receipt")); }
        Ok(bytes)
    }
    fn native_ios_png(&self, id: &LaunchId, request: &RequestId, outcome: &UiOutcome) -> Result<Vec<u8>> {
        let (artifacts, capture_id) = match outcome {

            UiOutcome::ObservedTargets { artifacts, catalog, .. } => (artifacts, &catalog.observation),
            UiOutcome::SystemAlertsObserved { artifacts, catalog } => (artifacts, &catalog.observation),
            _ => return Err(fault("not an iOS observation")),
        };
        if !frontcap_core::native::ios_ui::valid_observation_artifacts(artifacts)
            || artifacts[0].path != format!("observations/{capture_id}/screen.png")
        {
            return Err(fault("invalid iOS PNG inventory"));
        }
        let artifact = &artifacts[0];
        let runtime_dir = self.config.workspace.runtime_dir(id);
        let reader = std::env::var_os("FRONTCAP_NATIVE_ARTIFACT_READER").map(std::path::PathBuf::from);
        if let Some(png) = frontcap_recorder::native::retained_ios_png(&runtime_dir, id, request, outcome,
            &self.config.workspace.root.join("native-artifact-exports").join(id.as_str()), reader.as_deref()).map_err(fault)? {
            return Ok(png);
        }
        let spec: Option<frontcap_recorder::native::LaunchSpec> =
            load(&frontcap_recorder::native::spec_path(&runtime_dir)).map_err(fault)?;
        let flutter = matches!(spec.map(|s| s.target), Some(frontcap_recorder::native::target::Launch::Flutter(_)));
        if flutter && !matches!(outcome, UiOutcome::SystemAlertsObserved { .. }) {
            return Err(fault("Flutter does not publish native app-clock observations"));
        }
        let root = if flutter { frontcap_recorder::native::session_directory(&runtime_dir).map_err(fault)?.join("session/worker/system-ui/client") }
            else { frontcap_recorder::native::ios_observation_directory(&runtime_dir,request).map_err(fault)? };
        let path = root.join(&artifact.path);
        for directory in [&root, &root.join("observations"), path.parent().unwrap()] {
            if std::fs::symlink_metadata(directory)
                .map_err(fault)?
                .file_type()
                .is_symlink()
            {
                return Err(fault("linked iOS image directory"));
            }
        }
        let metadata = std::fs::symlink_metadata(&path).map_err(fault)?;
        if !metadata.is_file()
            || metadata.len() != artifact.bytes
            || metadata.len() > 16 * 1024 * 1024
        {
            return Err(fault("invalid iOS PNG size/type"));
        }
        let png = std::fs::read(path).map_err(fault)?;
        if png.len() as u64 != artifact.bytes || sha256_bytes_hex(&png) != artifact.sha256 {
            return Err(fault("iOS PNG differs from observed evidence"));
        }
        Ok(png)
    }
}

#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;

#[cfg(test)]
#[path="native_observation_tests.rs"]
mod observation_tests;
