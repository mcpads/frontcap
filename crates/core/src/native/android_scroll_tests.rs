use super::*;
use crate::native::{android_tap::State,android_views::{View,Role}};
fn view()->View {
    View {token:"abcd0000-0000-0000-0000-000000000001".into(),class_name:"android.widget.ScrollView".into(),
        role:Role::ScrollView,resource_id:String::new(),label:String::new(),text:None,error:None,scroll:None,
        edit_limit_utf16:None,tap_point_screen_px:None,secure:false,enabled:true,visible:true,focused:false,
        clickable:false,selected:false,transformed:false,layout_bounds_screen_px:[0,0,500,1000]}
}
fn geometry()->Geometry {
    Geometry {offset_px:[0,0],maximum_px:[0,5000],viewport_px:[500,1000],gestures:vec![Gesture {
        direction:Direction::Down,start_px:[250,750],end_px:[250,250]}]}
}
#[test]
fn directions_require_remaining_range_and_supported_visible_geometry() {
    let mut v=view();let mut g=geometry();g.validate(&v).unwrap();
    g.offset_px=[0,5000];assert!(g.validate(&v).is_err());
    g.gestures.clear();g.validate(&v).unwrap(); // Current finite endpoint, no available downward gesture.
    g=geometry();v.visible=false;assert!(g.validate(&v).is_err());v.visible=true;
    v.class_name="custom.ScrollView".into();assert!(g.validate(&v).is_err());v=view();
    g.gestures[0].end_px=[250,800];assert!(g.validate(&v).is_err());
    g=geometry();g.gestures[0].start_px=[500,750];assert!(g.validate(&v).is_err());
    g=geometry();g.gestures.push(g.gestures[0].clone());assert!(g.validate(&v).is_err());
    g=geometry();v.transformed=true;assert!(g.validate(&v).is_err());
}
#[test]
fn delivery_requires_bound_owner_moves_and_offsets_but_does_not_promise_movement() {
    let id=view().token;
    let request=Request {generation:id.clone(),controller_id:id.clone(),request_id:1,observation:id.clone(),target:id.clone(),
        now_ns:0,x:250,y:750,end_x:250,end_y:250,direction:Direction::Down};
    let mut receipt=Receipt {request:request.clone(),generation:id.clone(),pid:12,now_ns:0,last_id:1,state:State::Completed,
        reason:None,down_handled:Some(true),up_handled:Some(true),moves:Some(20),before_offset_px:Some([0,0]),after_offset_px:Some([0,0])};
    receipt.validate(&request,&id,12).unwrap(); // Window delivery is not proof of content movement or end.
    assert!(receipt.validate(&request,&id,13).is_err());
    receipt.moves=Some(0);assert!(receipt.validate(&request,&id,12).is_err());
    receipt.moves=Some(20);receipt.after_offset_px=None;assert!(receipt.validate(&request,&id,12).is_err());
    receipt.state=State::Pending;assert!(receipt.validate(&request,&id,12).is_err());
    receipt.down_handled=None;receipt.up_handled=None;receipt.moves=None;receipt.before_offset_px=None;
    receipt.validate(&request,&id,12).unwrap();
}
