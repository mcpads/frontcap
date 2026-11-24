use super::*;
use crate::input::TouchKind;
use frontcap_core::pointer::{HeldPointer, PointerHandle, PointerHold, PointerKind, PointerState, TouchContact};

impl Generation {
    pub async fn pointer_context_valid(&mut self) -> Result<bool, ChromiumError> {
        let Some(held) = self.held_pointer.clone() else { return Ok(true) };
        if self.left_document(held.document_id) { return Ok(false); }
        let Some(native) = held.native else { return Ok(true) };
        Ok(self.pointer_frame_context(&native.frame_id).await?.as_ref() == Some(&native))
    }

    async fn pointer_frame_context(&mut self, frame_id: &str) -> Result<Option<frontcap_core::pointer::PointerContext>, ChromiumError> {
        let mut stack = vec![self.page.call(&page::GetFrameTreeParams::default()).await?.frame_tree];
        while let Some(tree) = stack.pop() {
            if tree.frame.id.0 == frame_id {
                return Ok(Some(frontcap_core::pointer::PointerContext { frame_id: tree.frame.id.0, loader_id: tree.frame.loader_id.0 }));
            }
            stack.extend(tree.child_frames.into_iter().flatten());
        }
        Ok(None)
    }
    pub fn held_pointer(&self) -> Option<HeldPointer> {
        self.held_pointer.as_ref().map(|p| HeldPointer {
            handle: PointerHandle { launch_id: self.launch_id.clone(), request_id: p.request_id.clone() },
            document_id: p.document_id, point: p.point, html_drag: p.html_drag, button: p.button, buttons: p.mouse_buttons(), contacts: p.touch_contacts(), pointer: p.pointer,
            context: p.context.clone(),
        })
    }

    /// The context held input continues in: the held pointer's for an action on it, the held keys' for a
    /// key, the composition's for one; `None` when nothing is held there (an action starts its own).
    pub fn held_input_context(&self, action: &ActAction) -> Option<Option<String>> {
        match action {
            ActAction::PointerMove { .. } | ActAction::PointerPress { .. } | ActAction::PointerRelease { .. }
            | ActAction::PointerEnd { .. } | ActAction::PointerCancel { .. } | ActAction::TouchAdd { .. }
            | ActAction::TouchMove { .. } | ActAction::TouchEnd { .. } => self.held_pointer.as_ref().map(|p| p.context.clone()),
            ActAction::KeyDown { .. } | ActAction::KeyUp { .. } | ActAction::KeyRepeat { .. } => (!self.held_keys.is_empty()).then(|| self.keys_context.clone()),
            ActAction::Compose { .. } | ActAction::Commit { .. } => self.composing.then(|| self.compose_context.clone()),
            _ => None,
        }
    }

    pub fn held_keys(&self) -> &[String] {
        &self.held_keys
    }

    /// The order held keys are released in when input is cleaned up: other keys before modifiers, each
    /// last pressed first.
    pub fn key_release_order(&self) -> Vec<String> {
        let (modifiers, others): (Vec<&String>, Vec<&String>) = self.held_keys.iter().rev().partition(|k| crate::keys::modifier_bit(k).is_some());
        others.into_iter().chain(modifiers).cloned().collect()
    }

    /// Whether a composition may be in progress: cleanup cancels one if the page still has it.
    pub fn may_compose(&self) -> bool {
        self.composing
    }

    /// The focused document's composition text while one is in progress.
    pub async fn current_composition(&mut self) -> Result<Option<String>, ChromiumError> {
        // Read in the window the composition is in; a closed one took it with it.
        let context = self.compose_context.clone();
        let entered = match self.enter_for(context.as_deref()) {
            Ok(entered) => entered,
            Err(ChromiumError::ContextClosed(_)) => return Ok(None),
            Err(e) => return Err(e),
        };
        let composition = self.composition().await;
        if entered { self.leave_context(); }
        composition
    }

