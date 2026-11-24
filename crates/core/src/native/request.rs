use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidRequest(pub &'static str);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Target {
    AndroidViews,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub target: Target,
    pub device: String,
    pub generation: String,
    pub pid: u32,
}

/// Whether a native generation or controller identifier uses canonical lowercase UUID text.
pub fn canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, c)| {
            if [8, 13, 18, 23].contains(&i) {
                c == b'-'
            } else {
                c.is_ascii_digit() || (b'a'..=b'f').contains(&c)
            }
        })
}

impl Identity {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        let port = self
            .device
            .strip_prefix("emulator-")
            .and_then(|p| p.parse::<u16>().ok());
        if !port.is_some_and(|p| p > 0 && p % 2 == 0 && self.device == format!("emulator-{p}")) {
            return Err(InvalidRequest("expected an owned Android emulator serial"));
        }
        if self.pid == 0 || self.pid > i32::MAX as u32 || !canonical_uuid(&self.generation) {
            return Err(InvalidRequest("invalid native target identity"));
        }
        Ok(())
    }
}



#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    ScrollView { scroll: super::android_scroll::Scroll },
    TapView { tap: super::android_tap::Tap },
    ReplaceViewText { edit: super::android_edit::Edit },
    ReadClock {},
    Advance {
        by_us: u64,
    },
}

