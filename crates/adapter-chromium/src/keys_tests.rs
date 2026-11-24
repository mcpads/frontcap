use super::*;

fn keys(steps: &[InputStep]) -> Vec<(bool, String, u8)> {
    steps
        .iter()
        .map(|s| match s {
            InputStep::Key { down, key, modifiers, .. } => (*down, key.clone(), *modifiers),
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn named_keys_have_their_code_and_text() {
    let enter = named("Enter").unwrap();
    assert_eq!((enter.code, enter.key_code, enter.text.as_deref()), ("Enter", 13, Some("\r")));
    assert_eq!(named("a").unwrap().code, "KeyA");
    assert_eq!(named("!").unwrap().code, "Digit1");
    assert_eq!(named("Escape").unwrap().text, None);
    assert!(named("Ctrl").is_none());
}

#[test]
fn a_chord_presses_modifiers_first_and_releases_them_last() {
    let steps = chord("Control+Shift+K").unwrap();
    assert_eq!(
        keys(&steps),
        vec![
            (true, "Control".into(), 2),
            (true, "Shift".into(), 10),
            (true, "K".into(), 10),
            (false, "K".into(), 10),
            (false, "Shift".into(), 2),
            (false, "Control".into(), 0),
        ]
    );
    let InputStep::Key { key, .. } = &chord("Control+Shift+k").unwrap()[2] else { unreachable!() };
    assert_eq!(key, "K");
    let InputStep::Key { text, .. } = &steps[2] else { unreachable!() };
    assert_eq!(text, &None, "a key with Control inserts no text");
    assert_eq!(keys(&chord("Shift++").unwrap())[1], (true, "+".into(), 8));
    assert!(chord("Ctrl+K").unwrap_err().contains("Ctrl"));
    assert!(chord("").is_err());
}

#[test]
fn typed_characters_use_keys_when_the_table_has_them() {
    assert_eq!(keys(&typed('a')), vec![(true, "a".into(), 0), (false, "a".into(), 0)]);
    assert_eq!(keys(&typed('\n'))[0].1, "Enter");
    assert_eq!(typed('한'), vec![InputStep::InsertText { text: "한".into() }]);
}

#[test]
fn shifted_characters_hold_shift_like_a_keyboard() {
    assert_eq!(
        keys(&typed('A')),
        vec![(true, "Shift".into(), 8), (true, "A".into(), 8), (false, "A".into(), 8), (false, "Shift".into(), 0)]
    );
    let InputStep::Key { text, .. } = &typed('!')[1] else { unreachable!() };
    assert_eq!(text.as_deref(), Some("!"));
    let shift_a = chord("Shift+a").unwrap();
    let InputStep::Key { key, text, .. } = &shift_a[1] else { unreachable!() };
    assert_eq!((key.as_str(), text.as_deref()), ("A", Some("A")));
}

#[test]
fn plus_tab_and_line_breaks_are_keys() {
    assert_eq!(keys(&chord("+").unwrap())[0].1, "+");
    assert_eq!(typed('\t'), vec![InputStep::InsertText { text: "\t".into() }]);
    let shift_release = chord("Shift+a").unwrap();
    assert_eq!(keys(&shift_release).last(), Some(&(false, "Shift".to_string(), 0)), "a browser reports Shift released with shiftKey false");
    assert_eq!(typed_text("a\r\nb").iter().filter(|s| matches!(s, InputStep::Key { down: true, key, .. } if key == "Enter")).count(), 1);
}


#[test]
fn held_physical_codes_follow_shift_without_aliasing_right_modifiers() {
    let shifted = held("KeyA", 8).unwrap();
    assert_eq!((shifted.key.as_str(), shifted.code, shifted.text.as_deref()), ("A", "KeyA", Some("A")));
    assert_eq!(held("Digit1", 8).unwrap().key, "!");
    assert_eq!(held("Slash", 8).unwrap().key, "?");
    assert_eq!(held("ShiftRight", 0).unwrap().code, "ShiftRight");
    assert_eq!(modifier_bit("ShiftRight"), Some(8));
    let right_chord = chord("ShiftRight+a").unwrap();
    assert_eq!(right_chord[0].key_params().unwrap().code.as_deref(), Some("ShiftRight"));
    let both = chord("ShiftLeft+ShiftRight+a").unwrap();
    assert_eq!(both[4].key_params().unwrap().modifiers, Some(8));
    assert_eq!(both[5].key_params().unwrap().modifiers, None);
    assert!(chord("Shift+ShiftLeft+a").is_err());
    assert_eq!(held("KeyA", 2).unwrap().text, None);
    assert!(held("KeyUnknown", 0).is_none());
}