    /// Held input, read in the window it is held in (the pointer's, else the keys' or the
    /// composition's), whose boundary later input names.
    pub async fn pointer_state(&mut self) -> Result<PointerState, ChromiumError> {
        let context = self.held_pointer.as_ref().map(|p| p.context.clone())
            .or_else(|| (!self.held_keys.is_empty()).then(|| self.keys_context.clone()))
            .or_else(|| self.composing.then(|| self.compose_context.clone()))
            .flatten();
        let entered = match self.enter_for(context.as_deref()) {
            Ok(entered) => entered,
            // Its window is gone: what is held is read with the launch page.
            Err(ChromiumError::ContextClosed(_)) => false,
            Err(e) => return Err(e),
        };
        let state: Result<PointerState, ChromiumError> = async {
            let composition = self.composition().await?;
            Ok(PointerState { boundary: self.observation_boundary().await?, held: self.held_pointer(), keys: self.held_keys.clone(), composition })
        }.await;
        if entered { self.leave_context(); }
        state
    }

    /// The focused document's composition text while one is in progress, as the page's events report it.
    pub(super) async fn composition(&mut self) -> Result<Option<String>, ChromiumError> {
        Ok(self.world_eval("globalThis.__frontcap.composition()").await?.as_str().map(str::to_string))
    }

    /// The CDP modifier bits of the held keys.
    pub(super) fn held_modifiers(&self) -> u8 {
        self.held_keys.iter().filter_map(|k| crate::keys::modifier_bit(k)).fold(0, |a, b| a | b)
    }

    /// Keys held, and compositions set and committed. Keys reach the focused element; a held modifier
    /// is reported with every input that follows until it is released.
    pub(super) async fn key_plan(&mut self, action: &ActAction) -> Result<InputPlan, ChromiumError> {
        let held = self.held_modifiers();
        let named = |key: &str| crate::keys::held(key, held).ok_or_else(|| ChromiumError::InvalidArgument(format!("unknown key {key}")));
        let composition = self.composition().await?;
        let steps = match action {
            ActAction::KeyDown { key } => {
                let def = named(key)?;
                if self.held_keys.iter().any(|held| crate::keys::held(held, 0).is_some_and(|h| h.code == def.code)) { return Err(ChromiumError::InvalidArgument(format!("physical key {} is already held", def.code))); }
                if composition.is_some() { return Err(ChromiumError::InvalidArgument("commit or cancel the composition before holding a key".into())); }
                self.focus_reachable().await?;
                let modifiers = held | crate::keys::modifier_bit(key).unwrap_or(0);
                vec![InputStep::Key { repeat: false, down: true, key: def.key, code: def.code.into(), key_code: def.key_code, text: def.text, modifiers }]
            }
            ActAction::KeyRepeat { key } => {
                let def = named(key)?;
                if !self.held_keys.contains(key) { return Err(ChromiumError::InvalidArgument(format!("{key} is not held"))); }
                if crate::keys::modifier_bit(key).is_some() { return Err(ChromiumError::InvalidArgument("modifier keys do not repeat".into())); }
                if composition.is_some() { return Err(ChromiumError::InvalidArgument("commit or cancel the composition before repeating a key".into())); }
                self.focus_reachable().await?;
                vec![InputStep::Key { repeat: true, down: true, key: def.key, code: def.code.into(), key_code: def.key_code, text: def.text, modifiers: held }]
            }
            ActAction::KeyUp { key } => {
                let def = named(key)?;
                if !self.held_keys.contains(key) { return Err(ChromiumError::InvalidArgument(format!("{key} is not held"))); }
                self.focus_reachable().await?;
                let modifiers = self.held_keys.iter().filter(|k| *k != key).filter_map(|k| crate::keys::modifier_bit(k)).fold(0, |a, b| a | b);
                vec![InputStep::Key { repeat: false, down: false, key: def.key, code: def.code.into(), key_code: def.key_code, text: None, modifiers }]
            }
            ActAction::Compose { text } => {
                if text.is_empty() && composition.is_none() { return Err(ChromiumError::InvalidArgument("no composition to cancel".into())); }
                if composition.is_none() && self.world_eval("globalThis.__frontcap.focusEditable()").await? != serde_json::Value::Bool(true) {
                    return Err(ChromiumError::NotActionable("a composition needs a focused editable element in a reachable document".into()));
                }
                vec![InputStep::Compose { text: text.clone() }]
            }
            ActAction::Commit { key } => {
                let Some(text) = composition else { return Err(ChromiumError::InvalidArgument("no composition to commit".into())) };
                match key {
                    // An input method consumes the key that commits: its keydown is reported as processed
                    // (key code 229) while the composition is in progress, its keyup after it ends.
                    Some(key) => {
                        let def = named(key)?;
                        vec![
                            InputStep::Key { repeat: false, down: true, key: def.key.clone(), code: def.code.into(), key_code: 229, text: None, modifiers: held },
                            InputStep::CommitComposition { text },
                            InputStep::Key { repeat: false, down: false, key: def.key, code: def.code.into(), key_code: def.key_code, text: None, modifiers: held },
                        ]
                    }
                    None => vec![InputStep::CommitComposition { text }],
                }
            }
            _ => unreachable!("key action"),
        };
        Ok(InputPlan { pointer_context: None, action: action.clone(), node: None, steps, start_us: self.now, document: self.documents })
    }

