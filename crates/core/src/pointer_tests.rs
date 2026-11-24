use super::*;

#[test]
fn legacy_pointer_hold_retains_its_button_and_serialized_shape() {
    let old=serde_json::json!({"request_id":"begin","document_id":1,"point":[40,40],"button":"right"});
    let held:PointerHold=serde_json::from_value(old.clone()).unwrap();
    assert_eq!(held.mouse_buttons(),vec![PointerButton::Right]);
    assert_eq!(held.button_mask(),2);
    assert_eq!(serde_json::to_value(held).unwrap(),old);
}


#[test]
fn legacy_touch_hold_starts_with_contact_zero_and_keeps_serialized_shape() {
    let old=serde_json::json!({"request_id":"begin","document_id":1,"point":[40,40],"button":"left","pointer":"touch"});
    let held:PointerHold=serde_json::from_value(old.clone()).unwrap();
    assert_eq!(held.touch_contacts(),vec![TouchContact{id:0,point:[40,40],other_document:false}]);
    assert_eq!(held.next_contact_id,1);
    assert!(held.mouse_buttons().is_empty());
    assert_eq!(serde_json::to_value(held).unwrap(),old);
}
