//! Native pointer ownership. Handles cannot cross execution generations.
use crate::ids::{LaunchId, RequestId};
use crate::observation::ObservationBoundary;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerButton {
    #[default]
    Left,
    Right,
    Middle,
}

impl PointerButton {
    pub fn mask(self) -> u8 {
        match self { Self::Left => 1, Self::Right => 2, Self::Middle => 4 }
    }
}

/// What the pointer owns: mouse buttons, or contacts on a touch screen (the profile's `touch`).
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PointerKind {
    #[default]
    Mouse,
    Touch,
}

impl PointerKind {
    pub fn is_mouse(&self) -> bool {
        *self == PointerKind::Mouse
    }
}

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointerHandle {
    pub launch_id: LaunchId,
    pub request_id: RequestId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointerContext {
    pub frame_id: String,
    pub loader_id: String,
}

/// A live native touch contact, with viewport CSS coordinates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TouchContact {
    pub id: u32,
    pub point: [i32; 2],
    /// Started over another document than the session's first contact: Chrome sends its pointer events
    /// to the element under it, and its touch events to that first document, or none when it lies
    /// outside that document's frame.
    #[serde(default, skip_serializing_if = "is_false")]
    pub other_document: bool,
}

fn is_false(value: &bool) -> bool { !*value }

fn first_unused_contact() -> u32 { 1 }
fn is_first_unused_contact(id: &u32) -> bool { *id == 1 }

/// Durable native ownership, recorded in the same trace event as the input that establishes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointerHold {
    /// Native frame/document identity, used only for lifetime checks and recovery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native: Option<PointerContext>,
    pub request_id: RequestId,
    pub document_id: u64,
    pub point: [i32; 2],
    #[serde(default, skip_serializing_if = "is_false")]
    pub html_drag: bool,
    pub button: PointerButton,
    /// Held mouse buttons in press order. Empty on legacy single-button records and touch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buttons: Vec<PointerButton>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contacts: Vec<TouchContact>,
    /// Contact IDs never repeat within an owned touch session.
    #[serde(default = "first_unused_contact", skip_serializing_if = "is_first_unused_contact")]
    pub next_contact_id: u32,
    #[serde(default, skip_serializing_if = "PointerKind::is_mouse")]
    pub pointer: PointerKind,
    /// The popup the pointer is held in; absent for the launch page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeldPointer {
    pub handle: PointerHandle,
    pub document_id: u64,
    pub point: [i32; 2],
    #[serde(default, skip_serializing_if = "is_false")]
    pub html_drag: bool,
    pub button: PointerButton,
    /// Held mouse buttons in press order. Empty on legacy single-button records and touch.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub buttons: Vec<PointerButton>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contacts: Vec<TouchContact>,
    #[serde(default, skip_serializing_if = "PointerKind::is_mouse")]
    pub pointer: PointerKind,
    /// The popup the pointer is held in; absent for the launch page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PointerState {
    pub boundary: ObservationBoundary,
    pub held: Option<HeldPointer>,
    /// Keys held by `key_down`, in the order they were pressed.
    #[serde(default)]
    pub keys: Vec<String>,
    /// The focused element's composition text while one is in progress.
    #[serde(default)]
    pub composition: Option<String>,
}

impl PointerHold {
    pub fn touch_contacts(&self) -> Vec<TouchContact> {
        if self.pointer != PointerKind::Touch { vec![] }
        else if self.contacts.is_empty() { vec![TouchContact { id: 0, point: self.point, other_document: false }] }
        else { self.contacts.clone() }
    }

    pub fn mouse_buttons(&self) -> Vec<PointerButton> {
        if self.pointer == PointerKind::Touch { vec![] }
        else if self.buttons.is_empty() { vec![self.button] }
        else { self.buttons.clone() }
    }
    pub fn button_mask(&self) -> u8 {
        self.mouse_buttons().iter().fold(0, |mask, button| mask | button.mask())
    }
}

#[cfg(test)]
#[path = "pointer_tests.rs"]
mod tests;