    /// Keys reach the focused element; one in an unobserved frame cannot be observed receiving them.
    async fn focus_reachable(&mut self) -> Result<(), ChromiumError> {
        if self.world_eval("globalThis.__frontcap.unobservedFrameFocused()").await? == serde_json::Value::Bool(true) {
            return Err(ChromiumError::NotActionable(format!("focus is in an unobserved frame: {UNOBSERVED_FRAME}")));
        }
        Ok(())
    }

    async fn pointer_owner(&mut self, handle: &PointerHandle) -> Result<PointerHold, ChromiumError> {
        let held = self.held_pointer.as_ref().filter(|p| handle.launch_id == self.launch_id && handle.request_id == p.request_id)
            .cloned().ok_or_else(|| ChromiumError::InvalidArgument("pointer handle does not own this generation's held input".into()))?;
        if !self.pointer_context_valid().await? {
            return Err(ChromiumError::NotActionable("the held pointer's document changed".into()));
        }
        Ok(held)
    }

    async fn touch_owner(&mut self, handle: &PointerHandle) -> Result<PointerHold, ChromiumError> {
        let held = self.pointer_owner(handle).await?;
        if held.pointer != PointerKind::Touch { return Err(ChromiumError::InvalidArgument("this handle owns a mouse, not touch contacts".into())); }
        Ok(held)
    }

