use super::*;
fn catalog() -> Catalog {
    let observation = "abcd0000-0000-0000-0000-000000000001".to_string();
    Catalog { scrolls: vec![],
        transitioning: false,
        text_input: crate::native::ios_text::TextInput::Unfocused {},
        observation: observation.clone(),
        keyboard: false,
        targets: vec![Target {
            role: Role::Button,
            element_type: 9,
            identifier: "".into(),
            label: "Save".into(),
            value: None,
            selector_key: SelectorKey::Label,
            selector_value: "Save".into(),
            unique: true,
            enabled: true,
            selected: false,
            hittable: true,
            intersects_screen: true,
            frame_mpt: [1000, 2000, 30000, 40000],
            reference: format!("{observation}-0"),
        }],
    }
}
#[test]
fn reference_and_uniqueness_are_verified_not_trusted() {
    let mut value = catalog();
    value.validate().unwrap();
    assert!(valid_reference(
        &value.targets[0].reference,
        &value.observation
    ));
    assert!(!valid_reference(
        &format!("{}-00", value.observation),
        &value.observation
    ));
    assert!(!valid_reference(
        &format!("{}-2048", value.observation),
        &value.observation
    ));
    value.targets[0].reference = format!("{}-1", value.observation);
    assert!(value.validate().is_err());
    value = catalog();
    let mut duplicate = value.targets[0].clone();
    duplicate.reference = format!("{}-1", value.observation);
    value.targets.push(duplicate);
    assert!(value.validate().is_err());
    for target in &mut value.targets {
        target.unique = false;
        target.hittable = false;
    }
    value.validate().unwrap();
    value.targets[0].selector_value = "Delete".into();
    assert!(value.validate().is_err());
}
#[test]
fn readiness_and_unicode_values_preserve_real_boundaries() {
    let mut value = catalog();
    value.targets[0].value = Some(Value::Text("부산 🌊".into()));
    let bytes = serde_json::to_vec(&value).unwrap();
    assert_eq!(serde_json::from_slice::<Catalog>(&bytes).unwrap(), value);
    value.targets[0].enabled = false;
    assert!(value.validate().is_err());
    value.targets[0].hittable = false;
    value.targets[0].intersects_screen = false;
    value.targets[0].frame_mpt[1] = -2000;
    value.validate().unwrap();
    value.targets[0].element_type = 75;
    assert!(value.validate().is_err());
    value = catalog();
    value.targets[0].value = Some(Value::Text("🌊".repeat(8193)));
    assert!(value.validate().is_err());
}

#[test]
fn static_text_is_observable_but_cannot_advertise_tap_readiness() {
    let mut value = catalog();
    let row = &mut value.targets[0];
    row.role = Role::StaticText;
    row.element_type = 48;
    row.label = "부산 여행 🌊".into();
    row.selector_value = row.label.clone();
    assert!(value.validate().is_err());
    value.targets[0].hittable = false;
    value.validate().unwrap();
    let decoded: Catalog = serde_json::from_slice(&serde_json::to_vec(&value).unwrap()).unwrap();
    assert_eq!(decoded, value);
}