impl Operation {
    pub fn validate(&self, identity: &Identity) -> Result<(), InvalidRequest> {
        identity.validate()?;
        match self {
            Self::ScrollView {scroll} => {
                if identity.target != Target::AndroidViews { return Err(InvalidRequest("View scroll belongs to another target")); }
                scroll.request(&identity.generation,&identity.generation,1).map(|_|())
            }
            Self::TapView {tap} => {
                if identity.target != Target::AndroidViews { return Err(InvalidRequest("View tap belongs to another target")); }
                tap.request(&identity.generation,&identity.generation,1).map(|_|())
            }
            Self::ReplaceViewText {edit} => {
                if identity.target != Target::AndroidViews { return Err(InvalidRequest("View edit belongs to another target")); }
                edit.request(&identity.generation,&identity.generation,1).map(|_|())
            }
            Self::ReadClock {} => Ok(()),
            Self::Advance { by_us } => {
                if by_us.checked_mul(1000).is_none_or(|n| n > i64::MAX as u64) {
                    Err(InvalidRequest("native nanosecond delta overflows"))
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockOperation {
    Read,
    Advance,
}

/// Fields match the existing Android clock client's exact target request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClockRequest {
    pub generation: String,
    pub request_id: u64,
    pub operation: ClockOperation,
    pub delta_ns: u64,
}





#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "channel",
    content = "request",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum NativeRequest {
    ViewTap(super::android_tap::Request),
    ViewScroll(super::android_scroll::Request),
    ViewText(super::android_edit::Request),
    Clock(ClockRequest),
}

impl NativeRequest {
    pub fn build(
        identity: &Identity,
        controller: &str,
        number: u64,
        op: &Operation,
    ) -> Result<Self, InvalidRequest> {
        op.validate(identity)?;
        if number == 0 || number > i64::MAX as u64 {
            return Err(InvalidRequest("native request sequence exhausted"));
        }
        Ok(match op {
            Operation::ScrollView {scroll} => Self::ViewScroll(scroll.request(&identity.generation,controller,number)?),
            Operation::TapView {tap} => Self::ViewTap(tap.request(&identity.generation,controller,number)?),
            Operation::ReplaceViewText {edit} => Self::ViewText(edit.request(&identity.generation,controller,number)?),
            Operation::ReadClock {} => Self::Clock(ClockRequest {
                generation: identity.generation.clone(),
                request_id: number,
                operation: ClockOperation::Read,
                delta_ns: 0,
            }),
            Operation::Advance { by_us } => Self::Clock(ClockRequest {
                generation: identity.generation.clone(),
                request_id: number,
                operation: ClockOperation::Advance,
                delta_ns: by_us * 1000,
            }),
        })
    }
}

/// Evidence of a terminal Android outcome, supplied only after bridge receipt validation.
/// Known non-consuming rejections are distinct from successful and uncertain operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "channel", rename_all = "snake_case", deny_unknown_fields)]
pub enum Completion {
    ViewScroll { receipt: super::android_scroll::Receipt, receipt_sha256: String },
    ViewTap { receipt: super::android_tap::Receipt, receipt_sha256: String },
    ViewText { receipt: super::android_edit::Receipt, receipt_sha256: String },
    Clock {
        request: ClockRequest,
        generation: String,
        pid: u32,
        last_id: u64,
        before_ns: u64,
        now_ns: u64,
        applied: bool,
        receipt_sha256: String,
    },
    ClockRejected {
        request: ClockRequest,
        generation: String,
        pid: u32,
        last_id: u64,
        before_ns: u64,
        now_ns: u64,
        applied: bool,
        error: ClockRejection,
        receipt_sha256: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockRejection {
    InvalidRequest,
    ClockOverflow,
}

impl Completion {
    pub fn consumes_sequence(&self) -> bool {
        !matches!(self, Self::ClockRejected { .. })
    }

    pub fn validate(
        &self,
        identity: &Identity,
        bound: &NativeRequest,
    ) -> Result<(), InvalidRequest> {
        let (generation, pid, hash, matches) = match (self, bound) {
            (Self::ViewScroll {receipt,receipt_sha256},NativeRequest::ViewScroll(request)) => {
                receipt.validate(request,&identity.generation,identity.pid)?;
                (&receipt.generation,&receipt.pid,receipt_sha256,matches!(receipt.state,super::android_tap::State::Completed|super::android_tap::State::Rejected))
            }
            (Self::ViewTap {receipt,receipt_sha256},NativeRequest::ViewTap(request)) => {
                receipt.validate(request,&identity.generation,identity.pid)?;
                (&receipt.generation,&receipt.pid,receipt_sha256,matches!(receipt.state,super::android_tap::State::Completed|super::android_tap::State::Rejected))
            }
            (Self::ViewText {receipt,receipt_sha256},NativeRequest::ViewText(request)) => {
                receipt.validate(request,&identity.generation,identity.pid)?;
                (&receipt.generation,&receipt.pid,receipt_sha256,receipt.state!=super::android_edit::State::Pending)
            }
            (
                Self::Clock {
                    request,
                    generation,
                    pid,
                    last_id,
                    before_ns,
                    now_ns,
                    applied,
                    receipt_sha256,
                },
                NativeRequest::Clock(expected),
            ) => {
                let should_apply = expected.operation == ClockOperation::Advance;
                (
                    generation,
                    pid,
                    receipt_sha256,
                    request == expected
                        && *last_id == expected.request_id
                        && *applied == should_apply
                        && *now_ns <= i64::MAX as u64
                        && before_ns.checked_add(if should_apply { expected.delta_ns } else { 0 })
                            == Some(*now_ns),
                )
            }
            (
                Self::ClockRejected {
                    request,
                    generation,
                    pid,
                    last_id,
                    before_ns,
                    now_ns,
                    applied,
                    receipt_sha256,
                    ..
                },
                NativeRequest::Clock(expected),
            ) => (
                generation,
                pid,
                receipt_sha256,
                request == expected
                    && expected.request_id.checked_sub(1) == Some(*last_id)
                    && !*applied
                    && before_ns == now_ns
                    && *now_ns <= i64::MAX as u64,
            ),
            _ => return Err(InvalidRequest("receipt belongs to another native channel")),
        };
        if !matches
            || generation != &identity.generation
            || *pid != identity.pid
            || hash.len() != 64
            || !hash
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        {
            return Err(InvalidRequest(
                "receipt does not resolve the bound native request",
            ));
        }
        Ok(())
    }
}