    pub(super) async fn pointer_plan(&mut self, action: &ActAction) -> Result<InputPlan, ChromiumError> {
        use crate::input::MouseKind;
        if !self.patched { return Err(ChromiumError::Unsupported("pointer sessions require the patched input scheduler".into())); }
        let mut pointer_context = None;
        let (node, steps) = match action {
            ActAction::Wheel { target, point, delta, boundary } => {
                if self.held_pointer.is_some() { return Err(ChromiumError::InvalidArgument("end the held pointer before wheel input".into())); }
                if *delta == [0, 0] { return Err(ChromiumError::InvalidArgument("wheel delta must be nonzero".into())); }
                self.checked_pointer_point(target, boundary, [f64::from(point[0]), f64::from(point[1])]).await?;
                let node = self.resolve(target).await?;
                (Some(node.0), vec![InputStep::Wheel { x: point[0].into(), y: point[1].into(), dx: delta[0].into(), dy: delta[1].into(), modifiers: 0 }])
            }
            ActAction::PointerBegin { target, point, button, pointer, boundary } => {
                if self.held_pointer.is_some() { return Err(ChromiumError::InvalidArgument("end the current pointer session before beginning another".into())); }
                let touch = *pointer == PointerKind::Touch;
                if touch && !self.profile.touch { return Err(ChromiumError::InvalidArgument("touch needs a profile with touch".into())); }
                if touch && *button != frontcap_core::pointer::PointerButton::Left { return Err(ChromiumError::InvalidArgument("a touch has no button".into())); }
                self.checked_pointer_point(target, boundary, [f64::from(point[0]), f64::from(point[1])]).await?;
                let hit = self.hit_test_viewport([point[0].into(), point[1].into()]).await?;
                if hit.frame_id != self.main_frame {
                    pointer_context = self.pointer_frame_context(&hit.frame_id.0).await?;
                    if pointer_context.is_none() { return Err(ChromiumError::NotActionable("the pointer's child document disappeared".into())); }
                }
                let node = self.resolve(target).await?;
                let (x, y) = (point[0].into(), point[1].into());
                let step = if touch { InputStep::Touch { contact: None, kind: TouchKind::Start, x, y, modifiers: 0, other_document: false } } else { InputStep::Mouse { buttons: None, kind: MouseKind::Press, x, y, count: 1, button: *button, modifiers: 0 } };
                (Some(node.0), vec![step])
            }
            ActAction::PointerMove { handle, point, boundary } => {
                self.expect_boundary(boundary).await?;
                let held = self.pointer_owner(handle).await?;
                if point[0] < 0 || point[1] < 0 || i64::from(point[0]) >= i64::from(self.profile.viewport.width) || i64::from(point[1]) >= i64::from(self.profile.viewport.height) {
                    return Err(ChromiumError::InvalidArgument("pointer point is outside the viewport".into()));
                }
                let (x, y) = (point[0].into(), point[1].into());
                if held.touch_contacts().len() > 1 { return Err(ChromiumError::InvalidArgument("use touch_move with a contact ID while multiple contacts are held".into())); }
                let steps = if held.point == *point { vec![] } else if held.pointer == PointerKind::Touch {
                    let only = &held.touch_contacts()[0];
                    vec![InputStep::Touch { contact: Some(only.id), kind: TouchKind::Move, x, y, modifiers: 0, other_document: only.other_document }]
                } else {
                    vec![InputStep::Mouse { buttons: Some(held.button_mask()), kind: MouseKind::Drag, x, y, count: 0, button: held.button, modifiers: 0 }]
                };
                (None, steps)
            }
            ActAction::TouchAdd { handle, target, point, boundary } => {
                let held = self.touch_owner(handle).await?;
                if held.touch_contacts().len() >= 5 { return Err(ChromiumError::InvalidArgument("the touch profile supports at most five contacts".into())); }
                if held.next_contact_id >= i32::MAX as u32 { return Err(ChromiumError::InvalidArgument("touch contact IDs exhausted; end this session".into())); }
                self.checked_pointer_point(target, boundary, [point[0].into(), point[1].into()]).await?;
                let hit = self.hit_test_viewport([point[0].into(), point[1].into()]).await?;
                let owner = held.native.as_ref().map(|n| n.frame_id.as_str()).unwrap_or(self.main_frame.0.as_str());
                // Chrome keeps a touch sequence's touch events in its first contact's document.
                let other_document = hit.frame_id.0 != owner;
                pointer_context = held.native.clone();
                let node = self.resolve(target).await?;
                (Some(node.0), vec![InputStep::Touch { contact: Some(held.next_contact_id), kind: TouchKind::Start, x: point[0].into(), y: point[1].into(), modifiers: 0, other_document }])
            }
            ActAction::TouchMove { handle, contact, point, boundary } => {
                self.expect_boundary(boundary).await?;
                let held = self.touch_owner(handle).await?;
                let contacts = held.touch_contacts();
                let current = contacts.iter().find(|c| c.id == *contact).ok_or_else(|| ChromiumError::InvalidArgument("the touch contact is not held".into()))?;
                if point[0] < 0 || point[1] < 0 || i64::from(point[0]) >= i64::from(self.profile.viewport.width) || i64::from(point[1]) >= i64::from(self.profile.viewport.height) {
                    return Err(ChromiumError::InvalidArgument("touch point is outside the viewport".into()));
                }
                pointer_context = held.native.clone();
                let steps = if current.point == *point { vec![] } else { vec![InputStep::Touch { contact: Some(*contact), kind: TouchKind::Move, x: point[0].into(), y: point[1].into(), modifiers: 0, other_document: current.other_document }] };
                (None, steps)
            }
            ActAction::TouchEnd { handle, contact } => {
                let held = self.touch_owner(handle).await?;
                let contacts = held.touch_contacts();
                let current = contacts.iter().find(|c| c.id == *contact).ok_or_else(|| ChromiumError::InvalidArgument("the touch contact is not held".into()))?;
                pointer_context = held.native.clone();
                (None, vec![InputStep::Touch { contact: Some(*contact), kind: TouchKind::End, x: current.point[0].into(), y: current.point[1].into(), modifiers: 0, other_document: current.other_document }])
            }
            ActAction::PointerPress { handle, button, boundary } => {
                self.expect_boundary(boundary).await?;
                let held = self.pointer_owner(handle).await?;
                if held.pointer != PointerKind::Mouse { return Err(ChromiumError::InvalidArgument("touch contacts have no mouse buttons".into())); }
                if held.html_drag { return Err(ChromiumError::InvalidArgument("use end or cancel for the active HTML drag".into())); }
                if held.mouse_buttons().contains(button) { return Err(ChromiumError::InvalidArgument("the mouse button is already held".into())); }
                pointer_context = held.native.clone();
                (None, vec![InputStep::Mouse { buttons: Some(held.button_mask() | button.mask()), kind: MouseKind::Press, x: held.point[0].into(), y: held.point[1].into(), count: 1, button: *button, modifiers: 0 }])
            }
            ActAction::PointerRelease { handle, button } => {
                let held = self.pointer_owner(handle).await?;
                if held.pointer != PointerKind::Mouse { return Err(ChromiumError::InvalidArgument("touch contacts have no mouse buttons".into())); }
                if held.html_drag { return Err(ChromiumError::InvalidArgument("use end or cancel for the active HTML drag".into())); }
                if !held.mouse_buttons().contains(button) { return Err(ChromiumError::InvalidArgument("the mouse button is not held".into())); }
                pointer_context = held.native.clone();
                (None, vec![InputStep::Mouse { buttons: Some(held.button_mask() & !button.mask()), kind: MouseKind::Release, x: held.point[0].into(), y: held.point[1].into(), count: 1, button: *button, modifiers: 0 }])
            }
            ActAction::PointerEnd { handle } => {
                let held = self.pointer_owner(handle).await?;
                let (x, y) = (held.point[0].into(), held.point[1].into());
                let steps = if held.pointer == PointerKind::Touch {
                    vec![InputStep::Touch { contact: None, kind: TouchKind::End, x, y, modifiers: 0, other_document: held.touch_contacts().iter().any(|c| c.other_document) }]
                } else {
                    let mut mask = held.button_mask();
                    held.mouse_buttons().into_iter().rev().map(|button| {
                        mask &= !button.mask();
                        InputStep::Mouse { buttons: Some(mask), kind: MouseKind::Release, x, y, count: 1, button, modifiers: 0 }
                    }).collect()
                };
                (None, steps)
            }
            ActAction::PointerCancel { handle } => {
                let held = self.pointer_owner(handle).await?;
                let step = if held.html_drag {
                    InputStep::CancelHtmlDrag { x: held.point[0].into(), y: held.point[1].into() }
                } else if held.pointer == PointerKind::Touch {
                    InputStep::Touch { contact: None, kind: TouchKind::Cancel, x: held.point[0].into(), y: held.point[1].into(), modifiers: 0, other_document: held.touch_contacts().iter().any(|c| c.other_document) }
                } else {
                    InputStep::CancelMousePointer {}
                };
                (None, vec![step])
            }
            _ => unreachable!("pointer action"),
        };
        let mut steps = steps;
        crate::input::hold_modifiers(&mut steps, self.held_modifiers());
        Ok(InputPlan { pointer_context, action: action.clone(), node, steps, start_us: self.now, document: self.documents })
    }

