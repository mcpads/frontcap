//! What an agent asks `act` to do (mcp-surface.md, Tools). Durations are microseconds, as in the trace.
use crate::predicate::Locator;
use serde::{Deserialize, Serialize};

#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    PointerBegin {
        target: Locator,
        point: [i32; 2],
        #[serde(default)]
        button: crate::pointer::PointerButton,
        #[serde(default, skip_serializing_if = "crate::pointer::PointerKind::is_mouse")]
        pointer: crate::pointer::PointerKind,
        boundary: crate::observation::ObservationBoundary,
    },
    PointerMove { handle: crate::pointer::PointerHandle, point: [i32; 2], boundary: crate::observation::ObservationBoundary },
    PointerPress { handle: crate::pointer::PointerHandle, button: crate::pointer::PointerButton, boundary: crate::observation::ObservationBoundary },
    PointerRelease { handle: crate::pointer::PointerHandle, button: crate::pointer::PointerButton },
    TouchAdd { handle: crate::pointer::PointerHandle, target: Locator, point: [i32; 2], boundary: crate::observation::ObservationBoundary },
    TouchMove { handle: crate::pointer::PointerHandle, contact: u32, point: [i32; 2], boundary: crate::observation::ObservationBoundary },
    TouchEnd { handle: crate::pointer::PointerHandle, contact: u32 },
    PointerEnd { handle: crate::pointer::PointerHandle },
    PointerCancel { handle: crate::pointer::PointerHandle },
    Wheel { target: Locator, point: [i32; 2], delta: [i32; 2], boundary: crate::observation::ObservationBoundary },
    /// Presses a key and keeps it held: a held modifier applies to the input that follows until `KeyUp`.
    KeyDown { key: String },
    KeyUp { key: String },
    /// One explicit native repeat of an already-held key, without advancing time.
    KeyRepeat { key: String },
    /// Sets the focused editable element's composition (an input method's uncommitted text); empty text
    /// cancels it.
    Compose { text: String },
    /// Commits the composition, by `key` as an input method consumes it (`Enter`) or without a key.
    Commit {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
    },
    Click { target: Locator },
    /// Click an exact viewport CSS point after checking its observation boundary and hit recipient.
    ClickAt { target: Locator, point: [i32; 2], boundary: crate::observation::ObservationBoundary },
    Dblclick { target: Locator },
    Hover { target: Locator },
    /// Move the mouse, no button pressed, to an exact viewport CSS point after checking its observation
    /// boundary and hit recipient.
    HoverAt { target: Locator, point: [i32; 2], boundary: crate::observation::ObservationBoundary },
    /// Every character as its own key events (or inserted text when no key produces it).
    Type { target: Locator, text: String },
    /// One key or a chord such as `Control+Shift+K`, to `target` or, without one, the focused element.
    Press {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Locator>,
        key: String,
    },
    /// Replaces the whole content of an editable element.
    Fill { target: Locator, value: String },
    /// Chooses options of a `<select>` by value or, failing that, by label.
    Select { target: Locator, values: Vec<String> },
    /// Scrolls the target's nearest scrollable ancestor, or the document without a target.
    Scroll {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Locator>,
        delta_x: i32,
        delta_y: i32,
    },
    /// Presses on `target`, moves with the button held to `to` (an element's center) or by `offset`, and
    /// releases there: a pointer drag, and an HTML drag and drop when the target is draggable. The whole
    /// drag happens at one virtual instant unless `interval_us` explicitly grants progress before
    /// each distinct move; `steps` is the number of moves (default 5).
    Drag {
        target: Locator,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<Locator>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<Offset>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        steps: Option<u32>,
        #[serde(default)]
        interval_us: u64,
    },
    /// Sets the files of an `<input type=file>` (hidden ones too) as a user's choice would, firing
    /// `input` and `change`. Each file is copied into the bundle and read from there, with its
    /// modification time at the profile's initial date.
    Upload { target: Locator, files: Vec<String> },
    Navigate { url: String },
    Back {},
    Reload {},
    /// Closes the popup window the act addresses, as its user would: its `beforeunload` runs (the dialog
    /// rules answer a prompt; staying keeps the window open), then the window closes.
    Close {},
    /// Advances virtual time until `target` is actionable, re-judging at every frame tick.
    WaitActionable { target: Locator, max_us: u64 },
}

/// A distance in CSS pixels.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offset {
    pub dx: i32,
    pub dy: i32,
}

/// How an action is executed and recovered (runtime-lifecycle.md, Mutation requests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    /// Low-level input events with per-substep receipts.
    Input,
    /// Replaces the document; its evidence does not survive the document (engine patch class).
    Navigation,
    /// Advances virtual time; recovered as a clock request.
    Wait,
    /// Closes a popup window, whose document holds its evidence: recovered by whether the window is gone.
    Close,
}

