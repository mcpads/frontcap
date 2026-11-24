use super::*;
use frontcap_core::predicate::Locator;

#[test]
fn each_step_names_the_event_that_proves_it_arrived() {
    let mouse = |kind| InputStep::Mouse { buttons: None, kind, x: 1.0, y: 2.0, count: 1, button: Default::default(), modifiers: 0 };
    assert_eq!(mouse(MouseKind::Press).receipt_event(), Some("pointerdown"));
    assert_eq!(mouse(MouseKind::Release).receipt_event(), Some("pointerup"));
    assert_eq!(mouse(MouseKind::Move).receipt_event(), Some("pointermove"));
    let key = |down| InputStep::Key { repeat: false, down, key: "a".into(), code: "KeyA".into(), key_code: 65, text: Some("a".into()), modifiers: 0 };
    assert_eq!((key(true).receipt_event(), key(false).receipt_event()), (Some("keydown"), Some("keyup")));
    assert_eq!(InputStep::InsertText { text: "한".into() }.receipt_event(), Some("beforeinput"));
    assert_eq!(InputStep::Focus { at_end: false }.receipt_event(), None);
}

#[test]
fn a_move_carries_no_button_and_a_key_without_text_is_raw() {
    let m = InputStep::Mouse { buttons: None, kind: MouseKind::Move, x: 1.0, y: 2.0, count: 0, button: Default::default(), modifiers: 0 }.mouse_params().unwrap();
    assert_eq!(m.button, Some(input::MouseButton::None));
    let k = InputStep::Key { repeat: false, down: true, key: "Shift".into(), code: "ShiftLeft".into(), key_code: 16, text: None, modifiers: 8 }.key_params().unwrap();
    assert_eq!(k.r#type, input::DispatchKeyEventParamsType::RawKeyDown);
    assert_eq!(k.modifiers, Some(8));
}

#[test]
fn plans_round_trip_because_they_are_persisted() {
    let plan = InputPlan { pointer_context: None,
        start_us: Default::default(),
        document: 0,
        action: Action::Type { target: Locator::Css { selector: "#q".into() }, text: "a".into() },
        node: Some(7),
        steps: vec![InputStep::Focus { at_end: false }, InputStep::InsertText { text: "a".into() }],
    };
    let back: InputPlan = serde_json::from_str(&serde_json::to_string(&plan).unwrap()).unwrap();
    assert_eq!(back, plan);
    assert_eq!(plan.expected(), vec![0]);
}


#[test]
fn legacy_key_steps_remain_non_repeating_and_keep_their_serialized_shape() {
    let old = serde_json::json!({"step":"key","down":true,"key":"a","code":"KeyA","key_code":65,"text":"a","modifiers":0});
    let step: InputStep = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(step.key_params().unwrap().auto_repeat, Some(false));
    assert_eq!(serde_json::to_value(step).unwrap(), old);
}


#[test]
fn legacy_mouse_steps_keep_implicit_buttons_and_chords_name_native_transition_receipts() {
    let old=serde_json::json!({"step":"mouse","kind":"press","x":40.0,"y":40.0,"count":1,"button":"left","modifiers":0});
    let step: InputStep=serde_json::from_value(old.clone()).unwrap();
    assert_eq!(step.receipt_event(),Some("pointerdown"));
    assert_eq!(serde_json::to_value(step).unwrap(),old);
    let step=InputStep::Mouse { kind:MouseKind::Press,x:40.0,y:40.0,count:1,button:frontcap_core::pointer::PointerButton::Right,buttons:Some(3),modifiers:0 };
    assert_eq!(step.receipt_event(),Some("pointermove"));
    assert_eq!(step.mouse_params().unwrap().buttons,Some(3));
    let step=InputStep::Mouse { kind:MouseKind::Release,x:40.0,y:40.0,count:1,button:frontcap_core::pointer::PointerButton::Left,buttons:Some(2),modifiers:0 };
    assert_eq!(step.receipt_event(),Some("pointermove"));
    assert_eq!(step.mouse_params().unwrap().buttons,Some(2));
}


#[test]
fn legacy_touch_steps_keep_contact_zero_and_targeted_end_names_only_one_contact() {
    let old=serde_json::json!({"step":"touch","kind":"start","x":40.0,"y":40.0,"modifiers":0});
    let step:InputStep=serde_json::from_value(old.clone()).unwrap();
    assert_eq!(step.touch_params().unwrap().touch_points[0].id,Some(0.0));
    assert_eq!(serde_json::to_value(step).unwrap(),old);
    let step=InputStep::Touch{kind:TouchKind::End,contact:Some(3),x:80.0,y:40.0,modifiers:0,other_document:false};
    let native=step.touch_params().unwrap();
    assert_eq!(native.touch_points.len(),1);
    assert_eq!(native.touch_points[0].id,Some(3.0));
    let all=InputStep::Touch{kind:TouchKind::End,contact:None,x:80.0,y:40.0,modifiers:0,other_document:false};
    assert!(all.touch_params().unwrap().touch_points.is_empty());
}

#[test]
fn a_contact_over_another_document_is_received_by_its_pointer_events() {
    let touch=|kind,other_document|InputStep::Touch{kind,contact:Some(1),x:60.0,y:60.0,modifiers:0,other_document};
    let kinds=[TouchKind::Start,TouchKind::Move,TouchKind::End,TouchKind::Cancel];
    assert_eq!(kinds.map(|k|touch(k,false).receipt_event()),[Some("touchstart"),Some("touchmove"),Some("touchend"),Some("touchcancel")]);
    assert_eq!(kinds.map(|k|touch(k,true).receipt_event()),[Some("pointerdown"),Some("pointermove"),Some("pointerup"),Some("pointercancel")]);
    // The flag is left out of records that do not need it, so earlier records read back unchanged.
    assert!(serde_json::to_value(touch(TouchKind::Start,false)).unwrap().get("other_document").is_none());
    let step:InputStep=serde_json::from_value(serde_json::to_value(touch(TouchKind::Start,true)).unwrap()).unwrap();
    assert_eq!(step.receipt_event(),Some("pointerdown"));
}
