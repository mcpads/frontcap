//! Typed boundary for the instrumented Flutter UI-isolate endpoint.
//! Validation correlates receipts; it does not prove native display presentation or recovery.
use super::request::InvalidRequest;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Android,
    Ios,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub platform: Platform,
    pub device: String,
    pub generation: String,
    pub source_sha256: String,
}
impl Owner {
    pub fn validate_external_target(&self, target: &super::external_entry::Target) -> Result<(), InvalidRequest> {
        self.validate()?;
        target.validate()?;
        let matches = match (&self.platform, target) {
            (Platform::Android, super::external_entry::Target::Android { serial, .. }) => serial == &self.device,
            (Platform::Ios, super::external_entry::Target::Ios { simulator, .. }) => simulator == &self.device.to_ascii_lowercase(),
            _ => false,
        };
        if !matches { return Err(InvalidRequest("external entry device differs from Flutter owner")); }
        Ok(())
    }
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        Request {
            generation: self.generation.clone(),
            id: "owner-check".into(),
            operation: Operation::Read {},
        }
        .validate()?;
        if self.device.is_empty()
            || self.source_sha256.len() != 64
            || !self
                .source_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(InvalidRequest("invalid Flutter owner source or device"));
        }
        Ok(())
    }
}

pub const MAX_LOGICAL_US: u64 = 9_007_199_254_740_991;

pub(crate) fn valid_generation(value: &str) -> bool {
    value.len() == 48 && value.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Read {},
    Hold {},
    Advance {
        delta_us: u64,
    },
    Observe {},
    Tap {
        observation: String,
        target: String,
    },
    Scroll {
        observation: String,
        target: String,
        direction: ScrollDirection,
    },
    ReplaceText {
        observation: String,
        target: String,
        start: u32,
        end: u32,
        text: String,
    },
    Capture {
        observation: String,
    },
    Stop {},
}
impl Operation {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        match self {
            Self::Advance { delta_us } if *delta_us > MAX_LOGICAL_US => {
                return Err(InvalidRequest("Flutter delta exceeds exact integer range"));
            }
            Self::Tap {
                observation,
                target,
            }
            | Self::Scroll {
                observation,
                target,
                ..
            } => {
                reference(observation, b'o')?;
                reference(target, b's')?;
            }
            Self::ReplaceText {
                observation,
                target,
                start,
                end,
                text,
            } => {
                reference(observation, b'o')?;
                reference(target, b's')?;
                if start > end || *end > 16384 || text.encode_utf16().count() > 16384 {
                    return Err(InvalidRequest("invalid Flutter committed text edit"));
                }
            }
            Self::Capture { observation } => reference(observation, b'o')?,
            _ => {}
        }
        Ok(())
    }
    pub fn name(&self) -> OperationName {
        match self {
            Self::Read {} => OperationName::Read,
            Self::Hold {} => OperationName::Hold,
            Self::Advance { .. } => OperationName::Advance,
            Self::Observe {} => OperationName::Observe,
            Self::Tap { .. } => OperationName::Tap,
            Self::Scroll { .. } => OperationName::Scroll,
            Self::ReplaceText { .. } => OperationName::ReplaceText,
            Self::Capture { .. } => OperationName::Capture,
            Self::Stop {} => OperationName::Stop,
        }
    }
}

