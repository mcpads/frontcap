//! Native calls share recorder leases and request-result lookup, without fabricated DOM state.
use crate::LeaseToken;
use frontcap_core::{
    ids::{LaunchId, RequestId, RunId},
    native::{
        ownership::Retirement,
        request::{Completion, Operation},
    },
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Op {
    BeginFlutterEntry { generation: String, url: String },
    PollFlutterEntry { entry_request_id: RequestId, generation: String },
    RestartFlutter { generation: String, #[serde(default, skip_serializing_if = "Option::is_none")] url: Option<String> },
    SetAppState { generation: String, state: frontcap_core::native::app_state::State },
    RestartUikit { generation: String, #[serde(default, skip_serializing_if = "Option::is_none")] url: Option<String> },
    RestartAndroidViews { generation: String, #[serde(default, skip_serializing_if = "Option::is_none")] url: Option<String> },
    DispatchUrl { request: frontcap_core::native::external_entry::Request },
    ExecuteIosHost { operation: frontcap_core::native::ios_host::Operation },
    ExecuteAndroidHost { operation: frontcap_core::native::android_host::Operation },
    ExecuteAndroidWebview { operation: frontcap_core::native::android_webview::Operation },
    CompareWebviewProgress { before: RequestId, after: RequestId, before_capture: Option<RequestId>, after_capture: Option<RequestId> },
    InspectService,
    ReleaseService {
        generation: LaunchId,
        request: RequestId,
        response: frontcap_core::native::service::ResponseSpec,
    },
    ReleaseAsset {
        generation: LaunchId,
        request: RequestId,
        sha256: String,
    },
    /// The caller request ID also names the immutable capture.
    Observe,
    Execute {
        operation: Operation,
    },
    ExecuteIos {
        operation: frontcap_core::native::ios_request::Operation,
    },
    ExecuteWebview { operation: frontcap_core::native::webview::Operation },
    ExecuteFlutter {
        generation: String,
        operation: frontcap_core::native::flutter::Operation,
    },
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    FlutterEntry,
    FlutterRestart,
    UikitRestart,
    UikitEntry,
    AppLifecycle,
    AndroidViewsRestart,
    ExternalUrlDispatch,
    ObservedSystemAlerts,
    WebviewClock,
    WebviewRestart,
    IosHostRestart,
    IosHostUrlDispatch,
    IosHostColdEntry,
    IosHostLifecycle,
    AndroidHostRestart,
    AndroidHostVisibility,
    AndroidHostUrlDispatch,
    AndroidHostEntry,
    AndroidSpeechObservation,
    ObservedWebviewContacts,
    WebviewLifecycle,
    WebviewProgress,
    WebviewComparison,
    NativeRasterCapture,
    ObservedNativeContacts,
    WebviewAttachment,
    ReconcileWebview,
    ControlledService,
    ControlledResources,
    Observe,
    ReadClock,
    Advance,
    ObservedUikitTap,
    ObservedUikitPress,
    ObservedUikitText,
    ObservedViewText,
    ObservedViewTap,
    ObservedViewScroll,
    ObservedUikitScroll,
    ObservedSemanticTap,
    ObservedSemanticScroll,
    ObservedCommittedText,
    FrameworkRasterCapture,
    Hold,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub enum Ready {
    FlutterEntering { entry: frontcap_core::native::flutter_entry::Pending, system_ui: frontcap_core::native::ios_ui::UiIdentity,
        #[serde(default)] app_lifecycle: bool },
    IosHost { owner: frontcap_core::native::ios_host::Owner, contacts: bool, #[serde(default)] pages: bool, #[serde(default, skip_serializing_if = "Option::is_none")] system_ui: Option<frontcap_core::native::ios_ui::UiIdentity>, #[serde(default)] cold_entry: bool, #[serde(default, skip_serializing_if = "Option::is_none")] entry: Option<frontcap_core::native::ios_host::Entry>, #[serde(default)] app_lifecycle: bool, #[serde(default, skip_serializing_if = "Option::is_none")] visibility: Option<frontcap_core::native::app_state::State> },
    AndroidHost { owner: frontcap_core::native::android_webview::AppProcess, contacts: bool },
    AndroidWebview { owner: frontcap_core::native::android_webview::Debugger, contacts: bool },
    Webview { owner: frontcap_core::native::webview::Owner, #[serde(default)] contacts: bool, #[serde(default)] lifecycle: bool },
    AndroidViews { owner: frontcap_core::native::android_app::Owner, #[serde(default)] restart: bool,
        #[serde(default)] app_lifecycle: bool },
    Flutter {
        owner: frontcap_core::native::flutter::Owner,
        #[serde(default)]
        restart: bool,
        #[serde(default)]
        app_lifecycle: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external_url_target: Option<frontcap_core::native::external_entry::Target>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        system_ui: Option<frontcap_core::native::ios_ui::UiIdentity>,
    },
    Ios {
        owner: frontcap_core::native::ios_request::Owner,
        #[serde(default)]
        cold_entry: bool,
        #[serde(default)]
        app_lifecycle: bool,
        #[serde(default)]
        restart: bool,
        #[serde(default)]
        system_alerts: bool,
        /// Configured UIKit bundle eligible for installed-scheme admission. Legacy owners omit it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external_url_bundle: Option<String>,
    },
}
impl Ready {
    /// Compute a visibility change only from a validated live original runner completion.
    pub fn visibility_transition(&self, outcome: &Outcome) -> Result<Option<Self>, frontcap_core::native::request::InvalidRequest> {
        use frontcap_core::native::{ios_host::{Operation, Completion}, request::InvalidRequest};
        let Outcome::IosHostCompleted {completion:Completion::StateChanged {transition}} = outcome else {return Ok(None);};
        let state=transition.state()?;
        self.validate_operation(&Op::ExecuteIosHost {operation:Operation::SetAppState {owner:Box::new(transition.owner.clone()),state}})?;
        let Self::IosHost {system_ui:Some(runner),..} = self else {return Err(InvalidRequest("missing host visibility runner"));};
        transition.completion.validate_system(runner,&transition.completion.request)?;
        if !matches!(&transition.completion.request.operation,frontcap_core::native::ios_ui::UiOperation::SetHostAppState {pid,state:requested,..}
            if *pid==transition.owner.process.pid && *requested==state) {return Err(InvalidRequest("host visibility request differs"));}
        let mut next=self.clone();let Self::IosHost {visibility,..}=&mut next else {unreachable!()};*visibility=Some(state);
        Ok(Some(next))
    }
    /// Compute the next public phase only from a validated live begin/poll completion.
    pub fn entry_transition(&self, outcome: &Outcome)
        -> Result<Option<Self>, frontcap_core::native::request::InvalidRequest> {
        use frontcap_core::native::{flutter_entry::Progress, request::InvalidRequest};
        match outcome {
            Outcome::IosHostCompleted {completion:frontcap_core::native::ios_host::Completion::EntryBegun {entry}} => {
                entry.validate()?;
                self.validate_operation(&Op::ExecuteIosHost {operation:frontcap_core::native::ios_host::Operation::BeginEntry {
                    previous:entry.acceptance.previous.clone(),url:entry.acceptance.url.clone()}})?;
                let mut next=self.clone();
                let Self::IosHost {entry:pending,..}=&mut next else {return Err(InvalidRequest("entry returned for another target"));};
                *pending=Some(*entry.clone());Ok(Some(next))
            }
            Outcome::IosHostCompleted {completion:frontcap_core::native::ios_host::Completion::EntryPolled {begin,ready}} => {
                self.validate_operation(&Op::ExecuteIosHost {operation:frontcap_core::native::ios_host::Operation::PollEntry {begin:begin.clone()}})?;
                let mut next=self.clone();
                let Self::IosHost {owner,contacts,pages,system_ui,entry,app_lifecycle,visibility,..}=&mut next else {return Err(InvalidRequest("entry poll returned for another target"));};
                if let Some(ready)=ready {
                    ready.owner.validate()?;
                    if *owner==ready.owner || owner.simulator!=ready.owner.simulator || owner.attachment_sha256!=ready.owner.attachment_sha256
                        || *app_lifecycle!=ready.app_lifecycle || *contacts!=ready.contacts.is_some() || *pages!=ready.pages || system_ui.is_none() || ready.system_ui.is_none() || *system_ui==ready.system_ui {
                        return Err(InvalidRequest("cold replacement differs from current owner"));
                    }
                    if let Some(endpoint)=&ready.contacts {endpoint.validate(ready.owner.process.pid)?;}
                    let runner=ready.system_ui.as_ref().unwrap();runner.validate()?;
                    if runner.simulator!=ready.owner.simulator || runner.pid==ready.owner.process.pid {return Err(InvalidRequest("cold replacement runner differs"));}
                    *owner=ready.owner.clone();*system_ui=ready.system_ui.clone();*entry=None;*visibility=None;
                }
                Ok(Some(next))
            }
            Outcome::FlutterEntryStarted { entry } => {
                entry.validate()?;
                self.validate_operation(&Op::BeginFlutterEntry {
                    generation: entry.previous.generation.clone(), url: entry.acceptance.request.url.clone(),
                })?;
                let Self::Flutter { owner, external_url_target, system_ui: Some(system_ui), app_lifecycle, .. } = self else {
                    return Err(InvalidRequest("cold entry returned for another platform"));
                };
                if owner != &entry.previous || external_url_target.as_ref() != Some(&entry.acceptance.request.target) {
                    return Err(InvalidRequest("cold entry completion differs from current owner"));
                }
                Ok(Some(Self::FlutterEntering { entry: entry.clone(), system_ui: system_ui.clone(), app_lifecycle: *app_lifecycle }))
            }
            Outcome::FlutterEntryPolled { progress } => {
                progress.validate()?;
                let Self::FlutterEntering { entry, system_ui, app_lifecycle } = self else { return Err(InvalidRequest("cold readiness has no pending entry")); };
                if progress.entry() != entry { return Err(InvalidRequest("cold readiness differs from pending entry")); }
                Ok(Some(match progress {
                    Progress::Waiting { .. } => self.clone(),
                    Progress::Ready { replacement, .. } => Self::Flutter {
                        owner: replacement.owner().clone(), restart: true, app_lifecycle: *app_lifecycle,
                        external_url_target: Some(entry.acceptance.request.target.clone()), system_ui: Some(system_ui.clone()),
                    },
                }))
            }
            _ => Ok(None),
        }
    }
    pub fn validate_operation(
        &self,
        op: &Op,
    ) -> Result<(), frontcap_core::native::request::InvalidRequest> {
        use frontcap_core::native::{
            ios_request::{Operation as IosOperation, Request},
            request::InvalidRequest,
        };
        if let Self::AndroidViews { owner, .. } = self { owner.validate()?; }
        if let Self::Webview { owner, .. } = self { owner.validate()?; }
        if let Self::IosHost { owner, system_ui, .. } = self {
            owner.validate()?;
            if let Some(runner) = system_ui {
                runner.validate()?;
                if !runner.simulator.eq_ignore_ascii_case(&owner.simulator) || runner.pid == owner.process.pid {
                    return Err(InvalidRequest("system runner differs from native host"));
                }
            }
        }
        if let Self::IosHost {owner,contacts,system_ui,cold_entry,entry:Some(entry),..}=self {
            entry.validate()?;
            if !cold_entry || !contacts || system_ui.is_none() || &entry.acceptance.previous!=owner {return Err(InvalidRequest("invalid pending native entry"));}
            return match op {
                Op::ExecuteIosHost {operation} if operation.system_operation().is_some()
                    || matches!(operation,frontcap_core::native::ios_host::Operation::PollEntry {begin} if begin==&entry.begin) => operation.validate(),
                Op::Stop | Op::InspectService => Ok(()),
                _=>Err(InvalidRequest("pending iOS entry permits original poll, system UI or retirement")),
            };
        }
        if let Self::IosHost {visibility:Some(frontcap_core::native::app_state::State::Background),..}=self {
            let allowed=matches!(op,Op::Stop | Op::InspectService)
                || matches!(op,Op::ExecuteIosHost {operation} if operation.system_operation().is_some()
                    || matches!(operation,frontcap_core::native::ios_host::Operation::SetAppState {state:frontcap_core::native::app_state::State::Foreground,..}));
            if !allowed {return Err(InvalidRequest("background host permits foreground, system UI or retirement"));}
        }
        if let Self::FlutterEntering { entry, system_ui, .. } = self {
            entry.validate()?;
            system_ui.validate()?;
            if !entry.previous.device.eq_ignore_ascii_case(&system_ui.simulator) {
                return Err(InvalidRequest("pending entry system runner differs"));
            }
            return match op {
                Op::PollFlutterEntry { entry_request_id, generation }
                    if entry_request_id == &entry.request_id && generation == &entry.previous.generation => Ok(()),
                Op::ExecuteIos { operation } => frontcap_core::native::ios_ui::UiRequest {
                    generation: system_ui.generation.clone(), request_id: 1,
                    operation: frontcap_core::native::ios_system_ui::request_operation(operation)?,
                }.validate(system_ui),
                Op::Stop | Op::InspectService => Ok(()),
                _ => Err(InvalidRequest("pending Flutter entry permits original readiness, system UI or retirement")),
            };
        }
        match (self, op) {
            (Self::Flutter { owner, restart: true, external_url_target, system_ui: Some(runner), .. }, Op::BeginFlutterEntry { generation, url }) => {
                if generation != &owner.generation { return Err(InvalidRequest("cold entry generation differs")); }
                frontcap_core::native::flutter_lifecycle::ios_entry(owner, external_url_target.as_ref(), url)?;
                runner.validate()?;
                if !owner.device.eq_ignore_ascii_case(&runner.simulator) { return Err(InvalidRequest("cold entry system runner differs")); }
                Ok(())
            }
            (Self::AndroidViews {owner, restart: true, ..}, Op::RestartAndroidViews {generation, url}) => {
                if generation != &owner.generation {return Err(InvalidRequest("Android restart generation differs"));}
                frontcap_core::native::android_lifecycle::restart_entry(owner, url.as_deref())?;
                Ok(())
            }
            (Self::AndroidViews { owner, app_lifecycle: true, .. }, Op::SetAppState { generation, .. }) => {
                if generation != &owner.generation { return Err(InvalidRequest("Android visibility generation differs")); }
                Ok(())
            }
            (Self::Flutter { owner, app_lifecycle: true, .. }, Op::SetAppState { generation, .. }) => {
                owner.validate()?;
                if generation != &owner.generation { return Err(InvalidRequest("Flutter visibility generation differs")); }
                Ok(())
            }
            (Self::Ios { owner, app_lifecycle: true, .. }, Op::SetAppState {generation,state}) => {
                Request::build_pair(owner,&IosOperation::SetAppState {generation:generation.clone(),state:*state},1,1).map(|_|())
            },
            (Self::Ios { owner, restart: true, cold_entry, external_url_bundle: Some(bundle), .. }, Op::RestartUikit { generation, url }) => {
                owner.validate()?;
                if generation != &owner.app.generation { return Err(InvalidRequest("UIKit restart generation differs")); }
                if url.is_some() && !cold_entry { return Err(InvalidRequest("UIKit URL launch unavailable")); }
                frontcap_core::native::ios_lifecycle::restart_entry(owner, bundle, url.as_deref())?;
                Ok(())
            }
            (Self::Flutter { owner, restart: true, external_url_target, .. }, Op::RestartFlutter { generation, url }) => {
                owner.validate()?;
                if generation != &owner.generation {
                    return Err(InvalidRequest("Flutter restart differs from current standalone owner"));
                }
                frontcap_core::native::flutter_lifecycle::restart_entry(owner, external_url_target.as_ref(), url.as_deref())?;
                Ok(())
            }
            (Self::Flutter { owner, external_url_target: Some(target), .. }, Op::DispatchUrl { request }) => {
                owner.validate_external_target(target)?;
                request.validate()?;
                if &request.target != target || request.generation != owner.generation {
                    return Err(InvalidRequest("external URL target differs from current Flutter owner"));
                }
                Ok(())
            }
            (Self::Flutter { owner, system_ui: Some(runner), .. }, Op::ExecuteIos { operation }) => {
                owner.validate()?;
                runner.validate()?;
                if owner.platform != frontcap_core::native::flutter::Platform::Ios
                    || !owner.device.eq_ignore_ascii_case(&runner.simulator) {
                    return Err(InvalidRequest("system runner differs from Flutter host"));
                }
                frontcap_core::native::ios_ui::UiRequest { generation: runner.generation.clone(), request_id: 1,
                    operation: frontcap_core::native::ios_system_ui::request_operation(operation)? }.validate(runner)
            }
            (Self::AndroidViews { owner, .. }, Op::DispatchUrl { request }) => {
                request.validate()?;
                if request.generation != owner.generation || request.target !=
                    (frontcap_core::native::external_entry::Target::Android {
                        serial: owner.serial.clone(), package: owner.package.clone(),
                    })
                {
                    return Err(InvalidRequest("external URL target differs from current owner"));
                }
                Ok(())
            }
            (Self::Ios { owner, external_url_bundle: Some(bundle), .. }, Op::DispatchUrl { request }) => {
                owner.validate()?;
                request.validate()?;
                if request.generation != owner.app.generation || request.target !=
                    (frontcap_core::native::external_entry::Target::Ios {
                        simulator: owner.app.simulator.to_ascii_lowercase(), bundle: bundle.clone(),
                    })
                {
                    return Err(InvalidRequest("external URL target differs from current UIKit owner"));
                }
                Ok(())
            }
            (Self::IosHost { owner, contacts, pages, system_ui, cold_entry, app_lifecycle, visibility, .. }, Op::ExecuteIosHost { operation }) => {
                if let frontcap_core::native::ios_host::Operation::SetAppState {owner:requested,state}=operation
                    && (!app_lifecycle || system_ui.is_none() || requested.as_ref()!=owner || *visibility==Some(*state)) {return Err(InvalidRequest("host visibility unavailable, repeated or owner differs"));}
                if let frontcap_core::native::ios_host::Operation::BeginEntry {previous,..}=operation
                    && (!cold_entry || !contacts || system_ui.is_none() || previous!=owner) {return Err(InvalidRequest("cold iOS entry unavailable or owner differs"));}
                if matches!(operation,frontcap_core::native::ios_host::Operation::PollEntry {..}) {return Err(InvalidRequest("no pending iOS entry"));}
                if operation.system_operation().is_some() && system_ui.is_none() { return Err(InvalidRequest("system runner unavailable")); }
                if matches!(operation, frontcap_core::native::ios_host::Operation::DispatchUrl { owner: requested, .. } if requested.as_ref() != owner) {
                    return Err(InvalidRequest("native host URL owner differs"));
                }
                if matches!(operation, frontcap_core::native::ios_host::Operation::Restart {previous} if previous != owner) {
                    return Err(InvalidRequest("native host restart owner differs"));
                }
                if !contacts && operation.requires_contacts() { return Err(InvalidRequest("native host input producer unavailable")); }
                if !pages && operation.requires_pages() {return Err(InvalidRequest("native host page registry unavailable"));}
                operation.validate()
            }
            (Self::AndroidHost { owner, contacts }, Op::ExecuteAndroidHost { operation }) => {
                if matches!(operation, frontcap_core::native::android_host::Operation::SetAppState {owner:requested,..} if requested.as_ref()!=owner) {
                    return Err(InvalidRequest("native host visibility owner differs"));
                }
                if matches!(operation, frontcap_core::native::android_host::Operation::Restart { previous, .. } if previous.as_ref() != owner) {
                    return Err(InvalidRequest("native host restart owner differs"));
                }
                if matches!(operation, frontcap_core::native::android_host::Operation::DispatchUrl { owner: requested, .. } if requested.as_ref() != owner) {
                    return Err(InvalidRequest("native host URL owner differs"));
                }
                if !contacts && operation.requires_contacts() { return Err(InvalidRequest("native host input producer unavailable")); }
                operation.validate().map_err(|_| InvalidRequest("invalid native host operation"))
            }
            (Self::AndroidWebview { contacts, .. }, Op::ExecuteAndroidWebview { operation }) => {
                use frontcap_core::native::android_webview::Operation as A;
                if matches!(operation, A::Host { .. }) { return Err(InvalidRequest("native host operation requires host runtime")); }
                if !contacts && matches!(operation, A::Contacts { .. } | A::ReconcileContacts { .. } | A::Observe { input_basis: true }) {
                    return Err(InvalidRequest("Android WebView input producer unavailable"));
                }
                operation.validate().map_err(|_| InvalidRequest("invalid Android WebView operation"))
            }
            (Self::Webview { .. } | Self::AndroidWebview { .. }, Op::CompareWebviewProgress { .. }) => Ok(()),
            (Self::Webview { owner, contacts, lifecycle }, Op::ExecuteWebview { operation }) => {
                match operation {
                    frontcap_core::native::webview::Operation::Host { .. } => return Err(InvalidRequest("host operation requires host runtime")),
                    frontcap_core::native::webview::Operation::Restart { generation } if generation != &owner.endpoint.generation =>
                        return Err(InvalidRequest("restart generation differs from current owner")),
                    frontcap_core::native::webview::Operation::Background
                    | frontcap_core::native::webview::Operation::Foreground
                    | frontcap_core::native::webview::Operation::ReconcileLifecycle { .. } if !lifecycle =>
                        return Err(InvalidRequest("WebView lifecycle unavailable")),
                    frontcap_core::native::webview::Operation::Contacts { input, .. } => {
                        if !contacts { return Err(InvalidRequest("observed WebView contacts unavailable")); }
                        input.validate()?;
                    }
                    frontcap_core::native::webview::Operation::ReconcileContacts { .. } if !contacts => return Err(InvalidRequest("observed WebView contacts unavailable")),
                    _ => {}
                }
                if let frontcap_core::native::webview::Operation::Clock { kind, delta_ns } = operation {
                    owner.endpoint.validate(&frontcap_core::native::request::ClockRequest {
                        generation: owner.endpoint.generation.clone(), request_id: 1, operation: *kind, delta_ns: *delta_ns })?;
                }
                Ok(())
            }
            (_, Op::Observe | Op::Stop | Op::InspectService | Op::ReleaseService { .. } | Op::ReleaseAsset { .. }) => Ok(()),
            (Self::AndroidViews { owner, .. }, Op::Execute { operation }) => operation.validate(&owner.identity()),
            (Self::Ios { owner, system_alerts, .. }, Op::ExecuteIos { operation }) => {
                if !system_alerts && matches!(operation, IosOperation::ObserveSystemAlerts | IosOperation::TapSystemButton { .. }) {
                    return Err(InvalidRequest("system alert operations are not advertised"));
                }
                // Dedicated operations retain retirement or host-process evidence.
                if matches!(operation, IosOperation::Finish | IosOperation::SetAppState {..}) {
                    return Err(InvalidRequest("use Stop for retirement or SetAppState for host transitions"));
                }
                Request::build_pair(owner, operation, 1, 1).map(|_| ())
            }
            (
                Self::Flutter { owner, .. },
                Op::ExecuteFlutter {
                    generation,
                    operation,
                },
            ) => {
                owner.validate()?;
                if generation != &owner.generation {
                    return Err(InvalidRequest(
                        "Flutter generation differs from observation owner",
                    ));
                }
                if matches!(
                    operation,
                    frontcap_core::native::flutter::Operation::Stop {}
                ) {
                    return Err(InvalidRequest("use Stop for retirement or SetAppState for host transitions"));
                }
                operation.validate()
            }
            _ => Err(InvalidRequest(
                "operation belongs to another native platform",
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Status {
    pub launch_id: LaunchId,
    pub run_id: RunId,
    pub ready: Ready,
    pub capabilities: Vec<Capability>,
    pub lease_held: bool,
    pub running: Option<RequestId>,
    pub last_completed_request_id: Option<RequestId>,
    /// A failure fences new work; it does not resolve a dispatched request.
    pub failure: Option<String>,
    pub stopped: bool,
    pub evidence: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    FlutterEntryStarted { entry: frontcap_core::native::flutter_entry::Pending },
    FlutterEntryPolled { progress: frontcap_core::native::flutter_entry::Progress },
    AndroidViewsRestarted { replacement: frontcap_core::native::android_lifecycle::Replacement },
    UikitStateChanged { transition: frontcap_core::native::ios_lifecycle::Transition },
    AndroidViewsStateChanged { transition: frontcap_core::native::android_lifecycle::Transition },
    FlutterStateChanged { transition: frontcap_core::native::flutter_lifecycle::Transition },
    UikitRestarted { replacement: Box<frontcap_core::native::ios_lifecycle::Replacement> },
    FlutterRestarted { replacement: frontcap_core::native::flutter_lifecycle::Replacement },
    ExternalUrlAccepted { acceptance: frontcap_core::native::external_entry::Accepted },
    IosHostCompleted { completion: frontcap_core::native::ios_host::Completion },
    IosHostUnresolved { error: String },
    AndroidHostCompleted { completion: frontcap_core::native::android_host::Completion },
    AndroidHostUnresolved { error: String },
    AndroidWebviewCompleted { completion: frontcap_core::native::android_webview::Completion },
    AndroidWebviewUnresolved {
        error: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lifecycle: Option<frontcap_core::native::android_progress::Lifecycle>,
    },
    AndroidWebviewCompared { comparison: frontcap_core::native::webview_progress::Comparison<frontcap_core::native::android_webview::Debugger> },
    WebviewCompared { comparison: frontcap_core::native::webview_progress::Comparison },
    WebviewCompleted { completion: Box<frontcap_core::native::webview::Completion> },
    WebviewUnresolved { error: String },
    Service { reply: frontcap_core::native::service::Reply },
    AndroidViewsUnavailable { observation: frontcap_core::native::android_capture::Unavailable },
    AndroidViewsObserved { capture: frontcap_core::native::android_capture::Capture },
    FlutterCompleted {
        completion: frontcap_core::native::flutter::Completion,
    },
    Completed {
        completion: Completion,
    },
    IosCompleted {
        completion: frontcap_core::native::ios_request::Completion,
    },
    Stopped {
        retirement: Retirement,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reply {
    Status {
        status: Box<Status>,
    },
    Attached {
        lease: LeaseToken,
        status: Box<Status>,
        completed_while_detached: Vec<RequestId>,
    },
    Done {
        request_id: RequestId,
        outcome: Box<Outcome>,
    },
}

#[cfg(test)]
#[path = "native_tests.rs"]
mod tests;

#[cfg(test)]
#[path="native_view_tests.rs"]
mod view_tests;

#[cfg(test)]
#[path="native_host_entry_tests.rs"]
mod host_entry_tests;
