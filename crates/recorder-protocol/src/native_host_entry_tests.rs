use super::*;
use frontcap_core::native::{ios_host::{Owner,Entry,EntryAcceptance,Operation,Completion,Ready as HostReady}, external_entry::Observation, ios_ui::UiIdentity};
#[test]
fn native_cold_entry_fences_app_actions_and_transfers_only_valid_original_replacement() {
    let owner=Owner {simulator:"ABCD0000-0000-0000-0000-000000000001".into(),process:frontcap_core::process::ProcessIdentity {pid:42,started:"original".into()},attachment_sha256:"a".repeat(64)};
    let runner=UiIdentity {simulator:owner.simulator.clone(),pid:43,generation:"abcd0000-0000-0000-0000-000000000002".into()};
    let ready=Ready::IosHost {owner:owner.clone(),contacts:true,pages:true,system_ui:Some(runner.clone()),cold_entry:true,entry:None,app_lifecycle:false,visibility:None};
    let entry=Entry {begin:RequestId::new("begin").unwrap(),acceptance:EntryAcceptance {previous:owner.clone(),url:"consumer://history".into(),confirmation:Observation::Unobserved,destination:Observation::Unobserved}};
    let pending=ready.entry_transition(&Outcome::IosHostCompleted {completion:Completion::EntryBegun {entry:Box::new(entry.clone())}}).unwrap().unwrap();
    let op=|operation|Op::ExecuteIosHost {operation};
    for operation in [Operation::ObserveSystemAlerts,Operation::PollEntry {begin:entry.begin.clone()}] {pending.validate_operation(&op(operation)).unwrap();}
    pending.validate_operation(&Op::Stop).unwrap();
    for operation in [Operation::BeginEntry {previous:owner.clone(),url:entry.acceptance.url.clone()},Operation::Restart {previous:owner.clone()},Operation::Observe {input_basis:false},Operation::DiscoverPages,Operation::PollEntry {begin:RequestId::new("foreign").unwrap()}] {
        assert!(pending.validate_operation(&op(operation)).is_err());
    }
    assert!(pending.validate_operation(&Op::Observe).is_err());
    let waiting=Outcome::IosHostCompleted {completion:Completion::EntryPolled {begin:entry.begin.clone(),ready:None}};
    assert_eq!(pending.entry_transition(&waiting).unwrap(),Some(pending.clone()));assert!(ready.entry_transition(&waiting).is_err());
    let mut replacement=HostReady {owner:Owner {process:frontcap_core::process::ProcessIdentity {pid:44,started:"replacement".into()},..owner},contacts:Some(frontcap_core::native::contacts::Endpoint {pid:44,generation:"abcd0000-0000-0000-0000-000000000004".into(),available:true,short_tap:false,observed_contacts:true,request_capacity:4096}),pages:true,app_lifecycle:false,system_ui:Some(UiIdentity {pid:45,generation:"abcd0000-0000-0000-0000-000000000005".into(),..runner.clone()})};
    let complete=|ready|Outcome::IosHostCompleted {completion:Completion::EntryPolled {begin:entry.begin.clone(),ready:Some(Box::new(ready))}};
    let next=pending.entry_transition(&complete(replacement.clone())).unwrap().unwrap();
    next.validate_operation(&Op::Observe).unwrap();assert!(next.entry_transition(&waiting).is_err());
    replacement.system_ui=Some(runner);assert!(pending.entry_transition(&complete(replacement)).is_err());
    let mut disabled=ready;let Ready::IosHost {cold_entry,..}=&mut disabled else {panic!()};*cold_entry=false;
    assert!(disabled.entry_transition(&Outcome::IosHostCompleted {completion:Completion::EntryBegun {entry:Box::new(entry)}}).is_err());
}

#[test]
fn native_host_visibility_fences_app_work_and_requires_same_process_runner_evidence() {
    use frontcap_core::native::{app_state::State, ios_ui::{UiRequest,UiOperation,UiCompletion,UiOutcome,HostAppStateChange}};
    let owner=Owner {simulator:"ABCD0000-0000-0000-0000-000000000001".into(),process:frontcap_core::process::ProcessIdentity {pid:42,started:"original".into()},attachment_sha256:"a".repeat(64)};
    let runner=UiIdentity {simulator:owner.simulator.clone(),generation:"abcd0000-0000-0000-0000-000000000002".into(),pid:43};
    let ready=Ready::IosHost {owner:owner.clone(),contacts:true,pages:true,system_ui:Some(runner.clone()),cold_entry:true,entry:None,app_lifecycle:true,visibility:None};
    let op=|state|Op::ExecuteIosHost {operation:Operation::SetAppState {owner:Box::new(owner.clone()),state}};
    let completion=UiCompletion {generation:runner.generation.clone(),pid:runner.pid,last_id:1,receipt_sha256:"b".repeat(64),
        request:UiRequest {generation:runner.generation.clone(),request_id:1,operation:UiOperation::SetHostAppState {bundle:"dev.example.app".into(),pid:owner.process.pid,state:State::Background}},
        outcome:UiOutcome::HostAppStateChanged {change:HostAppStateChange {bundle:"dev.example.app".into(),pid:owner.process.pid,requested:State::Background,before_state:4,app_state:3}}};
    let transition=frontcap_core::native::ios_host::StateTransition {owner:owner.clone(),before:owner.process.clone(),after:owner.process.clone(),completion,directory:"/original/system-ui/client".into()};
    let result=|t|Outcome::IosHostCompleted {completion:Completion::StateChanged {transition:Box::new(t)}};
    let background=ready.visibility_transition(&result(transition.clone())).unwrap().unwrap();
    for denied in [Op::Observe,op(State::Background),Op::ExecuteIosHost {operation:Operation::DiscoverPages},Op::ExecuteIosHost {operation:Operation::BeginEntry {previous:owner.clone(),url:"example://home".into()}},Op::ExecuteIosHost {operation:Operation::Page {attachment:RequestId::new("page").unwrap(),command:frontcap_core::native::ios_host::PageOperation::Read}}] {
        assert!(background.validate_operation(&denied).is_err());
    }
    for allowed in [op(State::Foreground),Op::Stop,Op::ExecuteIosHost {operation:Operation::ObserveSystemAlerts}] {background.validate_operation(&allowed).unwrap();}
    assert!(background.visibility_transition(&result(transition.clone())).is_err());
    let mut changed=transition.clone();changed.after.started="replacement".into();assert!(ready.visibility_transition(&result(changed)).is_err());
    let mut changed=transition.clone();changed.completion.pid=44;assert!(ready.visibility_transition(&result(changed)).is_err());
    let mut disabled=ready.clone();let Ready::IosHost {app_lifecycle,..}=&mut disabled else {panic!()};*app_lifecycle=false;
    assert!(disabled.validate_operation(&op(State::Background)).is_err());
    let mut foreground=transition;foreground.completion.request.request_id=2;foreground.completion.last_id=2;
    let UiOperation::SetHostAppState {state,..}=&mut foreground.completion.request.operation else {panic!()};*state=State::Foreground;
    let UiOutcome::HostAppStateChanged {change}=&mut foreground.completion.outcome else {panic!()};change.requested=State::Foreground;change.before_state=3;change.app_state=4;
    let returned=background.visibility_transition(&result(foreground)).unwrap().unwrap();returned.validate_operation(&Op::Observe).unwrap();
}
