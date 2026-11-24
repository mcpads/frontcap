use super::*;
#[test]
fn focused_ranges_use_utf16_and_reject_invalid_identity_or_bounds() {
    let mut value = TextInput::Focused {
        token: "abcd0000-0000-0000-0000-000000000001".into(),
        role: TextRole::TextField,
        identifier: "".into(),
        label: "Title".into(),
        text: "부산 여행 🌊".into(),
        start: 8,
        end: 8,
        marked: false,
    };
    value.validate().unwrap();
    assert_eq!(
        serde_json::from_slice::<TextInput>(&serde_json::to_vec(&value).unwrap()).unwrap(),
        value
    );
    if let TextInput::Focused { end, .. } = &mut value {
        *end = 9;
    }
    assert!(value.validate().is_err());
    if let TextInput::Focused {
        start, end, token, ..
    } = &mut value
    {
        *start = 0;
        *end = 0;
        *token = "foreign".into();
    }
    assert!(value.validate().is_err());
    assert!(
        serde_json::from_str::<TextInput>(
            r#"{"state":"unsupported","reason":"secure_text","text":"secret"}"#
        )
        .is_err()
    );
    assert!(
        serde_json::from_str::<TextInput>(r#"{"state":"unfocused","text":"secret"}"#).is_err()
    );
}