    /// Holds what the plan presses before its input is sent, so a fault meanwhile still releases it.
    pub(super) fn pointer_started(&mut self, request: &RequestId, plan: &InputPlan) {
        match &plan.action {
            ActAction::PointerBegin { point, button, pointer, .. } => {
                self.held_pointer = Some(PointerHold { native: plan.pointer_context.clone(), request_id: request.clone(), point: *point, html_drag: false, button: *button, buttons: vec![], contacts: vec![], next_contact_id: 1, document_id: plan.document, pointer: *pointer, context: self.popup_id() });
            }
            ActAction::PointerPress { button, .. } => {
                if let Some(held) = self.held_pointer.as_mut() {
                    let mut buttons = held.mouse_buttons();
                    if !buttons.contains(button) { buttons.push(*button); }
                    held.buttons = buttons;
                }
            }
            ActAction::TouchAdd { point, .. } => {
                if let (Some(held), Some(InputStep::Touch { contact: Some(id), other_document, .. })) = (self.held_pointer.as_mut(), plan.steps.first()) {
                    let mut contacts = held.touch_contacts();
                    if !contacts.iter().any(|c| c.id == *id) { contacts.push(TouchContact { id: *id, point: *point, other_document: *other_document }); }
                    held.contacts = contacts;
                    held.next_contact_id = held.next_contact_id.max(*id + 1);
                }
            }
            ActAction::KeyDown { key } if !self.held_keys.contains(key) => {
                if self.held_keys.is_empty() { self.keys_context = self.popup_id(); }
                self.held_keys.push(key.clone());
            }
            _ => {}
        }
    }

