use super::*;
use crate::native::ios_targets::Target;

fn receipt() -> Receipt { serde_json::from_str(include_str!("../../tests/fixtures/uikit-edit.json")).unwrap() }
#[test]
fn source_edit_binds_owner_exact_range_and_basis_without_predicting_delegate_output() {
    let r=receipt();
    let app=ClockIdentity { simulator:"abcd0000-0000-0000-0000-000000000001".into(),
        generation:"abcdef00-0000-4000-8000-000000000001".into(),pid:r.pid };
    let check=|r:Receipt| Delivery::UikitTextControl { receipt:Box::new(r) }.validate(&app,0,0,"Field 🌟");
    check(r.clone()).unwrap();
    for mutate in [
        (|r:&mut Receipt| r.pid+=1) as fn(&mut Receipt),
        |r| r.replayed=true,
        |r| r.request.arguments.text="different".into(),
        |r| r.last_id+=1,
        |r| if let SourceResult::UikitTextInput { before:TextInput::Focused { text,.. },.. }=&mut r.result { *text="changed".into(); },
    ] { let mut bad=r.clone();mutate(&mut bad);assert!(check(bad).is_err()); }
    let mut transformed=r;
    if let SourceResult::UikitTextInput { after,.. }=&mut transformed.result { *after=TextInput::Unfocused {}; }
    check(transformed).unwrap(); // A normal delegate may change focus; retain actual state.
}
#[test]
fn observed_edit_rejects_surrogate_splits_wrong_field_composition_and_result_overflow() {
    let SourceResult::UikitTextInput { after,.. }=receipt().result else { panic!() };
    let observation="abcd0000-0000-0000-0000-000000000001".to_owned();
    let reference=format!("{observation}-0");
    let mut catalog=Catalog { scrolls: vec![], observation,keyboard:false,transitioning:false,text_input:after,
        targets:vec![Target { role:Role::TextField,element_type:49,identifier:"".into(),label:"Field".into(),
            value:None,selector_key:SelectorKey::Label,selector_value:"Field".into(),unique:true,enabled:true,
            selected:false,hittable:true,intersects_screen:true,frame_mpt:[0,0,1000,1000],reference:reference.clone() }] };
    catalog.validate().unwrap();validate_target(&catalog,&reference,3,5,"주말 여행").unwrap();
    assert!(validate_target(&catalog,&reference,7,7,"x").is_err());
    assert!(validate_target(&catalog,&reference,8,8,&"가".repeat(16384)).is_err());
    catalog.targets[0].selector_value="Body".into();
    assert!(validate_target(&catalog,&reference,0,0,"").is_err());
    catalog.targets[0].selector_value="Field".into();
    if let TextInput::Focused { marked,.. }=&mut catalog.text_input { *marked=true; }
    assert!(validate_target(&catalog,&reference,0,0,"").is_err());
}
