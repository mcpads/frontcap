//! Native contact state, distinct from page delivery and virtual contact age.
use super::request::{InvalidRequest, canonical_uuid};
use serde::{Deserialize, Serialize};

/// Bounded per-generation journal capacity supported by the controller.
pub const MAX_REQUEST_CAPACITY: u64 = 4096;
fn legacy_request_capacity() -> u64 { 24 }

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Contact {
    /// Contact identifier from 1 through 5, unique within one contact set.
    pub id: u8,
    pub x: f64,
    pub y: f64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum Operation {
    /// One short DOWN/UP pair; requires an advertising producer and released contacts.
    Tap { expected_revision: u64, x: f64, y: f64 },
    Set {
        expected_revision: u64,
        contacts: Vec<Contact>,
    },
    Cancel {
        expected_revision: u64,
    },
}
impl Operation {
    pub fn revision(&self) -> u64 {
        match self {
            Self::Set {
                expected_revision, ..
            }
            | Self::Tap { expected_revision, .. }
            | Self::Cancel { expected_revision } => *expected_revision,
        }
    }
    pub fn contacts(&self) -> &[Contact] {
        match self {
            Self::Set { contacts, .. } => contacts,
            Self::Cancel { .. } | Self::Tap { .. } => &[],
        }
    }
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if let Self::Tap { x, y, .. } = self
            && [x, y].into_iter().any(|v| !v.is_finite() || !(0.0..=1.0).contains(v))
        { return Err(InvalidRequest("invalid native tap coordinates")); }
        let mut ids = std::collections::HashSet::new();
        if self.revision() > MAX_REQUEST_CAPACITY
            || self.contacts().len() > 5
            || self.contacts().iter().any(|p| {
                !(1..=5).contains(&p.id)
                    || !ids.insert(p.id)
                    || !p.x.is_finite()
                    || !p.y.is_finite()
                    || !(0.0..=1.0).contains(&p.x)
                    || !(0.0..=1.0).contains(&p.y)
            })
        {
            return Err(InvalidRequest("invalid native contact state"));
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    #[serde(default)]
    pub short_tap: bool,
    pub generation: String,
    pub pid: u32,
    pub available: bool,
    #[serde(default)]
    pub observed_contacts: bool,
    #[serde(default = "legacy_request_capacity")]
    pub request_capacity: u64,
}
impl Endpoint {
    pub fn validate(&self, pid: u32) -> Result<(), InvalidRequest> {
        if !canonical_uuid(&self.generation) || self.pid != pid || pid == 0 || !self.available
            || !(1..=MAX_REQUEST_CAPACITY).contains(&self.request_capacity) {
            return Err(InvalidRequest(
                "native contact endpoint unavailable or foreign",
            ));
        }
        Ok(())
    }
}
/// Enqueue acknowledgement only. Observe the consumer separately for delivery/effects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Receipt {
    Enqueued {
        revision: u64,
        contacts: Vec<Contact>,
        changed: bool,
        mach_timestamp: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tap: Option<TapEvents>,
    },
    Rejected {
        reason: String,
    },
}

/// Actual HID event timestamps and window-coordinate points, not consumer delivery receipts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TapFrame {
    pub id: u8,
    pub x: f64,
    pub y: f64,
    pub down: bool,
    pub enqueued: bool,
    pub mach_timestamp: u64,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TapEvents {
    pub timebase_numer: u32,
    pub timebase_denom: u32,
    pub frames: [TapFrame; 2],
}
impl TapEvents {
    pub fn valid(&self, x: f64, y: f64) -> bool {
        let [down, up] = &self.frames;
        self.timebase_numer > 0 && self.timebase_denom > 0
            && down.down && !up.down && down.mach_timestamp > 0
            && up.mach_timestamp >= down.mach_timestamp
            && self.frames.iter().all(|f| f.id == 1 && f.enqueued && f.x == x && f.y == y)
            && u128::from(up.mach_timestamp - down.mach_timestamp) * u128::from(self.timebase_numer)
                <= 100_000_000 * u128::from(self.timebase_denom)
    }
}

/// Full-screen foreground window identity used to bind normalized raster coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub generation: String,
    pub pid: u32,
    pub token: String,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
    pub fullscreen: bool,
}
impl Window {
    pub fn validate(&self, endpoint: &Endpoint) -> Result<(), InvalidRequest> {
        if !endpoint.observed_contacts
            || self.generation != endpoint.generation
            || self.pid != endpoint.pid
            || self.token.is_empty()
            || self.token.len() > 128
            || !self.fullscreen
            || [self.width, self.height, self.scale]
                .iter()
                .any(|v| !v.is_finite() || *v <= 0.0)
            || self.width * self.scale > 16384.0
            || self.height * self.scale > 16384.0
        {
            return Err(InvalidRequest(
                "native contact window unavailable or not full-screen",
            ));
        }
        Ok(())
    }
}

/// Capture-scoped admission hint; the native window is checked again at enqueue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaptureInput {
    /// After interruption, cancel the module's requested contacts before sending another set.
    #[serde(default)]
    pub requires_cancel: bool,
    pub observation: crate::ids::RequestId,
    pub expected_revision: u64,
    pub remaining_requests: u64,
}
