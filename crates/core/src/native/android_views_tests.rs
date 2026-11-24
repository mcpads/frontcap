use super::*;
const REQUEST: &str = "abcd0000-0000-0000-0000-000000000001";
const GENERATION: &str = "abcd0000-0000-0000-0000-000000000002";
fn catalog() -> Catalog {
    Catalog { window_focused: true, ime_visible: Some(false), targets: vec![View {
        token: REQUEST.into(), class_name: "android.widget.EditText".into(), role: Role::TextField,
        resource_id: String::new(), label: "Title".into(), text: Some("부산 🌊".into()), secure: false,
        enabled: true, visible: true, focused: true, clickable: true, selected: false, transformed: false,
        error: None, scroll: None, tap_point_screen_px: None, edit_limit_utf16: Some(5000), layout_bounds_screen_px: [0,20,300,60],
    }] }
}
#[test]
fn observation_requires_exact_request_and_live_owner() {
    for observation in [Observation::Observed { request: REQUEST.into(), generation: GENERATION.into(), pid: 123,
        now_ns: 1000000, catalog: catalog() }, Observation::Unavailable { request: REQUEST.into(), generation: GENERATION.into(),
        pid: 123, now_ns: 1000000, reason: UnavailableReason::WindowNotLaidOut }] {
        observation.validate(REQUEST,GENERATION,123).unwrap();
        assert!(observation.validate(GENERATION,GENERATION,123).is_err());
        assert!(observation.validate(REQUEST,REQUEST,123).is_err());
        assert!(observation.validate(REQUEST,GENERATION,124).is_err());
    }
}
#[test]
fn hidden_source_nodes_are_not_hittability_claims_and_invalid_snapshots_reject() {
    let mut value = catalog(); value.targets[0].visible = false; value.targets[0].layout_bounds_screen_px = [-300,0,0,40];
    value.validate().unwrap(); // Negative/offscreen bounds remain observable, not actionable.
    let mut duplicate = value.clone(); duplicate.targets.push(value.targets[0].clone()); assert!(duplicate.validate().is_err());
    let mut secure = value.clone(); secure.targets[0].secure = true; assert!(secure.validate().is_err());
    secure.targets[0].text = None; secure.targets[0].edit_limit_utf16 = None; secure.validate().unwrap();
    let mut bad_limit = value.clone(); bad_limit.targets[0].edit_limit_utf16 = Some(16385); assert!(bad_limit.validate().is_err());
    bad_limit.targets[0].edit_limit_utf16 = Some(5000); bad_limit.targets[0].role = Role::Button; assert!(bad_limit.validate().is_err());
    let mut overflow = value.clone(); overflow.targets[0].layout_bounds_screen_px = [0,0,i64::MAX,40]; assert!(overflow.validate().is_err());
    let mut point = catalog(); point.targets[0].tap_point_screen_px = Some([150,40]); point.validate().unwrap();
    point.targets[0].visible = false; assert!(point.validate().is_err());
    point.targets[0].visible = true; point.targets[0].tap_point_screen_px = Some([300,40]); assert!(point.validate().is_err());
    let mut huge = value; huge.targets[0].text = Some("🌊".repeat(8193)); assert!(huge.validate().is_err());
}

#[test]
fn validation_error_is_bounded_and_redacted_for_secure_fields() {
    let mut value = catalog();
    value.targets[0].error = Some("Add a title".into());
    value.validate().unwrap();
    let encoded = serde_json::to_vec(&value).unwrap();
    assert_eq!(serde_json::from_slice::<Catalog>(&encoded).unwrap(), value);
    value.targets[0].error = Some("😀".repeat(2049));
    assert!(value.validate().is_err());
    value.targets[0].error = Some("Add a title".into());
    value.targets[0].secure = true;
    value.targets[0].text = None;
    value.targets[0].edit_limit_utf16 = None;
    assert!(value.validate().is_err());
    value.targets[0].error = None;
    value.validate().unwrap();
}