impl Action {
    pub fn name(&self) -> &'static str {
        match self {
            Action::PointerBegin { .. } => "pointer_begin",
            Action::PointerMove { .. } => "pointer_move",
            Action::PointerPress { .. } => "pointer_press",
            Action::PointerRelease { .. } => "pointer_release",
            Action::TouchAdd { .. } => "touch_add",
            Action::TouchMove { .. } => "touch_move",
            Action::TouchEnd { .. } => "touch_end",
            Action::PointerEnd { .. } => "pointer_end",
            Action::PointerCancel { .. } => "pointer_cancel",
            Action::Wheel { .. } => "wheel",
            Action::KeyDown { .. } => "key_down",
            Action::KeyUp { .. } => "key_up",
            Action::KeyRepeat { .. } => "key_repeat",
            Action::Compose { .. } => "compose",
            Action::Commit { .. } => "commit",
            Action::Click { .. } => "click",
            Action::ClickAt { .. } => "click_at",
            Action::Dblclick { .. } => "dblclick",
            Action::Hover { .. } => "hover",
            Action::HoverAt { .. } => "hover_at",
            Action::Type { .. } => "type",
            Action::Press { .. } => "press",
            Action::Fill { .. } => "fill",
            Action::Select { .. } => "select",
            Action::Scroll { .. } => "scroll",
            Action::Drag { .. } => "drag",
            Action::Upload { .. } => "upload",
            Action::Navigate { .. } => "navigate",
            Action::Back {} => "back",
            Action::Reload {} => "reload",
            Action::Close {} => "close",
            Action::WaitActionable { .. } => "wait_actionable",
        }
    }

    pub fn target(&self) -> Option<&Locator> {
        match self {
            Action::Click { target }
            | Action::PointerBegin { target, .. } | Action::TouchAdd { target, .. }
            | Action::Wheel { target, .. }
            | Action::ClickAt { target, .. } | Action::HoverAt { target, .. }
            | Action::Dblclick { target }
            | Action::Hover { target }
            | Action::Type { target, .. }
            | Action::Fill { target, .. }
            | Action::Select { target, .. }
            | Action::Drag { target, .. }
            | Action::Upload { target, .. }
            | Action::WaitActionable { target, .. } => Some(target),
            Action::Press { target, .. } | Action::Scroll { target, .. } => target.as_ref(),
            Action::Navigate { .. } | Action::Back {} | Action::Reload {} | Action::Close {} | Action::PointerMove { .. } | Action::PointerPress { .. } | Action::PointerRelease { .. } | Action::TouchMove { .. } | Action::TouchEnd { .. } | Action::PointerEnd { .. } | Action::PointerCancel { .. }
            | Action::KeyDown { .. } | Action::KeyUp { .. } | Action::KeyRepeat { .. } | Action::Compose { .. } | Action::Commit { .. } => None,
        }
    }

    /// Every locator the action uses: its target and, for a drag, its destination.
    pub fn locators(&self) -> Vec<&Locator> {
        let mut out: Vec<&Locator> = self.target().into_iter().collect();
        if let Action::Drag { to: Some(to), .. } = self {
            out.push(to);
        }
        out
    }

    /// The arguments other than the target, as recorded in the trace; `None` when there are none.
    pub fn args(&self) -> Option<serde_json::Value> {
        let mut v = serde_json::to_value(self).expect("actions serialize");
        let map = v.as_object_mut().expect("a tagged object");
        map.remove("type");
        map.remove("target");
        // The boundary is an acquisition precondition retained in command inputs, not an effect.
        map.remove("boundary");
        if let Some(handle) = map.get_mut("handle").and_then(serde_json::Value::as_object_mut) {
            handle.remove("launch_id");
        }
        (!map.is_empty()).then_some(v)
    }

    pub fn kind(&self) -> ActionKind {
        match self {
            Action::Navigate { .. } | Action::Back {} | Action::Reload {} => ActionKind::Navigation,
            Action::WaitActionable { .. } => ActionKind::Wait,
            Action::Close {} => ActionKind::Close,
            _ => ActionKind::Input,
        }
    }

    pub fn is_pointer(&self) -> bool {
        matches!(self, Action::TouchAdd { .. } | Action::PointerBegin { .. } | Action::PointerMove { .. } | Action::PointerPress { .. } | Action::PointerRelease { .. } | Action::TouchMove { .. } | Action::TouchEnd { .. } | Action::PointerEnd { .. } | Action::PointerCancel { .. } | Action::Wheel { .. })
    }

    /// Input of the `input` drawer: it never settles, answers with the held input, and what it holds is
    /// released when the lease is lost.
    pub fn is_held_input(&self) -> bool {
        self.is_pointer() || matches!(self, Action::KeyDown { .. } | Action::KeyUp { .. } | Action::KeyRepeat { .. } | Action::Compose { .. } | Action::Commit { .. })
    }
}

#[cfg(test)]
#[path = "action_tests.rs"]
mod tests;
