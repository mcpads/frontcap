use super::*;
use frontcap_core::ids::{RequestId, WatchName};
use frontcap_core::predicate::Locator;
use frontcap_recorder_protocol::Reply;

fn click() -> Op {
    Op::Act { then: None, action: frontcap_core::action::Action::Click { target: Locator::Css { selector: "#ok".into() } }, context: None }
}

fn inflight(op: Op, goal_us: Option<VirtualTime>) -> Inflight {
    Inflight { event_arm: None, request_id: RequestId::new("x").unwrap(), op, goal_us, goal_step: None, plan: None, start: None, result: None, quiet_since: None, acted: None }
}

#[test]
fn recorder_state_uses_the_applied_answer() {
    let op = Op::WatchRemove { name: WatchName::new("w").unwrap() };
    let answer = Answer::Ok(Reply::Released);
    assert_eq!(decide(&op, None, Some(&answer), None), Decision::Complete(Box::new(answer)));
    assert_eq!(decide(&op, None, None, None), Decision::NotApplied);
}

#[test]
fn inputs_resume_after_the_highest_received_substep() {
    let plan = frontcap_chromium::input::InputPlan { pointer_context: None,
        start_us: Default::default(),
        document: 0,
        action: frontcap_core::action::Action::Click { target: Locator::Css { selector: "#ok".into() } },
        node: Some(1),
        steps: vec![],
    };
    let i = Inflight { plan: Some(plan), ..inflight(click(), None) };
    assert_eq!(decide(&click(), Some(&i), None, Some(&[])), Decision::ResumeInput { from_substep: 0 });
    assert_eq!(decide(&click(), Some(&i), None, Some(&[0])), Decision::ResumeInput { from_substep: 1 });
    assert_eq!(decide(&click(), Some(&i), None, Some(&[0, 1])), Decision::ResumeInput { from_substep: 2 });
    assert!(matches!(decide(&click(), Some(&i), None, None), Decision::Fault { .. }));
    assert!(matches!(decide(&click(), Some(&inflight(click(), None)), None, Some(&[])), Decision::Fault { .. }), "no plan to resume");
}

#[test]
fn an_act_whose_action_completed_resumes_its_settle_and_a_navigation_otherwise_faults() {
    let reload = Op::Act { action: frontcap_core::action::Action::Reload {}, then: None, context: None };
    assert_eq!(decide(&reload, Some(&inflight(reload.clone(), None)), None, None), Decision::ResumeNavigation);
    let b = frontcap_recorder_protocol::Boundary { t_us: VirtualTime(1), seq: 1, dom_mutations: 0 };
    let done = Inflight { acted: Some(b), ..inflight(reload.clone(), None) };
    assert_eq!(decide(&reload, Some(&done), None, None), Decision::ResumeSettle);
    let close = Op::Act { action: frontcap_core::action::Action::Close {}, then: None, context: Some("popup-1".into()) };
    assert_eq!(decide(&close, Some(&inflight(close.clone(), None)), None, None), Decision::ResumeClose);
    assert_eq!(decide(&close, Some(&Inflight { acted: Some(b), ..inflight(close.clone(), None) }), None, None), Decision::ResumeSettle);
}

#[test]
fn interrupted_evaluations_are_not_reissued_in_either_world() {
    for op in [
        Op::Evaluate { expression: "sideEffect()".into() },
        Op::EvaluateInFrame { expression: "sideEffect()".into(), frame: frontcap_core::predicate::Locator::Css { selector: "#child".into() } },
    ] {
        assert!(matches!(decide(&op, Some(&inflight(op.clone(), None)), None, None), Decision::Fault { .. }));
        let saved = Answer::Err(frontcap_recorder_protocol::ErrorReply::new(
            frontcap_recorder_protocol::ErrorCode::ScriptFailed, "recorded exception",
        ));
        let completed = Inflight { result: Some(saved.clone()), ..inflight(op.clone(), None) };
        assert_eq!(decide(&op, Some(&completed), None, None), Decision::Complete(Box::new(saved)));
    }
}

#[test]
fn clocks_continue_to_the_stored_goal() {
    let op = Op::Advance { by_us: 5 };
    let i = inflight(op.clone(), Some(VirtualTime(9)));
    assert_eq!(decide(&op, Some(&i), None, None), Decision::ContinueClock { goal: VirtualTime(9) });
    assert!(matches!(decide(&op, None, None, None), Decision::Fault { .. }), "no goal to continue to");
}

#[test]
fn an_interrupted_answer_to_a_held_request_is_resolved_from_the_adopted_generation() {
    let op = Op::FailHeld { ordinal: "req-1".into(), reason: frontcap_core::rules::FailReason::Aborted };
    assert_eq!(decide(&op, None, None, None), Decision::ResolveHeld { ordinal: "req-1".into() });
    let applied = Answer::Ok(Reply::Released);
    assert_eq!(decide(&op, None, Some(&applied), None), Decision::Complete(Box::new(applied.clone())), "a saved answer is returned");
}
