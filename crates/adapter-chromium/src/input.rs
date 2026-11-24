//! Input actions as plans of low-level steps (mcp-surface.md, Input). A plan is fixed by the action's
//! actionability checks and persisted before its first step, so a recorder fault resumes it exactly.
use cdp::protocol::input;
use frontcap_core::action::Action;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TouchKind {
    Start,
    Move,
    End,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseKind {
    Press,
    Release,
    Move,
    /// A move with the button held. The patched engine delivers it while frozen; wait for its receipt
    /// before the next move so moves cannot coalesce. The pre-send marker prevents duplicate dispatch
    /// on recovery; a marker without a delivered receipt remains explicit missing-effect evidence.
    Drag,
}

/// A file an upload sets: the bundle's copy, and what the trace records of it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct UploadFile {
    pub path: String,
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum InputStep {
    CancelMousePointer {},
    CancelHtmlDrag { x: f64, y: f64 },
    Mouse {
        kind: MouseKind,
        x: f64,
        y: f64,
        count: u8,
        button: frontcap_core::pointer::PointerButton,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        buttons: Option<u8>,
        /// CDP modifier bits of the keys held meanwhile.
        #[serde(default)]
        modifiers: u8,
    },
    Wheel {
        x: f64,
        y: f64,
        dx: f64,
        dy: f64,
        #[serde(default)]
        modifiers: u8,
    },
    Key { down: bool, key: String, code: String, key_code: i64, text: Option<String>, modifiers: u8, #[serde(default, skip_serializing_if = "is_false")] repeat: bool },
    InsertText { text: String },
    /// Native touch transition. A named End lifts one contact; unnamed End/Cancel clears the set.
    Touch {
        kind: TouchKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contact: Option<u32>,
        x: f64,
        y: f64,
        #[serde(default)]
        modifiers: u8,
        /// A contact (or, without one, any contact) started over another document than the session's
        /// first: Chrome sends its pointer events to the element under it and its touch events to that
        /// first document, or none when it lies outside that document's frame.
        #[serde(default, skip_serializing_if = "is_false")]
        other_document: bool,
    },
    /// `Input.imeSetComposition` on the focused element; empty text cancels the composition.
    Compose { text: String },
    /// Commits the composition with its text, as an input method does.
    CommitComposition { text: String },
    /// Script steps, run by the observer world on the target; they write their own receipt. `at_end`
    /// puts the caret after the current content, so typed text appends.
    Focus {
        #[serde(default)]
        at_end: bool,
    },
    SelectContents,
    SelectOptions { values: Vec<String> },
    /// Sets the files of the input whose file chooser the steps before opened (a "Choose…" button in
    /// front of a hidden input): `Page.fileChooserOpened` names it.
    ChooseFiles { files: Vec<UploadFile> },
    /// Caller-authorized virtual progress before the next move. Its goal is persisted by the recorder
    /// before dispatch, so recovery finishes that same interval rather than granting it twice.
    Advance { by_us: u64 },
    /// `DOM.setFileInputFiles` on the target; its receipt is the `change` (or, for the same files again,
    /// `cancel`) event.
    SetFiles { files: Vec<UploadFile> },
}

impl InputStep {
    /// The page event whose receipt proves the step arrived; `None` for script steps.
    pub fn receipt_event(&self) -> Option<&'static str> {
        match self {
            InputStep::CancelMousePointer {} => Some("pointercancel"),
            InputStep::CancelHtmlDrag { .. } => Some("dragend"),
            InputStep::Mouse { kind: MouseKind::Press, button, buttons: Some(buttons), .. } if buttons & !button.mask() != 0 => Some("pointermove"),
            InputStep::Mouse { kind: MouseKind::Release, buttons: Some(buttons), .. } if *buttons != 0 => Some("pointermove"),
            InputStep::Mouse { kind: MouseKind::Press, .. } => Some("pointerdown"),
            InputStep::Mouse { kind: MouseKind::Release, .. } => Some("pointerup"),
            InputStep::Mouse { kind: MouseKind::Move, .. } => Some("pointermove"),
            InputStep::Mouse { kind: MouseKind::Drag, .. } | InputStep::Advance { .. } => None,
            InputStep::SetFiles { .. } | InputStep::ChooseFiles { .. } => Some("change"),
            InputStep::Wheel { .. } => Some("wheel"),
            InputStep::Key { down: true, .. } => Some("keydown"),
            InputStep::Key { down: false, .. } => Some("keyup"),
            InputStep::InsertText { .. } => Some("beforeinput"),
            InputStep::Touch { kind: TouchKind::Start, other_document: true, .. } => Some("pointerdown"),
            InputStep::Touch { kind: TouchKind::Move, other_document: true, .. } => Some("pointermove"),
            InputStep::Touch { kind: TouchKind::End, other_document: true, .. } => Some("pointerup"),
            InputStep::Touch { kind: TouchKind::Cancel, other_document: true, .. } => Some("pointercancel"),
            InputStep::Touch { kind: TouchKind::Start, .. } => Some("touchstart"),
            InputStep::Touch { kind: TouchKind::Move, .. } => Some("touchmove"),
            InputStep::Touch { kind: TouchKind::End, .. } => Some("touchend"),
            InputStep::Touch { kind: TouchKind::Cancel, .. } => Some("touchcancel"),
            InputStep::Compose { text } if text.is_empty() => Some("compositionend"),
            InputStep::Compose { .. } => Some("compositionupdate"),
            InputStep::CommitComposition { .. } => Some("compositionend"),
            InputStep::Focus { .. } | InputStep::SelectContents | InputStep::SelectOptions { .. } => None,
        }
    }

    /// Whether the step always reaches the page, so a missing receipt after dispatch is a violation.
    /// Script steps write their receipt themselves. Inserted text reaches nothing when the page has moved
    /// the focus to an element that is not editable; the text is then lost, as it would be in a browser,
    /// and nothing reports it.
    pub fn always_delivers(&self) -> bool {
        matches!(self, InputStep::CancelMousePointer {} | InputStep::CancelHtmlDrag { .. } | InputStep::Mouse { kind: MouseKind::Press | MouseKind::Release | MouseKind::Move, .. } | InputStep::SetFiles { .. } | InputStep::ChooseFiles { .. } | InputStep::Wheel { .. } | InputStep::Key { .. } | InputStep::Touch { .. } | InputStep::Compose { .. } | InputStep::CommitComposition { .. } | InputStep::Focus { .. } | InputStep::SelectContents | InputStep::SelectOptions { .. })
    }

    pub fn mouse_params(&self) -> Option<input::DispatchMouseEventParams> {
        use input::DispatchMouseEventParamsType as T;
        match *self {
            InputStep::Mouse { kind, x, y, count, button: held, modifiers, buttons: held_buttons } => {
                let (held_button, held_mask) = match held {
                    frontcap_core::pointer::PointerButton::Left => (input::MouseButton::Left, 1),
                    frontcap_core::pointer::PointerButton::Right => (input::MouseButton::Right, 2),
                    frontcap_core::pointer::PointerButton::Middle => (input::MouseButton::Middle, 4),
                };
                let (t, button, buttons) = match kind {
                    MouseKind::Press => (T::MousePressed, held_button, None),
                    MouseKind::Release => (T::MouseReleased, held_button, None),
                    // A move with a button is a drag in CDP terms (S14).
                    MouseKind::Move => (T::MouseMoved, input::MouseButton::None, None),
                    // `buttons` is what the page's pointer events report as held.
                    MouseKind::Drag => (T::MouseMoved, held_button, Some(held_mask)),
                };
                let buttons = held_buttons.map(i64::from).or(buttons);
                let modifiers = (modifiers != 0).then_some(i64::from(modifiers));
                Some(input::DispatchMouseEventParams { button: Some(button), buttons, click_count: Some(i64::from(count)), modifiers, ..input::DispatchMouseEventParams::new(t, x, y) })
            }
            InputStep::Wheel { x, y, dx, dy, modifiers } => {
                let modifiers = (modifiers != 0).then_some(i64::from(modifiers));
                Some(input::DispatchMouseEventParams { delta_x: Some(dx), delta_y: Some(dy), modifiers, ..input::DispatchMouseEventParams::new(T::MouseWheel, x, y) })
            }
            _ => None,
        }
    }

    pub fn touch_params(&self) -> Option<input::DispatchTouchEventParams> {
        use input::DispatchTouchEventParamsType as T;
        let InputStep::Touch { kind, contact, x, y, modifiers, .. } = *self else { return None };
        let finger = input::TouchPoint {
            x, y, radius_x: None, radius_y: None, rotation_angle: None, force: None, tangential_pressure: None, tilt_x: None, tilt_y: None, twist: None, id: Some(f64::from(contact.unwrap_or(0))),
        };
        let (t, points) = match kind {
            TouchKind::Start => (T::TouchStart, vec![finger]),
            TouchKind::Move => (T::TouchMove, vec![finger]),
            TouchKind::End => (T::TouchEnd, if contact.is_some() { vec![finger] } else { vec![] }),
            TouchKind::Cancel => (T::TouchCancel, vec![]),
        };
        Some(input::DispatchTouchEventParams { modifiers: (modifiers != 0).then_some(i64::from(modifiers)), ..input::DispatchTouchEventParams::new(t, points) })
    }

    pub fn key_params(&self) -> Option<input::DispatchKeyEventParams> {
        use input::DispatchKeyEventParamsType as T;
        let InputStep::Key { down, key, code, key_code, text, modifiers, repeat } = self else { return None };
        // A key with text is `keyDown` (it inserts its text); one without is `rawKeyDown`, as a browser
        // reports a non-printing key.
        let t = match (down, text) {
            (true, Some(_)) => T::KeyDown,
            (true, None) => T::RawKeyDown,
            (false, _) => T::KeyUp,
        };
        Some(input::DispatchKeyEventParams {
            auto_repeat: Some(*repeat),
            location: match code.as_str() { "ShiftLeft" | "ControlLeft" | "AltLeft" | "MetaLeft" => Some(1), "ShiftRight" | "ControlRight" | "AltRight" | "MetaRight" => Some(2), _ => None },
            key: Some(key.clone()),
            code: Some(code.clone()),
            windows_virtual_key_code: Some(*key_code),
            native_virtual_key_code: Some(*key_code),
            text: if *down { text.clone() } else { None },
            unmodified_text: if *down { text.clone() } else { None },
            modifiers: (*modifiers != 0).then_some(i64::from(*modifiers)),
            ..input::DispatchKeyEventParams::new(t)
        })
    }
}

/// Adds the bits of held modifier keys to every native step, as a keyboard reports them with the input.
pub fn hold_modifiers(steps: &mut [InputStep], held: u8) {
    for s in steps {
        if let InputStep::Mouse { modifiers, .. } | InputStep::Wheel { modifiers, .. } | InputStep::Key { modifiers, .. } | InputStep::Touch { modifiers, .. } = s {
            *modifiers |= held;
        }
    }
}

/// What an input action will do, fixed by its actionability checks; persisted before dispatch.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InputPlan {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer_context: Option<frontcap_core::pointer::PointerContext>,
    pub action: Action,
    /// Backend node id of the resolved target; `None` for an action without one.
    pub node: Option<i64>,
    pub steps: Vec<InputStep>,
    /// Where the plan was made: the instant of its first event and the document it acts on. A drag
    /// lets virtual time move (S17), so its record keeps the instant it began.
    #[serde(default)]
    pub start_us: frontcap_core::time::VirtualTime,
    #[serde(default)]
    pub document: u64,
}

impl InputPlan {
    /// The substeps whose receipt must exist once the plan has run.
    pub fn expected(&self) -> Vec<usize> {
        self.steps.iter().enumerate().filter(|(_, s)| s.always_delivers()).map(|(i, _)| i).collect()
    }
}

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;

fn is_false(value: &bool) -> bool { !*value }
