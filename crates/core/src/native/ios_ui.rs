//! XCTest request identity is distinct from the app clock identity.
use super::{
    ios::ClockIdentity,
    request::{InvalidRequest, canonical_uuid},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiIdentity {
    pub simulator: String,
    pub generation: String,
    pub pid: u32,
}
impl UiIdentity {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if !canonical_uuid(&self.simulator.to_ascii_lowercase())
            || !canonical_uuid(&self.generation)
            || self.pid == 0
            || self.pid > i32::MAX as u32
        {
            return Err(InvalidRequest("invalid XCTest identity"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "operation",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum UiOperation {
    Launch {
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        environment: BTreeMap<String, String>,
    },
    SetAppState { app: ClockIdentity, state: super::app_state::State },
    /// Host visibility only; this operation never invents an application clock identity.
    SetHostAppState { bundle: String, pid: u32, state: super::app_state::State },
    Restart { app: ClockIdentity, #[serde(default, skip_serializing_if = "Option::is_none")] url: Option<String> },
    ObserveTargets {},
    ObserveSystemAlerts {},
    TapSystemButton { reference: String, observation: String },
    TapTarget {
        reference: String,
        observation: String,
    },
    PressTarget {
        reference: String,
        observation: String,
        clock_request: super::request::ClockRequest,
    },
    ScrollTarget { reference:String, observation:String, direction:super::ios_scroll::Direction },
    ReplaceText { reference:String, observation:String, start:usize, end:usize, text:String },
    Terminate {},
    Finish {},
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiRequest {
    pub generation: String,
    pub request_id: u64,
    #[serde(flatten)]
    pub operation: UiOperation,
}
impl UiRequest {
    pub fn validate(&self, identity: &UiIdentity) -> Result<(), InvalidRequest> {
        identity.validate()?;
        if let UiOperation::Launch { environment } = &self.operation {
            validate_launch_environment(environment)?;
        }
        if let UiOperation::SetHostAppState { bundle, pid, .. } = &self.operation {
            super::external_entry::Target::Ios { simulator: identity.simulator.to_ascii_lowercase(), bundle: bundle.clone() }.validate()?;
            if *pid == 0 || *pid > i32::MAX as u32 || *pid == identity.pid {
                return Err(InvalidRequest("host app differs from independent runner"));
            }
        }
        if let UiOperation::SetAppState { app, .. } = &self.operation {
            app.validate()?;
            if !app.simulator.eq_ignore_ascii_case(&identity.simulator) || app.pid == identity.pid {
                return Err(InvalidRequest("host transition belongs to another app or device"));
            }
        }
        if let UiOperation::Restart { app, url } = &self.operation {
            app.validate()?;
            if let Some(url) = url { super::external_entry::validate_url(url)?; }
            if !app.simulator.eq_ignore_ascii_case(&identity.simulator) || app.pid == identity.pid {
                return Err(InvalidRequest("restart belongs to another app or device"));
            }
        }
        if self.generation != identity.generation
            || self.request_id == 0
            || self.request_id > i64::MAX as u64
        {
            return Err(InvalidRequest(
                "UI request differs from its runner identity",
            ));
        }
        if let UiOperation::TapTarget {
            reference,
            observation,
        } | UiOperation::TapSystemButton { reference, observation }
          | UiOperation::PressTarget { reference, observation, .. }
          | UiOperation::ScrollTarget { reference, observation, .. }
          | UiOperation::ReplaceText { reference, observation, .. } = &self.operation
            && !super::ios_targets::valid_reference(reference, observation)
        {
            return Err(InvalidRequest("invalid UIKit target reference"));
        }
        if let UiOperation::ReplaceText { start,end,text,.. }=&self.operation {
            super::ios_edit::validate_arguments(*start,*end,text)?;
        }
        if let UiOperation::PressTarget { clock_request: clock, .. } = &self.operation
            && (!canonical_uuid(&clock.generation) || clock.request_id == 0
                || clock.request_id > i64::MAX as u64 || clock.delta_ns > i64::MAX as u64
                || clock.operation != super::request::ClockOperation::Advance) {
            return Err(InvalidRequest("invalid UIKit press clock request"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all="snake_case")]
pub enum UnavailableReason { ScrollInMotion, CaptureChanged }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiOutcome {
    HostAppStateChanged { change: HostAppStateChange },
    AppStateChanged { app: ClockIdentity, change: super::ios_lifecycle::StateChange },
    SystemAlertsObserved { catalog: super::ios_system_ui::Catalog, artifacts: Vec<super::observation::Artifact> },
    SystemButtonTapped { reference: String, observation: String },
    SystemButtonNotApplied { reference: String, observation: String, reason: super::ios_system_ui::NotAppliedReason },
    ObservationUnavailable { app:ClockIdentity, now_ns:u64, reason:UnavailableReason, artifacts:Vec<super::observation::Artifact> },
    Scrolled { app:ClockIdentity, reference:String, observation:String, now_ns:u64,
        direction:super::ios_scroll::Direction, before:super::ios_scroll::Scroll, after:super::ios_scroll::Scroll },
    TextEdit { app:ClockIdentity, reference:String, observation:String, now_ns:u64,
        delivery:super::ios_edit::Delivery },
    TargetPressed { app: ClockIdentity, evidence: super::ios_press::Evidence },
    TargetPressNotApplied {
        app: ClockIdentity,
        reference: String,
        observation: String,
        clock_request: super::request::ClockRequest,
        reason: super::ios_targets::NotAppliedReason,
    },
    Launched {
        app: ClockIdentity,
    },
    Restarted {
        previous: ClockIdentity,
        app: ClockIdentity,
        process_absent: bool,
        port_closed: bool,
        now_ns: u64,
    },
    ObservedTargets {
        app: ClockIdentity,
        now_ns: u64,
        catalog: super::ios_targets::Catalog,
        artifacts: Vec<super::observation::Artifact>,
    },
    TargetTapped {
        app: ClockIdentity,
        reference: String,
        observation: String,
    },
    TargetNotApplied {
        app: ClockIdentity,
        reference: String,
        observation: String,
        reason: super::ios_targets::NotAppliedReason,
    },
    Terminated {
        app: ClockIdentity,
        process_absent: bool,
        port_closed: bool,
    },
}
/// Native visibility of an already running app, without app clock or page claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostAppStateChange {
    pub bundle: String,
    pub pid: u32,
    pub requested: super::app_state::State,
    pub before_state: u8,
    pub app_state: u8,
}
impl HostAppStateChange {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        use super::app_state::State;
        let valid = match self.requested {
            State::Background => self.before_state == 4 && matches!(self.app_state, 2 | 3),
            State::Foreground => matches!(self.before_state, 2 | 3) && self.app_state == 4,
        };
        if !valid || self.pid == 0 || self.pid > i32::MAX as u32 {
            return Err(InvalidRequest("unconfirmed independent host visibility"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiCompletion {
    pub request: UiRequest,
    pub generation: String,
    pub pid: u32,
    pub last_id: u64,
    pub outcome: UiOutcome,
    pub receipt_sha256: String,
}
impl UiCompletion {
    pub fn validate(
        &self,
        runner: &UiIdentity,
        request: &UiRequest,
        expected_app: Option<&ClockIdentity>,
    ) -> Result<(), InvalidRequest> {
        if matches!(request.operation, UiOperation::ObserveSystemAlerts {} | UiOperation::TapSystemButton { .. }) {
            let app = expected_app.ok_or(InvalidRequest("system UI requires an acquired session"))?;
            app.validate()?;
            if !app.simulator.eq_ignore_ascii_case(&runner.simulator) {
                return Err(InvalidRequest("system UI belongs to another simulator"));
            }
            return self.validate_system(runner, request);
        }
        request.validate(runner)?;
        let (app, valid) = match (&request.operation, &self.outcome) {
            (UiOperation::SetAppState { app: bound, state }, UiOutcome::AppStateChanged { app, change }) => {
                change.validate()?;
                (app, expected_app == Some(app) && bound == app && state == &change.requested)
            },
            (UiOperation::ObserveTargets {},UiOutcome::ObservationUnavailable { app,now_ns,reason,artifacts }) =>
                (app,expected_app==Some(app) && *now_ns<=i64::MAX as u64 && match reason {
                    UnavailableReason::ScrollInMotion=>artifacts.is_empty(),
                    UnavailableReason::CaptureChanged=>valid_observation_artifacts(artifacts),
                }),
            (UiOperation::ScrollTarget { reference,observation,direction },
             UiOutcome::Scrolled { app,reference:actual,observation:observed,now_ns,direction:delivered,before,after }) => {
                super::ios_scroll::validate_delivery(before,after,*direction)?;
                (app,expected_app==Some(app) && reference==actual && observation==observed && direction==delivered && *now_ns<=i64::MAX as u64)
            }
            (UiOperation::ScrollTarget { reference,observation,.. }, UiOutcome::TargetNotApplied { app,reference:actual,observation:observed,.. }) =>
                (app,expected_app==Some(app) && reference==actual && observation==observed),
            (UiOperation::ReplaceText { reference,observation,start,end,text },
             UiOutcome::TextEdit { app,reference:actual,observation:observed,now_ns,delivery }) => {
                delivery.validate(app,*start,*end,text)?;
                (app,expected_app==Some(app) && reference==actual && observation==observed && *now_ns<=i64::MAX as u64)
            }
            (UiOperation::PressTarget { reference, observation, clock_request },
             UiOutcome::TargetPressed { app, evidence }) => {
                evidence.validate(app, reference, observation, clock_request)?;
                (app, expected_app == Some(app))
            }
            (UiOperation::PressTarget { reference, observation, clock_request },
             UiOutcome::TargetPressNotApplied { app, reference: actual, observation: observed,
                 clock_request: actual_clock, .. }) => {
                app.validate_request(clock_request)?;
                (app, expected_app == Some(app) && reference == actual && observation == observed && clock_request == actual_clock)
            }
            (UiOperation::Launch { .. }, UiOutcome::Launched { app }) => (app, expected_app.is_none()),
            (UiOperation::Restart { app: bound, .. }, UiOutcome::Restarted {
                previous, app, process_absent, port_closed, now_ns,
            }) => {
                previous.validate()?;
                (app, expected_app == Some(previous) && bound == previous && *process_absent
                    && *port_closed && *now_ns == 0 && app.pid != previous.pid
                    && app.generation != previous.generation
                    && app.simulator.eq_ignore_ascii_case(&previous.simulator))
            }
            (
                UiOperation::ObserveTargets {},
                UiOutcome::ObservedTargets {
                    app,
                    now_ns,
                    catalog,
                    artifacts,
                },
            ) => {
                catalog.validate()?;
                (
                    app,
                    expected_app == Some(app)
                        && *now_ns <= i64::MAX as u64
                        && valid_observation_artifacts(artifacts)
                        && artifacts[0].path.split('/').nth(1)
                            == Some(catalog.observation.as_str()),
                )
            }
            (
                UiOperation::TapTarget {
                    reference,
                    observation,
                },
                UiOutcome::TargetTapped {
                    app,
                    reference: actual,
                    observation: captured,
                }
                | UiOutcome::TargetNotApplied {
                    app,
                    reference: actual,
                    observation: captured,
                    ..
                },
            ) => (
                app,
                expected_app == Some(app) && reference == actual && observation == captured,
            ),
            (
                UiOperation::Terminate {} | UiOperation::Finish {},
                UiOutcome::Terminated {
                    app,
                    process_absent,
                    port_closed,
                },
            ) => (
                app,
                *process_absent && *port_closed && expected_app == Some(app),
            ),
            _ => return Err(InvalidRequest("UI completion belongs to another operation")),
        };
        app.validate()?;
        if !valid || !app.simulator.eq_ignore_ascii_case(&runner.simulator) {
            return Err(InvalidRequest("UI receipt does not resolve its bound app request"));
        }
        self.validate_identity(runner, request)
    }
    /// Validate runner/device evidence without inventing an application clock identity.
    /// The caller still needs live session authority to dispatch; this checks a saved receipt only.
    pub fn validate_system(&self, runner: &UiIdentity, request: &UiRequest) -> Result<(), InvalidRequest> {
        request.validate(runner)?;
        let valid = match (&request.operation, &self.outcome) {
            (UiOperation::SetHostAppState { bundle, pid, state }, UiOutcome::HostAppStateChanged { change }) => {
                change.validate()?;
                &change.bundle == bundle && change.pid == *pid && change.requested == *state
            },
            (UiOperation::ObserveSystemAlerts {}, UiOutcome::SystemAlertsObserved { catalog, artifacts }) => {
                catalog.validate()?;
                valid_observation_artifacts(artifacts)
                    && artifacts[0].path.split('/').nth(1) == Some(catalog.observation.as_str())
            }
            (UiOperation::TapSystemButton { reference, observation },
                UiOutcome::SystemButtonTapped { reference: actual, observation: observed }
                | UiOutcome::SystemButtonNotApplied { reference: actual, observation: observed, .. }) =>
                reference == actual && observation == observed,
            _ => false,
        };
        if !valid { return Err(InvalidRequest("system UI receipt differs from its bound request")); }
        self.validate_identity(runner, request)
    }
    fn validate_identity(&self, runner: &UiIdentity, request: &UiRequest) -> Result<(), InvalidRequest> {
        if &self.request != request
            || self.generation != runner.generation
            || self.pid != runner.pid
            || self.last_id != request.request_id
            || self.receipt_sha256.len() != 64
            || !self
                .receipt_sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(InvalidRequest(
                "UI receipt does not resolve its bound runner/app request",
            ));
        }
        Ok(())
    }
}

/// Both files belong to one producer capture and have bounded content-addressed metadata.
pub fn valid_observation_artifacts(artifacts: &[super::observation::Artifact]) -> bool {
    if artifacts.len() != 2 {
        return false;
    }
    let mut capture = None;
    for (item, filename) in artifacts.iter().zip(["screen.png", "hierarchy.txt"]) {
        let parts: Vec<_> = item.path.split('/').collect();
        if parts.len() != 3
            || parts[0] != "observations"
            || !canonical_uuid(parts[1])
            || parts[2] != filename
            || capture.is_some_and(|id| id != parts[1])
            || item.bytes == 0
            || item.bytes > 16 * 1024 * 1024
            || item.sha256.len() != 64
            || !item
                .sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return false;
        }
        capture = Some(parts[1]);
    }
    true
}

#[cfg(test)]
#[path = "ios_ui_tests.rs"]
mod tests;

/// App configuration cannot replace instrumentation or loader controls.
pub fn valid_app_environment_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key.as_bytes()[0].is_ascii_uppercase()
        && key.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
        && !["FC_", "DYLD_", "SIMCTL_"].iter().any(|prefix| key.starts_with(prefix))
}
pub fn validate_launch_environment(environment: &BTreeMap<String, String>) -> Result<(), InvalidRequest> {
    if environment.len() > 16 || environment.iter().any(|(key, value)| {
        !valid_app_environment_key(key) || value.len() > 2048 || value.chars().any(char::is_control)
    }) {
        return Err(InvalidRequest("invalid app launch environment"));
    }
    Ok(())
}
