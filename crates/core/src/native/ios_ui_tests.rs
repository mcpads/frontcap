use super::*;
fn runner() -> UiIdentity {
    UiIdentity {
        simulator: "abcd0000-0000-0000-0000-000000000001".into(),
        generation: "abcd0000-0000-0000-0000-000000000002".into(),
        pid: 123,
    }
}



#[test]
fn generic_tap_outcomes_cannot_change_observation_target_or_app() {
    use super::super::ios_targets::NotAppliedReason;
    let runner = runner();
    let app = ClockIdentity {
        simulator: runner.simulator.clone(),
        generation: "abcd0000-0000-0000-0000-000000000003".into(),
        pid: 124,
    };
    let observation = "abcd0000-0000-0000-0000-000000000009".to_string();
    let reference = format!("{observation}-3");
    let request = UiRequest {
        generation: runner.generation.clone(),
        request_id: 2,
        operation: UiOperation::TapTarget {
            reference: reference.clone(),
            observation: observation.clone(),
        },
    };
    let mut completion = UiCompletion {
        request: request.clone(),
        generation: runner.generation.clone(),
        pid: runner.pid,
        last_id: 2,
        receipt_sha256: "a".repeat(64),
        outcome: UiOutcome::TargetNotApplied {
            app: app.clone(),
            reference: reference.clone(),
            observation: observation.clone(),
            reason: NotAppliedReason::DisabledTarget,
        },
    };
    completion.validate(&runner, &request, Some(&app)).unwrap();
    completion.outcome = UiOutcome::TargetTapped {
        app: app.clone(),
        reference: reference.clone(),
        observation: observation.clone(),
    };
    completion.validate(&runner, &request, Some(&app)).unwrap();
    if let UiOutcome::TargetTapped { reference, .. } = &mut completion.outcome {
        reference.push('0');
    }
    assert!(completion.validate(&runner, &request, Some(&app)).is_err());
    let wrong = ClockIdentity { pid: 125, ..app };
    assert!(
        completion
            .validate(&runner, &request, Some(&wrong))
            .is_err()
    );
}

#[test]
fn generic_catalog_is_bound_to_the_same_capture_and_clock_owner() {
    use super::super::{ios_targets::Catalog, observation::Artifact};
    let runner = runner();
    let app = ClockIdentity {
        simulator: runner.simulator.clone(),
        generation: "abcd0000-0000-0000-0000-000000000003".into(),
        pid: 124,
    };
    let observation = "abcd0000-0000-0000-0000-000000000009".to_string();
    let request = UiRequest {
        generation: runner.generation.clone(),
        request_id: 2,
        operation: UiOperation::ObserveTargets {},
    };
    let mut completion = UiCompletion {
        request: request.clone(),
        generation: runner.generation.clone(),
        pid: runner.pid,
        last_id: 2,
        receipt_sha256: "a".repeat(64),
        outcome: UiOutcome::ObservedTargets {
            app: app.clone(),
            now_ns: 0,
            catalog: Catalog { scrolls: vec![],
                transitioning: false,
                text_input: crate::native::ios_text::TextInput::Unfocused {},
                observation: observation.clone(),
                keyboard: false,
                targets: vec![],
            },
            artifacts: ["screen.png", "hierarchy.txt"]
                .map(|name| Artifact {
                    path: format!("observations/{observation}/{name}"),
                    bytes: 1,
                    sha256: "b".repeat(64),
                })
                .to_vec(),
        },
    };
    completion.validate(&runner, &request, Some(&app)).unwrap();
    if let UiOutcome::ObservedTargets { catalog, .. } = &mut completion.outcome {
        catalog.observation = "abcd0000-0000-0000-0000-000000000008".into();
    }
    assert!(completion.validate(&runner, &request, Some(&app)).is_err());
}

