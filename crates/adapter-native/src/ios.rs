//! One-call iOS clock bridge; simulator and XCTest ownership must be supplied by a session owner.
use frontcap_core::native::{
    ios::{ClockCompletion, ClockIdentity},
    request::{ClockRequest, InvalidRequest},
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClockCall {
    Initialize {
        identity: ClockIdentity,
        source_sha256: String,
    },
    Execute {
        identity: ClockIdentity,
        request: ClockRequest,
    },
    Recover {
        identity: ClockIdentity,
        request: ClockRequest,
    },
    /// Reserve the exact advance for a separately journaled UI delivery, without transmission.
    Reserve {
        identity: ClockIdentity,
        request: ClockRequest,
    },
    ReserveUi {
        identity: ClockIdentity,
        request: ClockRequest,
        ui_identity: UiIdentity,
        ui_request: UiRequest,
    },
    ReleaseUi {
        identity: ClockIdentity,
        request: ClockRequest,
        ui_identity: UiIdentity,
        ui_request: UiRequest,
        receipt_sha256: String,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum ClockReply {
    UiReserved {
        identity: ClockIdentity,
        request: ClockRequest,
        ui_identity: UiIdentity,
        ui_request: UiRequest,
    },
    UiReleased {
        identity: ClockIdentity,
        request: ClockRequest,
        ui_identity: UiIdentity,
        ui_request: UiRequest,
        receipt_sha256: String,
    },
    /// A reservation acknowledgement is not permission to replay an interrupted gesture.
    Reserved {
        identity: ClockIdentity,
        request: ClockRequest,
    },
    Ready {
        identity: ClockIdentity,
        source_sha256: String,
    },
    Completed {
        completion: ClockCompletion,
    },
    Unresolved {
        error: String,
    },
}
impl ClockReply {
    pub fn validate(&self, call: &ClockCall) -> Result<(), InvalidRequest> {
        match (self, call) {
            (Self::UiReserved { identity, request, ui_identity, ui_request },
             ClockCall::ReserveUi { identity: expected, request: bound, ui_identity: runner, ui_request: ui }) => {
                validate_ui_reservation(expected,bound,runner,ui)?;
                if identity!=expected || request!=bound || ui_identity!=runner || ui_request!=ui {
                    return Err(InvalidRequest("UI clock reservation differs from intent"));
                }
                Ok(())
            }
            (Self::UiReleased { identity, request, ui_identity, ui_request, receipt_sha256 },
             ClockCall::ReleaseUi { identity: expected, request: bound, ui_identity: runner, ui_request: ui, receipt_sha256: proof }) => {
                validate_ui_reservation(expected,bound,runner,ui)?;
                if identity!=expected || request!=bound || ui_identity!=runner || ui_request!=ui
                    || receipt_sha256!=proof || proof.len()!=64
                    || !proof.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)) {
                    return Err(InvalidRequest("UI clock release differs from its rejection proof"));
                }
                Ok(())
            }
            (Self::Reserved { identity, request }, ClockCall::Reserve { identity: expected, request: bound }) => {
                expected.validate_request(bound)?;
                if identity != expected || request != bound
                    || request.operation != frontcap_core::native::request::ClockOperation::Advance {
                    return Err(InvalidRequest("external iOS clock reservation differs from intent"));
                }
                Ok(())
            }
            (
                Self::Ready {
                    identity,
                    source_sha256,
                },
                ClockCall::Initialize {
                    identity: expected,
                    source_sha256: source,
                },
            ) => {
                identity.validate()?;
                if identity != expected
                    || source_sha256 != source
                    || source.len() != 64
                    || !source
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                {
                    return Err(InvalidRequest(
                        "iOS clock initialization differs from frozen inputs",
                    ));
                }
                Ok(())
            }
            (
                Self::Completed { completion },
                ClockCall::Execute { identity, request } | ClockCall::Recover { identity, request },
            ) => completion.validate(identity, request),
            _ => Err(InvalidRequest("iOS clock bridge did not resolve its call")),
        }
    }
}

fn validate_ui_reservation(app: &ClockIdentity, clock: &ClockRequest, runner: &UiIdentity,
    ui: &UiRequest) -> Result<(), InvalidRequest> {
    app.validate_request(clock)?;
    ui.validate(runner)?;
    if !app.simulator.eq_ignore_ascii_case(&runner.simulator) || app.pid==runner.pid
        || app.generation==runner.generation {
        return Err(InvalidRequest("UI and clock reservation owners differ"));
    }
    if let frontcap_core::native::ios_ui::UiOperation::PressTarget { clock_request, .. } = &ui.operation
        && clock_request==clock {
        return Ok(());
    }
    Err(InvalidRequest("UI request does not bind the clock reservation"))
}

#[cfg(test)]
#[path = "ios_tests.rs"]
mod tests;

use frontcap_core::native::ios_ui::{UiCompletion, UiIdentity, UiRequest};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "call", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiCall {
    Initialize {
        identity: UiIdentity,
        source_sha256: String,
    },
    Execute {
        identity: UiIdentity,
        app: Option<ClockIdentity>,
        request: UiRequest,
    },
    Recover {
        identity: UiIdentity,
        app: Option<ClockIdentity>,
        request: UiRequest,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case", deny_unknown_fields)]
pub enum UiReply {
    Ready {
        identity: UiIdentity,
        source_sha256: String,
    },
    Completed {
        completion: Box<UiCompletion>,
    },
    Unresolved {
        error: String,
    },
}
impl UiReply {
    pub fn validate(&self, call: &UiCall) -> Result<(), InvalidRequest> {
        match (self, call) {
            (
                Self::Ready {
                    identity,
                    source_sha256,
                },
                UiCall::Initialize {
                    identity: expected,
                    source_sha256: source,
                },
            ) => {
                identity.validate()?;
                if identity != expected
                    || source_sha256 != source
                    || source.len() != 64
                    || !source
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
                {
                    return Err(InvalidRequest(
                        "XCTest initialization differs from frozen inputs",
                    ));
                }
                Ok(())
            }
            (
                Self::Completed { completion },
                UiCall::Execute {
                    identity,
                    app,
                    request,
                }
                | UiCall::Recover {
                    identity,
                    app,
                    request,
                },
            ) => {
                if app.is_none() && matches!(request.operation,
                    frontcap_core::native::ios_ui::UiOperation::ObserveSystemAlerts {}
                    | frontcap_core::native::ios_ui::UiOperation::TapSystemButton { .. }
                    | frontcap_core::native::ios_ui::UiOperation::SetHostAppState { .. }) {
                    completion.validate_system(identity, request)
                } else { completion.validate(identity, request, app.as_ref()) }
            },
            _ => Err(InvalidRequest("XCTest bridge did not resolve its call")),
        }
    }
}
