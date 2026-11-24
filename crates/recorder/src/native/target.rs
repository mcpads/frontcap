//! Platform-specific ownership behind the shared native recorder journal and leases.
use frontcap_core::{
    ids::RequestId,
    native::ownership::Retirement,
};
use frontcap_native::{device::AndroidLaunch, ios_session, ios_target::IosLaunch, session};
use frontcap_recorder_protocol::native::{Op, Outcome, Ready};
use serde::{Deserialize, Serialize};
use std::{io, path::Path, sync::atomic::AtomicBool, time::Instant};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(try_from = "LaunchWire", into = "LaunchWire")]
pub enum Launch {
    IosHost(frontcap_native::ios_host::Config),
    AndroidHost(frontcap_native::android_webview_session::Config),
    AndroidWebview(frontcap_native::android_webview_session::Config),
    Webview(Box<frontcap_native::webview_session::Config>),
    Android(AndroidLaunch),
    Ios(IosLaunch),
    Flutter(frontcap_native::flutter_target::Launch),
}
// Preserve legacy native profiles; Flutter profiles explicitly separate runtime and host platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum LaunchWire {
    IosHost(IosHostLaunchWire),
    AndroidHost(AndroidHostLaunchWire),
    AndroidWebview(AndroidWebviewLaunchWire),
    Webview(WebviewLaunchWire),
    Flutter(Box<FlutterLaunchWire>),
    Native(Box<NativeLaunchWire>),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(
    tag = "platform",
    content = "config",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum NativeLaunchWire {
    Android(AndroidLaunch),
    Ios(IosLaunch),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FlutterRuntime {
    Flutter,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FlutterLaunchWire {
    runtime: FlutterRuntime,
    platform: frontcap_core::native::flutter::Platform,
    config: frontcap_native::flutter_target::Launch,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebviewRuntime { Webview }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HostRuntime { Host }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct IosHostLaunchWire { runtime: HostRuntime, platform: WebviewPlatform, config: frontcap_native::ios_host::Config }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AndroidHostLaunchWire { runtime: HostRuntime, platform: AndroidWebviewPlatform, config: frontcap_native::android_webview_session::Config }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WebviewPlatform { Ios }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AndroidWebviewPlatform { Android }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AndroidWebviewLaunchWire { runtime: WebviewRuntime, platform: AndroidWebviewPlatform, config: frontcap_native::android_webview_session::Config }
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WebviewLaunchWire { runtime: WebviewRuntime, platform: WebviewPlatform, config: Box<frontcap_native::webview_session::Config> }
impl TryFrom<LaunchWire> for Launch {
    type Error = String;
    fn try_from(value: LaunchWire) -> Result<Self, String> {
        Ok(match value {
            LaunchWire::IosHost(value) => {
                value.config.validate().map_err(|error|error.to_string())?;
                Self::IosHost(value.config)
            },
            LaunchWire::AndroidHost(value) => {
                if value.config.device.host_app.is_none() { return Err("host runtime requires host_app".into()); }
                Self::AndroidHost(value.config)
            },
            LaunchWire::AndroidWebview(value) => Self::AndroidWebview(value.config),
            LaunchWire::Webview(value) => Self::Webview(value.config),
            LaunchWire::Native(value) => match *value {
                NativeLaunchWire::Android(config) => Self::Android(config),
                NativeLaunchWire::Ios(config) => Self::Ios(config),
            },
            LaunchWire::Flutter(value) => {
                if value.platform != value.config.worker.config.platform {
                    return Err("Flutter host platform differs from worker configuration".into());
                }
                Self::Flutter(value.config)
            }
        })
    }
}
impl From<Launch> for LaunchWire {
    fn from(value: Launch) -> Self {
        match value {
            Launch::IosHost(config) => Self::IosHost(IosHostLaunchWire { runtime: HostRuntime::Host, platform: WebviewPlatform::Ios, config }),
            Launch::AndroidHost(config) => Self::AndroidHost(AndroidHostLaunchWire { runtime: HostRuntime::Host, platform: AndroidWebviewPlatform::Android, config }),
            Launch::AndroidWebview(config) => Self::AndroidWebview(AndroidWebviewLaunchWire { runtime: WebviewRuntime::Webview, platform: AndroidWebviewPlatform::Android, config }),
            Launch::Webview(config) => Self::Webview(WebviewLaunchWire { runtime: WebviewRuntime::Webview, platform: WebviewPlatform::Ios, config }),
            Launch::Android(config) => Self::Native(Box::new(NativeLaunchWire::Android(config))),
            Launch::Ios(config) => Self::Native(Box::new(NativeLaunchWire::Ios(config))),
            Launch::Flutter(config) => Self::Flutter(Box::new(FlutterLaunchWire {
                runtime: FlutterRuntime::Flutter,
                platform: config.worker.config.platform.clone(),
                config,
            })),
        }
    }
}
impl Launch {
    pub(super) fn configure_service(&mut self, service: Option<&frontcap_service_fixture::Record>) -> io::Result<()> {
        self.configure_service_endpoint(service, false)
    }
    pub(super) fn configure_resources(&mut self, service: Option<&frontcap_service_fixture::Record>) -> io::Result<()> {
        self.configure_service_endpoint(service, true)
    }
    fn configure_service_endpoint(&mut self, service: Option<&frontcap_service_fixture::Record>, resources: bool) -> io::Result<()> {
        let suffix = |service: &frontcap_service_fixture::Record| if resources { String::new() } else { service.requests_path().trim_start_matches('/').to_owned() };
        if let Self::Android(config) = self
            && let Some(profile) = config.view_app.as_mut() {
            profile.validate()?;
            if let Some(key) = profile.service_url_extra.as_ref() {
                let service = service.ok_or_else(|| io::Error::other("service URL extra requires a controlled service"))?;
                if profile.launch_extras.contains_key(key) || profile.launch_extras.len() >= 16 {
                    return Err(io::Error::other("service URL extra conflicts with app configuration or capacity"));
                }
                profile.launch_extras.insert(key.clone(), format!("http://10.0.2.2:{}/{}", service.address.port(), suffix(service)));
            }
        }
        if let Self::Webview(config) = self
            && let Some(key) = &config.service_url_environment {
            if !frontcap_core::native::ios_ui::valid_app_environment_key(key) || config.app.environment.contains_key(key) || config.app.environment.len() >= 16 {
                return Err(io::Error::other("invalid or conflicting WebView service environment"));
            }
            let service = service.ok_or_else(|| io::Error::other("WebView service environment requires controlled service"))?;
            config.app.environment.insert(key.clone(), format!("http://127.0.0.1:{}/{}", service.address.port(), suffix(service)));
        }
        if let Self::Ios(config) = self {
            config.validate_app_environment()?;
            if let Some(key) = config.service_url_environment.as_ref() {
                let service = service.ok_or_else(|| io::Error::other("service URL environment requires a controlled service"))?;
                if config.app_environment.contains_key(key) || config.app_environment.len() >= 16 {
                    return Err(io::Error::other("service URL environment conflicts with app configuration or capacity"));
                }
                config.app_environment.insert(key.clone(), format!("http://127.0.0.1:{}/{}", service.address.port(), suffix(service)));
            }
        }
        if let Self::Flutter(config) = self
            && let Some(key) = config.service_url_define.as_ref() {
            let service = service.ok_or_else(|| io::Error::other("service URL definition requires a controlled service"))?;
            if config.worker.config.app_defines.contains_key(key) {
                return Err(io::Error::other("service URL definition conflicts with app configuration"));
            }
            let host = match config.worker.config.platform {
                frontcap_core::native::flutter::Platform::Android => "10.0.2.2",
                frontcap_core::native::flutter::Platform::Ios => "127.0.0.1",
            };
            config.worker.config.app_defines.insert(key.clone(), format!("http://{host}:{}/{}", service.address.port(), suffix(service)));
        }
        Ok(())
    }
}

pub(super) enum Session {
    IosHost(frontcap_native::webview_session::Session),
    AndroidHost(frontcap_native::android_webview_session::Session),
    AndroidWebview(frontcap_native::android_webview_session::Session),
    Webview(frontcap_native::webview_session::Session),
    Flutter(Box<frontcap_native::flutter_target::Target>),
    AndroidViews(session::Session),
    Ios(ios_session::Session),
}
pub(super) struct Acquisition {
    pub result: io::Result<(Session, Ready)>,
    pub retirement: Option<Retirement>,
    pub ios_startup_cleanup: ios_session::StartupCleanup,
}
impl Session {
    #[cfg(test)]
    pub(super) fn acquire_with_budget(executable: &Path, config: Launch, directory: &Path,
        deadline: Instant, budget: Option<&frontcap_core::write_budget::WriteBudget>) -> Acquisition {
        Self::acquire_with_observer(executable, config, directory, deadline, budget, None)
    }
    #[cfg(test)]
    pub(super) fn acquire_with_observer(executable: &Path, config: Launch, directory: &Path,
        deadline: Instant, budget: Option<&frontcap_core::write_budget::WriteBudget>,
        before_start: Option<&mut ios_session::BeforeStart<'_>>) -> Acquisition {
        Self::acquire_with_cancellation(executable, config, directory, deadline, budget, before_start, &AtomicBool::new(false))
    }
    pub(super) fn acquire_with_cancellation(executable: &Path, config: Launch, directory: &Path,
        deadline: Instant, budget: Option<&frontcap_core::write_budget::WriteBudget>,
        before_start: Option<&mut ios_session::BeforeStart<'_>>, cancel: &AtomicBool) -> Acquisition {
        let mut cleanup = (None, ios_session::StartupCleanup::Unproven);
        let result = Self::start_with_budget(executable, config, directory, deadline, budget, &mut cleanup, (cancel, before_start));
        Acquisition { result, retirement: cleanup.0, ios_startup_cleanup: cleanup.1 }
    }
    #[cfg(test)]
    pub(super) fn start(
        executable: &Path,
        config: Launch,
        directory: &Path,
        deadline: Instant,
    ) -> io::Result<(Self, Ready)> {
        Self::acquire_with_budget(executable, config, directory, deadline, None).result
    }
    fn start_with_budget(
        executable: &Path, config: Launch, directory: &Path, deadline: Instant,
        budget: Option<&frontcap_core::write_budget::WriteBudget>,
        cleanup: &mut (Option<Retirement>, ios_session::StartupCleanup),
        control: (&AtomicBool, Option<&mut ios_session::BeforeStart<'_>>),
    ) -> io::Result<(Self, Ready)> {
        let (cancel, before_start) = control;
        let (retired, ios_startup_cleanup) = cleanup;
        match config {
            Launch::IosHost(config) => {
                let mut session = frontcap_native::webview_session::Session::start_host_with_budget(executable, config.clone(), directory, deadline, budget)?;
                let ready = session.wait_host_ready(deadline, cancel)?;
                config.validate_ready(&ready)?;
                Ok((Self::IosHost(session), Ready::IosHost {owner:ready.owner, contacts:ready.contacts.is_some(),pages:ready.pages,system_ui:ready.system_ui,cold_entry:config.cold_entry(),entry:None,app_lifecycle:ready.app_lifecycle,visibility:None}))
            }
            Launch::AndroidHost(config) => {
                let mut session = frontcap_native::android_webview_session::Session::start_with_budget(executable, config.clone(), directory, deadline, budget)?;
                let ready = session.wait_host_ready(deadline, cancel)?;
                config.validate_host_ready(&ready)?;
                Ok((Self::AndroidHost(session), Ready::AndroidHost { owner: ready.owner, contacts: ready.contacts.is_some() }))
            }
            Launch::AndroidWebview(config) => {
                let mut session = frontcap_native::android_webview_session::Session::start_with_budget(executable, config.clone(), directory, deadline, budget)?;
                let ready = session.wait_ready(deadline, cancel)?;
                config.validate_ready(&ready)?;
                Ok((Self::AndroidWebview(session), Ready::AndroidWebview { owner: ready.owner, contacts: ready.contacts.is_some() }))
            }
            Launch::Webview(config) => {
                let contacts = config.contacts;
                let lifecycle = config.lifecycle.is_some();
                let mut session = frontcap_native::webview_session::Session::start_with_budget(executable, *config, directory, deadline, budget)?;
                let owner = session.wait_ready(deadline, cancel)?;
                owner.validate().map_err(io::Error::other)?;
                Ok((Self::Webview(session), Ready::Webview { owner, contacts, lifecycle }))
            }
            Launch::Flutter(config) => {
                let (target, owner) = frontcap_native::flutter_target::Target::acquire_with_budget(
                    config, directory, deadline, cancel, budget,
                )?;
                let external_url_target = target.external_url_target().cloned();
                let system_ui = target.system_ui().cloned();
                let restart = target.restart_available();
                let app_lifecycle = target.app_lifecycle();
                Ok((Self::Flutter(Box::new(target)), Ready::Flutter { owner, external_url_target, system_ui, restart, app_lifecycle }))
            }
            Launch::Android(config) => {
                if config.view_app.is_none() { return Err(io::Error::other("Android native launch requires a View profile")); }
                let mut session = session::Session::start_with_budget(executable, config, directory, deadline, budget)?;
                {
                    let owner = session.wait_view_ready(deadline, cancel)?;
                    owner.validate().map_err(io::Error::other)?;
                    let restart = session.view_restart_available();
                    let app_lifecycle = session.view_lifecycle_available()?;
                    return Ok((Self::AndroidViews(session), Ready::AndroidViews { owner, restart, app_lifecycle }));
                }
            }
            Launch::Ios(config) => {
                let external_url_bundle = (config.surface == frontcap_native::ios_execution::UiSurface::Uikit)
                    .then(|| config.xctest.app.bundle.clone());
                let startup = ios_session::Session::start_with_observer(executable, config, directory, deadline, budget, before_start);
                match startup.cleanup {
                    ios_session::StartupCleanup::Retired(retirement) => *retired = Some(retirement),
                    cleanup => *ios_startup_cleanup = cleanup,
                }
                let mut session = startup.result?;
                let readiness = (|| -> io::Result<Ready> {
                    let owner = session.wait_ready(deadline, cancel)?;
                    owner.validate().map_err(io::Error::other)?;
                    let system_alerts = session.system_alerts()?;
                    let restart = session.restart_available()?;
                    let cold_entry = session.entry_available()?;
                    let app_lifecycle = session.app_lifecycle()?;
                    Ok(Ready::Ios { owner, external_url_bundle, system_alerts, restart, cold_entry, app_lifecycle })
                })();
                match readiness {
                    Ok(ready) => Ok((Self::Ios(session), ready)),
                    Err(error) => {
                        // Retain original authority through failed readiness; saved records alone
                        // never grant permission to dispose of a managed session volume.
                        let cleanup = session.stop_startup(Instant::now() + std::time::Duration::from_secs(60));
                        match cleanup {
                            Ok(ios_session::StartupCleanup::Retired(retirement)) => { *retired = Some(retirement); Err(error) }
                            Ok(cleanup) => { *ios_startup_cleanup = cleanup; Err(error) }
                            Err(cleanup) => Err(io::Error::other(format!("iOS acquisition: {error}; target retirement: {cleanup}"))),
                        }
                    }
                }
            }
        }
    }
    pub(super) fn validate_operation(&self, id: &RequestId, op: &Op) -> io::Result<()> {
        if let Self::IosHost(_) = self {
            return match op { Op::Observe | Op::Stop => Ok(()), Op::ExecuteIosHost {operation} => operation.validate().map_err(io::Error::other),
                _ => Err(io::Error::other("operation differs from iOS native host session")) };
        }
        if let Self::AndroidHost(_) = self {
            return match op { Op::Observe | Op::Stop => Ok(()), Op::ExecuteAndroidHost { operation } => operation.validate(),
                _ => Err(io::Error::other("operation differs from native host session")) };
        }
        if let Self::AndroidWebview(_) = self {
            return match op { Op::Observe | Op::Stop => Ok(()), Op::ExecuteAndroidWebview { operation } => operation.validate(),
                _ => Err(io::Error::other("operation differs from Android WebView session")) };
        }
        if let Self::Webview(_) = self {
            return match op { Op::Observe | Op::Stop | Op::ExecuteWebview { .. } => Ok(()),
                _ => Err(io::Error::other("operation differs from WebView session")) };
        }
        if let Self::AndroidViews(session) = self {
            return match op { Op::RestartAndroidViews {generation, url} => session.validate_view_restart(id,generation,url.as_deref()),
                Op::SetAppState {generation,state} => session.validate_view_state(id,generation,*state),
                Op::Observe | Op::DispatchUrl { .. } => session.validate_view_foreground(), Op::Stop => Ok(()), Op::Execute {operation} => session.validate_view_operation(id,operation),
                _=>Err(io::Error::other("operation is not supported by Android Views")) };
        }
        if let Self::Ios(session) = self {
            return match op {
                Op::SetAppState {generation,state} => session.validate_operation(id,&frontcap_core::native::ios_request::Operation::SetAppState {generation:generation.clone(),state:*state}),
                Op::RestartUikit { generation, url } => session.validate_restart(id, generation, url.as_deref()),
                Op::Observe => session.validate_operation(id,&session.surface().observation()),
                Op::Stop => Ok(()),
                Op::DispatchUrl { .. } if session.surface() == frontcap_native::ios_execution::UiSurface::Uikit => session.validate_foreground(),
                Op::ExecuteIos { operation } => session.validate_operation(id, operation),
                _ => Err(io::Error::other("operation differs from iOS session")),
            };
        }
        if let Self::Flutter(target) = self {
            match op {
                Op::SetAppState {generation,state} => target.validate_set_app_state(id,generation,*state),
                Op::BeginFlutterEntry { generation, url } => target.validate_begin_entry(id, generation, url),
                Op::PollFlutterEntry { entry_request_id, generation } => target.validate_poll_entry(id, entry_request_id, generation),
                Op::RestartFlutter { generation, url } => target.validate_restart(id, generation, url.as_deref()),
                Op::DispatchUrl { request } => target.validate_external_entry(id, request),
                Op::ExecuteIos { operation } => target.validate_system_operation(id, operation),
                Op::Observe => target
                    .validate_operation(id, &frontcap_core::native::flutter::Operation::Observe {}),
                Op::ExecuteFlutter { operation, .. } => target.validate_operation(id, operation),
                Op::Stop => Ok(()),
                _ => Err(io::Error::other("operation differs from Flutter session")),
            }
        } else {
            Ok(())
        }
    }
    pub(super) fn apply(
        &mut self,
        id: &RequestId,
        op: Op,
        deadline: Instant,
    ) -> io::Result<Outcome> {
        let cancel = AtomicBool::new(false);
        match self {
            Self::IosHost(session) => {
                let request = match op { Op::Observe => frontcap_core::native::ios_host::Operation::Observe {input_basis:false},
                    Op::ExecuteIosHost {operation} => operation,
                    _ => return Err(io::Error::other("operation differs from iOS native host session")) };
                ios_host_reply(session.execute(id.clone(),frontcap_native::webview_session::Operation::Host {request},deadline,&cancel)?)
            }
            Self::AndroidHost(session) => {
                let operation = match op { Op::Observe => frontcap_core::native::android_host::Operation::Observe { input_basis: false },
                    Op::ExecuteAndroidHost { operation } => operation,
                    _ => return Err(io::Error::other("operation differs from native host session")) };
                host_reply(session.execute(id.clone(), frontcap_native::android_webview_session::Operation::Host { request: operation }, deadline, &cancel)?)
            }
            Self::AndroidWebview(session) => {
                let operation = match op { Op::Observe => frontcap_core::native::android_webview::Operation::Observe { input_basis: false },
                    Op::ExecuteAndroidWebview { operation } => operation,
                    _ => return Err(io::Error::other("operation differs from Android WebView session")) };
                match session.execute(id.clone(), operation, deadline, &cancel)? {
                    frontcap_native::android_webview_session::Reply::Completed { completion } => Ok(Outcome::AndroidWebviewCompleted { completion: *completion }),
                    frontcap_native::android_webview_session::Reply::Failed { error } => Ok(Outcome::AndroidWebviewUnresolved { error, lifecycle: None }),
                    frontcap_native::android_webview_session::Reply::HomeReturnFailed { error, lifecycle } => Ok(Outcome::AndroidWebviewUnresolved { error, lifecycle: lifecycle.map(|value| *value) }),
                }
            }
            Self::Webview(session) => {
                let operation = match op { Op::Observe => frontcap_core::native::webview::Operation::Capture,
                    Op::ExecuteWebview { operation } => operation,
                    _ => return Err(io::Error::other("operation differs from WebView session")) };
                match session.execute(id.clone(), operation, deadline, &cancel)? {
                    frontcap_native::webview_session::Reply::Completed { completion } => Ok(Outcome::WebviewCompleted { completion }),
                    frontcap_native::webview_session::Reply::Failed { error } => Ok(Outcome::WebviewUnresolved { error }),
                }
            }
            Self::Flutter(target) => {
                let operation = match op {
                    Op::SetAppState {generation,state} => return target.set_app_state(id.clone(), generation, state, deadline, &cancel)
                        .map(|transition| Outcome::FlutterStateChanged {transition}),
                    Op::BeginFlutterEntry { generation, url } => return target.begin_entry(id.clone(), generation, url, deadline, &cancel)
                        .map(|entry| Outcome::FlutterEntryStarted { entry }),
                    Op::PollFlutterEntry { entry_request_id, generation } => return target.poll_entry(id.clone(), entry_request_id, generation, deadline, &cancel)
                        .map(|progress| Outcome::FlutterEntryPolled { progress }),
                    Op::RestartFlutter { generation, url } => return target.restart(id.clone(), generation, url, deadline, &cancel)
                        .map(|replacement| Outcome::FlutterRestarted { replacement }),
                    Op::ExecuteIos { operation } => return target.execute_system(id.clone(), operation, deadline, &cancel)
                        .map(|completion| Outcome::IosCompleted { completion: frontcap_core::native::ios_request::Completion::Ui(Box::new(completion)) }),
                    Op::DispatchUrl { request } => return target.dispatch_url(id.clone(), request, deadline, &cancel)
                        .map(|acceptance| Outcome::ExternalUrlAccepted { acceptance }),
                    Op::Observe => frontcap_core::native::flutter::Operation::Observe {},
                    Op::ExecuteFlutter { operation, .. } => operation,
                    _ => return Err(io::Error::other("operation differs from Flutter session")),
                };
                Ok(Outcome::FlutterCompleted {
                    completion: target.execute(id.clone(), operation, deadline, &cancel)?,
                })
            }
            Self::AndroidViews(session) => {
                let reply=match op {
                    Op::SetAppState {generation,state} => return match session.set_view_state(id.clone(), generation, state, deadline, &cancel)? {
                        frontcap_native::android_app_state::Reply::Completed {transition} => Ok(Outcome::AndroidViewsStateChanged {transition: *transition}),
                        frontcap_native::android_app_state::Reply::Failed {error} => Err(io::Error::other(error)),
                    },
                    Op::RestartAndroidViews {generation, url} => return match session.restart_views(id.clone(), generation, url, deadline, &cancel)? {
                        frontcap_native::android_restarts::Reply::Completed {replacement} => Ok(Outcome::AndroidViewsRestarted {replacement: *replacement}),
                        frontcap_native::android_restarts::Reply::Failed {error} => Err(io::Error::other(error)),
                    },
                    Op::DispatchUrl { request } => return match session.dispatch_view_url(id.clone(), request, deadline, &cancel)? {
                        frontcap_native::external_entry::Reply::Accepted { acceptance } => Ok(Outcome::ExternalUrlAccepted { acceptance }),
                        frontcap_native::external_entry::Reply::Failed { error } => Err(io::Error::other(error)),
                    },
                    Op::Observe=>session.observe_views(id.clone(),deadline,&cancel)?,
                    Op::Execute {operation}=>session.execute_view_operation(id.clone(),operation,deadline,&cancel)?,
                    _=>return Err(io::Error::other("operation differs from Android View session")),
                };
                match reply {
                    session::ViewReply::Completed {completion}=>Ok(Outcome::Completed {completion}),
                    session::ViewReply::Unavailable {observation} => Ok(Outcome::AndroidViewsUnavailable {observation}),
                    session::ViewReply::Observed { capture } => Ok(Outcome::AndroidViewsObserved { capture }),
                    session::ViewReply::Failed { error } => Err(io::Error::other(error)),
                }
            }
            Self::Ios(session) => {
                if let Op::SetAppState {generation,state} = op {
                    return match session.set_app_state(id.clone(),generation,state,deadline,&cancel)? {
                        frontcap_native::ios_app_state::Reply::Completed {transition} => Ok(Outcome::UikitStateChanged {transition: *transition}),
                        frontcap_native::ios_app_state::Reply::Failed {error} => Err(io::Error::other(error)),
                    };
                }
                if let Op::RestartUikit { generation, url } = op {
                    return match session.restart(id.clone(), generation, url, deadline, &cancel)? {
                        frontcap_native::ios_restarts::Reply::Completed { replacement } => Ok(Outcome::UikitRestarted { replacement }),
                        frontcap_native::ios_restarts::Reply::Failed { error } => Err(io::Error::other(error)),
                    };
                }
                let operation = match op {
                    Op::DispatchUrl { request } => return match session.dispatch_url(id.clone(), request, deadline, &cancel)? {
                        frontcap_native::external_entry::Reply::Accepted { acceptance } => Ok(Outcome::ExternalUrlAccepted { acceptance }),
                        frontcap_native::external_entry::Reply::Failed { error } => Err(io::Error::other(error)),
                    },
                    Op::Observe => session.surface().observation(),
                    Op::ExecuteIos { operation } => operation,
                    _ => return Err(io::Error::other("operation differs from iOS session")),
                };
                match session.execute(id.clone(), operation, deadline, &cancel)? {
                    ios_session::Reply::Completed { completion } => {
                        Ok(Outcome::IosCompleted { completion })
                    }
                    ios_session::Reply::Failed { error } => Err(io::Error::other(error)),
                }
            }
        }
    }
    pub(super) fn stop(self, deadline: Instant) -> io::Result<Retirement> {
        match self {
            Self::AndroidHost(session) | Self::AndroidWebview(session) => retirement(session.stop(deadline)?.retirement()),
            Self::IosHost(session) | Self::Webview(session) => retirement(session.stop(deadline)?.retirement()),
            Self::Flutter(target) => target.stop(deadline),
            Self::AndroidViews(session) => match session.stop(deadline)? {
                session::Recovery::Finished { result, .. } => retirement(result.retirement),
                _ => Err(io::Error::other("Android retirement unresolved")),
            },
            Self::Ios(session) => retirement(session.stop(deadline)?.retirement()),
        }
    }
}
fn retirement(value: Option<Retirement>) -> io::Result<Retirement> {
    value
        .filter(|r| r.passed)
        .ok_or_else(|| io::Error::other("native retirement has no passing terminal evidence"))
}
pub(super) fn inspect_stop(directory: &Path, ready: &Ready, deadline: Instant) -> io::Result<Retirement> {
    match ready {
        Ready::AndroidHost { .. } | Ready::AndroidWebview { .. } =>
            retirement(frontcap_native::android_webview_session::inspect(directory)?.retirement()),
        Ready::IosHost { .. } | Ready::Webview { .. } => retirement(frontcap_native::webview_session::inspect(directory)?.retirement()),
        Ready::Flutter { .. } | Ready::FlutterEntering { .. } => loop {
            match frontcap_native::flutter_session::inspect(&directory.join("session"))? {
                frontcap_native::flutter_session::Recovery::Finished(result)
                    if result.retirement.passed =>
                {
                    break Ok(result.retirement)
                }
                frontcap_native::flutter_session::Recovery::Active if Instant::now() < deadline => {
                    // Failed commands can close the controller before its independent owner finishes.
                    // Wait only on that original active authority; never reconstruct it.
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                _ => break Err(io::Error::other("Flutter retirement unresolved")),
            }
        }
        Ready::AndroidViews { .. } => match session::inspect(directory)? {
            session::Recovery::Finished { result, .. } => retirement(result.retirement),
            _ => Err(io::Error::other("Android retirement unresolved")),
        },
        Ready::Ios { .. } => retirement(ios_session::inspect(directory)?.retirement()),
    }
}

/// Preserve worker uncertainty without treating a page completion as native-host evidence.
pub(super) fn host_reply(reply: frontcap_native::android_webview_session::Reply) -> io::Result<Outcome> {
    use frontcap_native::android_webview_session::{Reply, Completion};
    match reply {
        Reply::Completed { completion } => match *completion {
            Completion::Host { completion } => Ok(Outcome::AndroidHostCompleted { completion }),
            _ => Err(io::Error::other("page completion differs from native host operation")),
        },
        Reply::Failed { error } => Ok(Outcome::AndroidHostUnresolved { error }),
        Reply::HomeReturnFailed { .. } => Err(io::Error::other("page lifecycle result differs from native host operation")),
    }
}

pub(super) fn ios_host_reply(reply:frontcap_native::webview_session::Reply)->io::Result<Outcome> {
    use frontcap_native::webview_session::{Reply,Completion};
    match reply {
        Reply::Completed {completion} => match *completion {
            Completion::Host {completion} => Ok(Outcome::IosHostCompleted {completion}),
            _ => Err(io::Error::other("iOS native host returned page completion")),
        },
        Reply::Failed {error} => Ok(Outcome::IosHostUnresolved {error}),
    }
}

#[cfg(test)]
#[path = "target_service_config_tests.rs"]
mod service_config_tests;

#[cfg(test)]
#[path = "target_retirement_tests.rs"]
mod retirement_tests;