#[test]
fn unavailable_observation_is_a_bound_read_result_without_input_authority() {
    let runner=runner();
    let app=ClockIdentity { simulator:runner.simulator.clone(),generation:"abcd0000-0000-0000-0000-000000000003".into(),pid:124 };
    let request=UiRequest { generation:runner.generation.clone(),request_id:2,operation:UiOperation::ObserveTargets {} };
    let mut result=UiCompletion { request:request.clone(),generation:runner.generation.clone(),pid:runner.pid,last_id:2,
        receipt_sha256:"a".repeat(64),outcome:UiOutcome::ObservationUnavailable { app:app.clone(),now_ns:500,
            reason:UnavailableReason::ScrollInMotion,artifacts:vec![] } };
    result.validate(&runner,&request,Some(&app)).unwrap();
    assert_eq!(serde_json::from_value::<UiCompletion>(serde_json::to_value(&result).unwrap()).unwrap(),result);
    if let UiOutcome::ObservationUnavailable { reason,.. }=&mut result.outcome { *reason=UnavailableReason::CaptureChanged; }
    assert!(result.validate(&runner,&request,Some(&app)).is_err());
    if let UiOutcome::ObservationUnavailable { artifacts,.. }=&mut result.outcome {
        *artifacts=["screen.png","hierarchy.txt"].into_iter().map(|name| super::super::observation::Artifact {
            path:format!("observations/abcd0000-0000-0000-0000-000000000009/{name}"),bytes:1,sha256:"b".repeat(64),
        }).collect();
    }
    result.validate(&runner,&request,Some(&app)).unwrap();
    let other=ClockIdentity { pid:125,..app };assert!(result.validate(&runner,&request,Some(&other)).is_err());
    let tap=UiRequest { operation:UiOperation::TapTarget { reference:"invalid".into(), observation:"invalid".into() },..request };
    assert!(result.validate(&runner,&tap,Some(&other)).is_err());
}

#[test]
fn launch_environment_preserves_old_wire_and_rejects_control_overrides() {
    let mut request = UiRequest {
        generation: runner().generation, request_id: 1,
        operation: UiOperation::Launch { environment: Default::default() },
    };
    let wire = serde_json::to_value(&request).unwrap();
    assert_eq!(wire["arguments"], serde_json::json!({}));
    assert_eq!(serde_json::from_value::<UiRequest>(wire).unwrap(), request);
    for (key, value, accepted) in [
        ("APP_SERVICE_URL", "http://127.0.0.1:123/g/requests/", true),
        ("LABEL", "한글 'quoted' $value", true), ("LABEL", "", true),
        ("FC_MANUAL", "0", false), ("DYLD_INSERT_LIBRARIES", "other", false),
        ("SIMCTL_CHILD_VALUE", "x", false), ("bad-key", "x", false),
        ("LABEL", "line\nbreak", false), ("LABEL", "\u{0085}", false),
    ] {
        request.operation = UiOperation::Launch { environment: [(key.into(), value.into())].into() };
        assert_eq!(request.validate(&runner()).is_ok(), accepted, "{key}");
    }
    let environment = (0..17).map(|i| (format!("KEY{i}"), String::new())).collect();
    request.operation = UiOperation::Launch { environment };
    assert!(request.validate(&runner()).is_err());
    request.operation = UiOperation::Launch { environment: [("LABEL".into(), "가".repeat(683))].into() };
    assert!(request.validate(&runner()).is_err());
}

#[test]
fn restart_requires_the_original_app_retirement_and_a_distinct_zero_clock_owner() {
    let runner = runner();
    let previous = ClockIdentity { simulator: runner.simulator.clone(),
        generation: "abcd0000-0000-0000-0000-000000000003".into(), pid: 124 };
    let app = ClockIdentity { generation: "abcd0000-0000-0000-0000-000000000004".into(), pid: 125, ..previous.clone() };
    let request = UiRequest { generation: runner.generation.clone(), request_id: 8,
        operation: UiOperation::Restart { app: previous.clone(), url: None } };
    let receipt = UiCompletion { request: request.clone(), generation: runner.generation.clone(), pid: runner.pid, last_id: 8,
        outcome: UiOutcome::Restarted { previous: previous.clone(), app: app.clone(), process_absent: true, port_closed: true, now_ns: 0 },
        receipt_sha256: "a".repeat(64) };
    receipt.validate(&runner, &request, Some(&previous)).unwrap();
    assert!(receipt.validate(&runner, &request, None).is_err());
    assert!(receipt.validate(&runner, &request, Some(&app)).is_err());
    for outcome in [
        UiOutcome::Restarted { previous: previous.clone(), app: previous.clone(), process_absent: true, port_closed: true, now_ns: 0 },
        UiOutcome::Restarted { previous: app.clone(), app: app.clone(), process_absent: true, port_closed: true, now_ns: 0 },
        UiOutcome::Restarted { previous: previous.clone(), app: app.clone(), process_absent: false, port_closed: true, now_ns: 0 },
        UiOutcome::Restarted { previous: previous.clone(), app: app.clone(), process_absent: true, port_closed: false, now_ns: 0 },
        UiOutcome::Restarted { previous: previous.clone(), app: app.clone(), process_absent: true, port_closed: true, now_ns: 1 },
    ] {
        assert!(UiCompletion { outcome, ..receipt.clone() }.validate(&runner, &request, Some(&previous)).is_err());
    }
}