fn reference(value: &str, prefix: u8) -> Result<(), InvalidRequest> {
    if value.len() < 2
        || value.as_bytes()[0] != prefix
        || !value.as_bytes()[1..].iter().all(u8::is_ascii_digit)
    {
        return Err(InvalidRequest("invalid Flutter observed identity"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub generation: String,
    pub id: String,
    // The operation rejects every remaining field. Applying deny_unknown_fields
    // to this outer struct would also reject valid flattened operation fields.
    #[serde(flatten)]
    pub operation: Operation,
}
impl Request {
    /// The caller supplies only its current observation for this generation.
    /// This is an admission check, not authority to reconstruct a stale basis.
    pub fn validate_basis(&self, basis: Option<&Observation>) -> Result<(), InvalidRequest> {
        self.validate()?;
        let (observation, target) = match &self.operation {
            Operation::Tap {
                observation,
                target,
            }
            | Operation::Scroll {
                observation,
                target,
                ..
            }
            | Operation::ReplaceText {
                observation,
                target,
                ..
            } => (observation, Some(target)),
            Operation::Capture { observation } => (observation, None),
            _ => return Ok(()),
        };
        let basis = basis.ok_or(InvalidRequest(
            "Flutter operation needs a fresh observation",
        ))?;
        basis.validate()?;
        if &basis.observation != observation {
            return Err(InvalidRequest("Flutter operation uses a stale observation"));
        }
        if let Some(target) = target {
            let node = basis
                .nodes
                .iter()
                .find(|node| &node.id == target)
                .ok_or(InvalidRequest("Flutter target is not observed"))?;
            if node.rect[0] >= node.rect[2] || node.rect[1] >= node.rect[3] {
                return Err(InvalidRequest("Flutter target has empty bounds"));
            }
            if let Operation::ReplaceText {
                start, end, text, ..
            } = &self.operation
            {
                if !node.editable || !node.focused {
                    return Err(InvalidRequest(
                        "Flutter target is not a focused editable field",
                    ));
                }
                let mut units = 0;
                let mut start_valid = *start == 0;
                let mut end_valid = *end == 0;
                for ch in node.value.chars() {
                    units += ch.len_utf16() as u32;
                    start_valid |= *start == units;
                    end_valid |= *end == units;
                }
                if !start_valid
                    || !end_valid
                    || units - (end - start) + text.encode_utf16().count() as u32 > 16384
                {
                    return Err(InvalidRequest(
                        "invalid Flutter UTF-16 edit range or length",
                    ));
                }
            } else if let Operation::Scroll { direction, .. } = &self.operation {
                if !node.scroll_actions.contains(direction) {
                    return Err(InvalidRequest("Flutter scroll action is not observed"));
                }
            } else if !node.tappable || node.rect[0] >= node.rect[2] || node.rect[1] >= node.rect[3]
            {
                return Err(InvalidRequest(
                    "Flutter target is not an observed tap target",
                ));
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if !valid_generation(&self.generation) {
            return Err(InvalidRequest("invalid Flutter generation"));
        }
        if self.id.is_empty()
            || self.id.len() > 100
            || !self
                .id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            || self.id.starts_with('-')
        {
            return Err(InvalidRequest("invalid Flutter request identity"));
        }
        self.operation.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationName {
    Read,
    Hold,
    Advance,
    Observe,
    Tap,
    Scroll,
    ReplaceText,
    Capture,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub tooltip: String,
    pub value: String,
    pub tappable: bool,
    #[serde(default)]
    pub editable: bool,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub scroll_actions: Vec<ScrollDirection>,
    #[serde(default)]
    pub scroll_position: Option<f64>,
    #[serde(default)]
    pub scroll_min: Option<f64>,
    #[serde(default)]
    pub scroll_max: Option<f64>,
    /// Logical pointer coordinates, after the view transform has been inverted.
    pub rect: [f64; 4],
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewGeometry {
    pub width: f64,
    pub height: f64,
    /// Platform-reported obscured edges in logical pixels: left, top, right, bottom.
    pub insets: [f64; 4],
}
impl ViewGeometry {
    fn validate(&self) -> Result<(), InvalidRequest> {
        if !self.width.is_finite()
            || !self.height.is_finite()
            || self.width <= 0.0
            || self.height <= 0.0
            || !self.insets.iter().all(|v| v.is_finite() && *v >= 0.0)
        {
            return Err(InvalidRequest("invalid Flutter view geometry"));
        }
        Ok(())
    }
    fn contains_center(&self, rect: [f64; 4]) -> bool {
        let x = rect[0] + (rect[2] - rect[0]) / 2.0;
        let y = rect[1] + (rect[3] - rect[1]) / 2.0;
        x >= self.insets[0]
            && x < self.width - self.insets[2]
            && y >= self.insets[1]
            && y < self.height - self.insets[3]
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub observation: String,
    pub revision: u64,
    pub framework_frame: u64,
    pub logical_us: u64,
    pub nodes: Vec<Node>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<ViewGeometry>,
}
impl Observation {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        reference(&self.observation, b'o')?;
        if self.logical_us > MAX_LOGICAL_US {
            return Err(InvalidRequest("invalid Flutter observation time"));
        }
        if let Some(view) = &self.view {
            view.validate()?;
        }
        let mut ids = HashSet::new();
        for node in &self.nodes {
            reference(&node.id, b's')?;
            if (node.tappable || node.editable || !node.scroll_actions.is_empty())
                && self
                    .view
                    .as_ref()
                    .is_some_and(|v| !v.contains_center(node.rect))
            {
                return Err(InvalidRequest(
                    "Flutter interactive target is obscured or outside view",
                ));
            }
            let directions: HashSet<_> = node.scroll_actions.iter().collect();
            if directions.len() != node.scroll_actions.len()
                || [node.scroll_position, node.scroll_min, node.scroll_max]
                    .iter()
                    .flatten()
                    .any(|v| !v.is_finite())
                || node
                    .scroll_min
                    .zip(node.scroll_max)
                    .is_some_and(|(min, max)| min > max)
            {
                return Err(InvalidRequest("invalid Flutter scroll metadata"));
            }
            if !ids.insert(&node.id)
                || !node.rect.iter().all(|v| v.is_finite())
                || node.rect[0] > node.rect[2]
                || node.rect[1] > node.rect[3]
            {
                return Err(InvalidRequest("invalid Flutter semantic nodes"));
            }
        }
        Ok(())
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Advance {
    pub before_us: u64,
    pub requested_us: u64,
    pub after_us: u64,
    pub time_changed: bool,
    pub callbacks: u64,
    pub drain_completed: bool,
    pub framework_frame_completed: bool,
    pub failure: Option<String>,
    pub cancelled_on_failure: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Raster {
    #[serde(rename = "framework-layer-png")]
    FrameworkLayerPng,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Capture {
    Complete {
        surface: Observation,
        error: Option<String>,
        png_base64: String,
        raster: Raster,
    },
    Partial {
        surface: Observation,
        error: String,
        raster: Raster,
    },
}
impl Capture {
    pub fn surface(&self) -> &Observation {
        match self {
            Self::Complete { surface, .. } | Self::Partial { surface, .. } => surface,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotApplied {
    ObservationChanged,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completion {
    pub generation: String,
    pub id: String,
    pub operation: OperationName,
    pub logical_us: u64,
    pub held: bool,
    pub faulted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advance: Option<Advance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<Observation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<Capture>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_applied: Option<NotApplied>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum Response {
    Completed(Box<Completion>),
    Pending { id: String, generation: String },
    Rejected { error: String },
}
impl Completion {
    /// Validate against the exact dispatched request, last known time and capture basis.
    /// A failed advance can change time; preserving that terminal failure forbids re-execution.
    pub fn validate(
        &self,
        request: &Request,
        before_us: u64,
        basis: Option<&Observation>,
    ) -> Result<(), InvalidRequest> {
        request.validate()?;
        if self.generation != request.generation
            || self.id != request.id
            || self.operation != request.operation.name()
            || !self.held
            || self.logical_us > MAX_LOGICAL_US
            || before_us > MAX_LOGICAL_US
        {
            return Err(InvalidRequest("Flutter receipt identity or clock mismatch"));
        }
        if self.not_applied.is_some() {
            if !matches!(
                request.operation,
                Operation::Tap { .. }
                    | Operation::ReplaceText { .. }
                    | Operation::Scroll { .. }
                    | Operation::Capture { .. }
            ) || self.logical_us != before_us
                || self.faulted
                || self.advance.is_some()
                || self.surface.is_some()
                || self.capture.is_some()
            {
                return Err(InvalidRequest("invalid Flutter no-effect completion"));
            }
            request.validate_basis(basis)?;
            return Ok(());
        }
        if self.faulted
            && !matches!(
                request.operation,
                Operation::Read {} | Operation::Stop {} | Operation::Advance { .. }
            )
        {
            return Err(InvalidRequest(
                "Flutter faulted timeline cannot complete new surface or hold work",
            ));
        }
        if self.advance.is_some() != matches!(request.operation, Operation::Advance { .. })
            || self.surface.is_some() != matches!(request.operation, Operation::Observe {})
            || self.capture.is_some() != matches!(request.operation, Operation::Capture { .. })
        {
            return Err(InvalidRequest(
                "Flutter receipt payload differs from operation",
            ));
        }
        if let Operation::Advance { delta_us } = request.operation {
            let a = self.advance.as_ref().unwrap();
            let requested = before_us
                .checked_add(delta_us)
                .filter(|v| *v <= MAX_LOGICAL_US)
                .ok_or(InvalidRequest("Flutter requested instant overflow"))?;
            if a.before_us != before_us
                || a.requested_us != requested
                || a.after_us != self.logical_us
                || a.after_us < before_us
                || a.after_us > requested
                || a.time_changed != (a.after_us != before_us)
                || self.faulted != a.failure.is_some()
                || a.drain_completed == self.faulted
                || (a.framework_frame_completed && !a.drain_completed)
                || (!self.faulted
                    && (a.after_us != requested
                        || !a.framework_frame_completed
                        || a.cancelled_on_failure != 0))
            {
                return Err(InvalidRequest("inconsistent Flutter advance outcome"));
            }
        } else if self.logical_us != before_us {
            return Err(InvalidRequest("Flutter non-advance changed logical time"));
        }
        if let Some(surface) = &self.surface {
            surface.validate()?;
            if surface.logical_us != self.logical_us {
                return Err(InvalidRequest("Flutter observation time mismatch"));
            }
        }
        if let Some(capture) = &self.capture {
            let surface = capture.surface();
            surface.validate()?;
            let Operation::Capture { observation } = &request.operation else {
                unreachable!()
            };
            if &surface.observation != observation
                || basis != Some(surface)
                || surface.logical_us != self.logical_us
            {
                return Err(InvalidRequest(
                    "Flutter capture does not match observed basis",
                ));
            }
            match capture {
                Capture::Complete {
                    error, png_base64, ..
                } if error.is_some() || png_base64.is_empty() => {
                    return Err(InvalidRequest(
                        "Flutter complete capture has no clean raster",
                    ));
                }
                Capture::Partial { error, .. } if error.is_empty() => {
                    return Err(InvalidRequest("Flutter partial capture has no failure"));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "flutter_tests.rs"]
mod tests;
