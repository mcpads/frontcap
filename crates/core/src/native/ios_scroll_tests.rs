use super::*;
fn scroll()->Scroll {
    Scroll { token:"abcd0000-0000-0000-0000-000000000001".into(),role:Role::Table,
        identifier:"list".into(),label:"".into(),offset_mpt:[0,-10000],minimum_mpt:[0,-10000],
        maximum_mpt:[0,1000000],viewport_mpt:[300000,600000],enabled:true,dragging:false,decelerating:false }
}
#[test]
fn scroll_bounds_distinguish_finite_end_motion_and_zero_delivery() {
    let mut before=scroll();before.validate().unwrap();
    assert!(before.allows(Direction::Down));assert!(!before.allows(Direction::Up));
    assert!(!before.allows(Direction::Left));assert!(!before.allows(Direction::Right));
    validate_delivery(&before,&before,Direction::Down).unwrap(); // No movement is not finite end.
    let mut after=before.clone();after.offset_mpt[1]=after.maximum_mpt[1];
    validate_delivery(&before,&after,Direction::Down).unwrap();assert!(!after.allows(Direction::Down));
    assert!(after.allows(Direction::Up));after.decelerating=true;assert!(!after.allows(Direction::Up));
    after.decelerating=false;after.offset_mpt[1]+=1;assert!(!after.allows(Direction::Up));
    after.token="abcd0000-0000-0000-0000-000000000002".into();
    assert!(validate_delivery(&before,&after,Direction::Down).is_err());
    before.minimum_mpt[1]=before.maximum_mpt[1]+1;assert!(before.validate().is_err());
}
#[test]
fn scroll_requires_one_ready_observed_source_with_matching_selector() {
    use crate::native::{ios_targets::Target,ios_text::TextInput};
    let source=scroll();let observation=source.token.clone();let reference=format!("{observation}-0");
    let mut catalog=Catalog { observation,keyboard:false,transitioning:false,text_input:TextInput::Unfocused {},
        scrolls:vec![source.clone()],targets:vec![Target { role:Role::Table,element_type:26,
        identifier:"list".into(),label:"".into(),value:None,selector_key:SelectorKey::Identifier,
        selector_value:"list".into(),unique:true,enabled:true,selected:false,hittable:true,
        intersects_screen:true,frame_mpt:[0,0,300000,600000],reference:reference.clone() }] };
    catalog.validate().unwrap();assert_eq!(target(&catalog,&reference,Direction::Down).unwrap(),&source);
    assert!(target(&catalog,&reference,Direction::Up).is_err());
    catalog.scrolls.push(source);assert!(target(&catalog,&reference,Direction::Down).is_err());
    assert!(catalog.validate().is_err());catalog.scrolls.pop();catalog.transitioning=true;
    assert!(target(&catalog,&reference,Direction::Down).is_err());
}