#[test]
fn host_state_completion_requires_the_requested_transition_and_same_process_identity() {
    use crate::{native::{app_state::State, ios_lifecycle::{StateChange,Transition}, ios_request::Owner}, process::ProcessIdentity};
    let owner = Owner { runner: runner(), app: ClockIdentity { simulator: runner().simulator,
        generation: "abcd0000-0000-0000-0000-000000000003".into(), pid: 124 },
        controller_id: "abcd0000-0000-0000-0000-000000000004".into(), source_sha256: "a".repeat(64) };
    let request = UiRequest { generation: owner.runner.generation.clone(), request_id: 2,
        operation: UiOperation::SetAppState { app: owner.app.clone(), state: State::Background } };
    let completion = UiCompletion { generation: request.generation.clone(), last_id: 2, pid: owner.runner.pid, request,
        receipt_sha256: "a".repeat(64), outcome: UiOutcome::AppStateChanged { app: owner.app.clone(), change: StateChange {
            requested: State::Background, before_state: 4, app_state: 2, now_ns: None } } };
    let process = ProcessIdentity { pid: owner.app.pid, started: "original process".into() };
    let transition = Transition { owner, process_before: process.clone(), process_after: process, completion };
    transition.validate().unwrap();
    let mut changed = transition.clone(); changed.process_after.started = "reused PID".into();
    assert!(changed.validate().is_err());
    for (requested,before_state,app_state,now_ns) in [
        (State::Background,4,4,None), (State::Background,4,2,Some(0)),
        (State::Foreground,2,4,None), (State::Foreground,2,4,Some(u64::MAX)),
    ] { assert!(StateChange {requested,before_state,app_state,now_ns}.validate().is_err()); }
    let mut changed = transition.clone();
    if let UiOutcome::AppStateChanged { change,.. } = &mut changed.completion.outcome {
        *change = StateChange {requested:State::Foreground,before_state:2,app_state:4,now_ns:Some(0)};
    }
    assert!(changed.validate().is_err());
}

#[test]
fn independent_host_visibility_binds_running_app_without_a_clock() {
    use crate::native::app_state::State;
    let runner=runner();
    let request=UiRequest {generation:runner.generation.clone(),request_id:1,
        operation:UiOperation::SetHostAppState {bundle:"dev.example.flutter".into(),pid:456,state:State::Background}};
    let change=HostAppStateChange {bundle:"dev.example.flutter".into(),pid:456,requested:State::Background,before_state:4,app_state:3};
    let receipt=UiCompletion {request:request.clone(),generation:runner.generation.clone(),pid:runner.pid,last_id:1,
        outcome:UiOutcome::HostAppStateChanged {change:change.clone()},receipt_sha256:"a".repeat(64)};
    receipt.validate_system(&runner,&request).unwrap();
    for changed in [HostAppStateChange {pid:457,..change.clone()},HostAppStateChange {app_state:4,..change.clone()},
        HostAppStateChange {requested:State::Foreground,before_state:3,app_state:4,..change.clone()}] {
        assert!(UiCompletion {outcome:UiOutcome::HostAppStateChanged {change:changed},..receipt.clone()}.validate_system(&runner,&request).is_err());
    }
    let request=UiRequest {operation:UiOperation::SetHostAppState {bundle:"dev.example.flutter".into(),pid:runner.pid,state:State::Background},..request};
    assert!(request.validate(&runner).is_err());
}
