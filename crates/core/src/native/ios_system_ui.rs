//! Observed SpringBoard alerts. These are runner/device snapshots, not app-clock observations.
use super::request::{InvalidRequest, canonical_uuid};
use serde::{Deserialize, Serialize};

/// Only these iOS operations can run without a native application-clock binding.
pub fn request_operation(operation: &super::ios_request::Operation) -> Result<super::ios_ui::UiOperation, InvalidRequest> {
    use super::{ios_request::Operation, ios_ui::UiOperation};
    match operation {
        Operation::ObserveSystemAlerts => Ok(UiOperation::ObserveSystemAlerts {}),
        Operation::TapSystemButton { reference, observation } => Ok(UiOperation::TapSystemButton {
            reference: reference.clone(), observation: observation.clone(),
        }),
        _ => Err(InvalidRequest("operation is not system UI")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotAppliedReason {
    StaleObservation,
    UnknownTarget,
    AmbiguousTarget,
    DisabledTarget,
    UnhittableTarget,
    ObservationChanged,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Catalog {
    pub observation: String,
    pub bundle: String,
    /// False means the hierarchy changed across screenshot capture; no button is actionable.
    pub stable: bool,
    pub alerts: Vec<Alert>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alert {
    pub label: String,
    pub texts: Vec<String>,
    pub frame_mpt: [i32; 4],
    pub buttons: Vec<Button>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Button {
    pub reference: String,
    pub identifier: String,
    pub label: String,
    pub frame_mpt: [i32; 4],
    pub unique: bool,
    pub enabled: bool,
    pub hittable: bool,
}
impl Button {
    fn same_selector(&self, other: &Self) -> bool {
        self.identifier == other.identifier
            && self.label == other.label
            && self.frame_mpt == other.frame_mpt
    }
}
impl Catalog {
    pub fn validate(&self) -> Result<(), InvalidRequest> {
        if !canonical_uuid(&self.observation)
            || self.bundle != "com.apple.springboard"
            || self.alerts.len() > 8
        {
            return Err(InvalidRequest("invalid system alert observation"));
        }
        let mut text_units = 0;
        let mut text = |s: &str| -> Result<(), InvalidRequest> {
            let units = s.encode_utf16().count();
            text_units += units;
            if units > 1024 || text_units > 65536 {
                Err(InvalidRequest("system alert text exceeds budget"))
            } else {
                Ok(())
            }
        };
        let valid_frame = |f: &[i32; 4]| f[2] >= 0 && f[3] >= 0 && !f.contains(&i32::MIN);
        let mut index = 0;
        for alert in &self.alerts {
            text(&alert.label)?;
            if !valid_frame(&alert.frame_mpt) || alert.texts.len() > 16 || alert.buttons.len() > 16
            {
                return Err(InvalidRequest("invalid bounded system alert"));
            }
            for value in &alert.texts {
                text(value)?;
            }
            for button in &alert.buttons {
                text(&button.identifier)?;
                text(&button.label)?;
                let unique = self
                    .alerts
                    .iter()
                    .flat_map(|a| &a.buttons)
                    .filter(|other| button.same_selector(other))
                    .count()
                    == 1;
                if button.reference != format!("{}-{index}", self.observation)
                    || !valid_frame(&button.frame_mpt)
                    || button.unique != unique
                    || (button.hittable
                        && (!button.enabled
                            || !unique
                            || button.frame_mpt[2] == 0
                            || button.frame_mpt[3] == 0))
                {
                    return Err(InvalidRequest("invalid system alert button"));
                }
                index += 1;
            }
        }
        Ok(())
    }
    pub fn target(&self, observation: &str, reference: &str) -> Result<&Button, InvalidRequest> {
        self.validate()?;
        if self.stable
            && self.observation == observation
            && let Some(button) = self
                .alerts
                .iter()
                .flat_map(|a| &a.buttons)
                .find(|b| b.reference == reference && b.unique && b.enabled && b.hittable)
        {
            return Ok(button);
        }
        Err(InvalidRequest(
            "system button requires a fresh stable alert observation",
        ))
    }
}

#[cfg(test)]
#[path = "ios_system_ui_tests.rs"]
mod tests;
