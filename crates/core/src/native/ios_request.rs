//! Recorder intent and exact iOS wire requests after the initial app launch.
use super::{
    ios::{ClockCompletion, ClockIdentity},
    ios_ui::{UiCompletion, UiIdentity, UiOperation, UiRequest},
    request::{ClockOperation, ClockRequest, InvalidRequest, canonical_uuid},
};
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub runner: UiIdentity,
    pub app: ClockIdentity,
    pub controller_id: String,
    pub source_sha256: String,
}
impl Owner {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        self.runner.validate()?;
        self.app.validate()?;
        if !self
            .runner
            .simulator
            .eq_ignore_ascii_case(&self.app.simulator)
            || self.runner.pid == self.app.pid
            || self.runner.generation == self.app.generation
            || !canonical_uuid(&self.controller_id)
            || self.source_sha256.len() != 64
            || !self
                .source_sha256
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(InvalidRequest("invalid iOS request owner"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    SetAppState { generation: String, state: super::app_state::State },
    ReadClock,
    AdvanceClock {
        delta_ns: u64,
    },
    ObserveTargets,
    ObserveSystemAlerts,
    TapSystemButton { reference: String, observation: String },
    TapTarget {
        reference: String,
        observation: String,
    },
    PressTarget {
        reference: String,
        observation: String,
        delta_ns: u64,
    },
    ScrollTarget { reference:String, observation:String, direction:super::ios_scroll::Direction },
    ReplaceText { reference:String, observation:String, start:usize, end:usize, text:String },
    Finish,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "channel",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Request {
    Clock(ClockRequest),
    Ui(UiRequest),
    Press { ui: UiRequest, clock: ClockRequest },
}
impl Request {
    pub fn build(
        owner: &Owner,
        operation: &Operation,
        number: u64,
    ) -> Result<Self, InvalidRequest> {
        owner.validate()?;
        match operation {
            Operation::PressTarget { .. } => Err(InvalidRequest("press requires both channel numbers")),
            Operation::ReadClock | Operation::AdvanceClock { .. } => {
                let request = ClockRequest {
                    generation: owner.app.generation.clone(),
                    request_id: number,
                    operation: if matches!(operation, Operation::ReadClock) {
                        ClockOperation::Read
                    } else {
                        ClockOperation::Advance
                    },
                    delta_ns: match operation {
                        Operation::AdvanceClock { delta_ns } => *delta_ns,
                        _ => 0,
                    },
                };
                owner.app.validate_request(&request)?;
                Ok(Self::Clock(request))
            }
            _ => {
                let request = UiRequest {
                    generation: owner.runner.generation.clone(),
                    request_id: number,
                    operation: match operation {
                        Operation::SetAppState { generation, state } => {
                            if generation != &owner.app.generation { return Err(InvalidRequest("host transition generation differs")); }
                            UiOperation::SetAppState { app: owner.app.clone(), state: *state }
                        },
                        Operation::ObserveTargets => UiOperation::ObserveTargets {},
                        Operation::ObserveSystemAlerts => UiOperation::ObserveSystemAlerts {},
                        Operation::TapSystemButton { reference, observation } => UiOperation::TapSystemButton {
                            reference: reference.clone(), observation: observation.clone(),
                        },
                        Operation::TapTarget {
                            reference,
                            observation,
                        } => UiOperation::TapTarget {
                            reference: reference.clone(),
                            observation: observation.clone(),
                        },
                        Operation::ScrollTarget { reference,observation,direction } => UiOperation::ScrollTarget {
                            reference:reference.clone(),observation:observation.clone(),direction:*direction,
                        },
                        Operation::ReplaceText { reference,observation,start,end,text } => UiOperation::ReplaceText {
                            reference:reference.clone(),observation:observation.clone(),start:*start,end:*end,text:text.clone(),
                        },
                        Operation::Finish => UiOperation::Finish {},
                        _ => unreachable!(),
                    },
                };
                request.validate(&owner.runner)?;
                Ok(Self::Ui(request))
            }
        }
    }
    pub fn build_pair(owner: &Owner, operation: &Operation, number: u64, clock_number: u64) -> Result<Self, InvalidRequest> {
        let Operation::PressTarget { reference, observation, delta_ns } = operation else {
            return Self::build(owner, operation, number);
        };
        owner.validate()?;
        let clock = ClockRequest { generation: owner.app.generation.clone(), request_id: clock_number,
            operation: ClockOperation::Advance, delta_ns: *delta_ns };
        owner.app.validate_request(&clock)?;
        let ui = UiRequest { generation: owner.runner.generation.clone(), request_id: number,
            operation: UiOperation::PressTarget { reference: reference.clone(), observation: observation.clone(),
                clock_request: clock.clone() } };
        ui.validate(&owner.runner)?;
        Ok(Self::Press { ui, clock })
    }
    pub fn number(&self) -> u64 {
        match self {
            Self::Clock(r) => r.request_id,
            Self::Ui(r) => r.request_id,
            Self::Press { ui, .. } => ui.request_id,
        }
    }
    pub fn is_clock(&self) -> bool {
        matches!(self, Self::Clock(_))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "channel",
    content = "receipt",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Completion {
    Clock(ClockCompletion),
    Ui(Box<UiCompletion>),
    Press { ui: Box<UiCompletion>, clock: PressClock },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum PressClock {
    Delivered { completion: ClockCompletion },
    Released { request: ClockRequest, ui_receipt_sha256: String },
}
impl PressClock {
    pub fn request(&self) -> &ClockRequest {
        match self {
            Self::Delivered { completion } => &completion.request,
            Self::Released { request, .. } => request,
        }
    }
}
impl Completion {
    pub fn request(&self) -> Request {
        match self {
            Self::Clock(c) => Request::Clock(c.request.clone()),
            Self::Ui(c) => Request::Ui(c.request.clone()),
            Self::Press { ui, clock } => Request::Press { ui: ui.request.clone(), clock: clock.request().clone() },
        }
    }
    pub fn validate(&self, owner: &Owner, request: &Request) -> Result<(), InvalidRequest> {
        owner.validate()?;
        match (self, request) {
            (Self::Clock(c), Request::Clock(r)) => c.validate(&owner.app, r),
            (Self::Ui(c), Request::Ui(r)) => c.validate(&owner.runner, r, Some(&owner.app)),
            (Self::Press { ui, clock }, Request::Press { ui: request, clock: reserved }) => {
                use super::ios_ui::UiOutcome;
                ui.validate(&owner.runner, request, Some(&owner.app))?;
                owner.app.validate_request(reserved)?;
                if clock.request()!=reserved || !matches!(&request.operation,
                    UiOperation::PressTarget { clock_request, .. } if clock_request==reserved) {
                    return Err(InvalidRequest("press clock completion differs"));
                }
                match (&ui.outcome, clock) {
                    (UiOutcome::TargetPressed { evidence, .. }, PressClock::Delivered { completion }) => {
                        completion.validate(&owner.app,reserved)?;
                        let raw=&evidence.clock_reply;
                        if completion.status!=raw.status || completion.before_ns!=raw.before_ns
                            || completion.now_ns!=raw.now_ns || completion.applied!=raw.applied
                            || completion.advance_result!=raw.advance_result || completion.drain_result!=raw.drain_result {
                            return Err(InvalidRequest("press UI and clock effects disagree"));
                        }
                    }
                    (UiOutcome::TargetPressNotApplied { clock_request, .. }, PressClock::Released { ui_receipt_sha256, .. })
                        if clock_request==reserved && ui_receipt_sha256==&ui.receipt_sha256 => (),
                    _ => return Err(InvalidRequest("press result lacks delivered or released clock proof")),
                }
                Ok(())
            }
            _ => Err(InvalidRequest("iOS completion channel differs")),
        }
    }
}
