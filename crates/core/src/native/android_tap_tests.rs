use super::*;
#[test]
fn tap_delivery_requires_both_edges_and_preserves_pending_uncertainty() {
    let id="00000000-0000-0000-0000-000000000001";
    let request=Request {generation:id.into(),controller_id:id.into(),request_id:1,observation:id.into(),target:id.into(),now_ns:0,x:100,y:200};
    let mut receipt=Receipt {request:request.clone(),generation:id.into(),pid:12,now_ns:0,last_id:1,state:State::Pending,down_handled:None,up_handled:None,reason:None};
    assert!(receipt.validate(&request,id,12).is_ok());
    receipt.state=State::Completed;receipt.down_handled=Some(true);
    assert!(receipt.validate(&request,id,12).is_err());
    receipt.up_handled=Some(false);assert!(receipt.validate(&request,id,12).is_ok());
    receipt.now_ns=1;assert!(receipt.validate(&request,id,12).is_err());
    receipt.now_ns=0;receipt.reason=Some(Reason::StaleObservation);assert!(receipt.validate(&request,id,12).is_err());
    receipt.state=State::Rejected;receipt.down_handled=None;receipt.up_handled=None;
    assert!(receipt.validate(&request,id,12).is_ok());
    assert!(receipt.validate(&request,id,13).is_err());
    let mut other=request.clone();other.x+=1;assert!(receipt.validate(&other,id,12).is_err());
}