    pub(super) fn pointer_finished(&mut self, request: &RequestId, plan: &InputPlan) {
        match &plan.action {
            ActAction::PointerBegin { .. } | ActAction::PointerPress { .. } | ActAction::TouchAdd { .. } => self.pointer_started(request, plan),
            ActAction::PointerRelease { button, .. } => {
                if let Some(held) = self.held_pointer.as_mut() {
                    let mut buttons = held.mouse_buttons();
                    buttons.retain(|b| b != button);
                    if let Some(first) = buttons.first() { held.button = *first; held.buttons = buttons; }
                    else { self.held_pointer = None; }
                }
            }
            ActAction::PointerMove { point, .. } => {
                if let Some(held) = self.held_pointer.as_mut() {
                    held.point = *point;
                    if let Some(first) = held.contacts.first_mut() { first.point = *point; }
                }
            }
            ActAction::TouchMove { contact, point, .. } => {
                if let Some(held) = self.held_pointer.as_mut() {
                    let mut contacts = held.touch_contacts();
                    if let Some(current) = contacts.iter_mut().find(|c| c.id == *contact) { current.point = *point; }
                    if let Some(first) = contacts.first() { held.point = first.point; }
                    held.contacts = contacts;
                }
            }
            ActAction::TouchEnd { contact, .. } => {
                if let Some(held) = self.held_pointer.as_mut() {
                    let mut contacts = held.touch_contacts();
                    contacts.retain(|c| c.id != *contact);
                    if let Some(first) = contacts.first() { held.point = first.point; held.contacts = contacts; }
                    else { self.held_pointer = None; }
                }
            }
            ActAction::PointerEnd { .. } | ActAction::PointerCancel { .. } => self.held_pointer = None,
            ActAction::KeyDown { .. } => self.pointer_started(request, plan),
            ActAction::KeyUp { key } => {
                self.held_keys.retain(|k| k != key);
                if self.held_keys.is_empty() { self.keys_context = None; }
            }
            ActAction::Compose { text } => {
                self.composing = !text.is_empty();
                self.compose_context = if self.composing { self.popup_id() } else { None };
            }
            ActAction::Commit { .. } => {
                self.composing = false;
                self.compose_context = None;
            }
            _ => {}
        }
    }
}
