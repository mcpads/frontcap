use super::*;
#[test]
fn cold_flutter_phase_keeps_system_authority_without_app_authority() {
    use frontcap_core::native::{flutter::{Owner, Platform}, flutter_entry::{Pending, Progress, Waiting},
        external_entry::{Accepted, Target}, ios_ui::UiIdentity};
    let owner = Owner { platform: Platform::Ios, device: "ABCD0000-0000-0000-0000-000000000001".into(),
        generation: "a".repeat(48), source_sha256: "b".repeat(64) };
    let target = Target::Ios { simulator: owner.device.to_ascii_lowercase(), bundle: "org.example.client".into() };
    let runner = UiIdentity { simulator: owner.device.clone(), generation: "abcd0000-0000-0000-0000-000000000002".into(), pid: 123 };
    let ready = Ready::Flutter { owner: owner.clone(), restart: true, app_lifecycle: true, external_url_target: Some(target.clone()), system_ui: Some(runner) };
    let begin = Op::BeginFlutterEntry { generation: owner.generation.clone(), url: "client-test://item/1".into() };
    ready.validate_operation(&begin).unwrap();
    let entry = Pending { request_id: RequestId::new("entry").unwrap(), previous: owner.clone(),
        acceptance: Accepted::new(frontcap_core::native::flutter_lifecycle::ios_entry(&owner, Some(&target), "client-test://item/1").unwrap()) };
    let pending = ready.entry_transition(&Outcome::FlutterEntryStarted { entry: entry.clone() }).unwrap().unwrap();
    assert!(matches!(&pending, Ready::FlutterEntering { app_lifecycle: true, .. }));
    let poll = Op::PollFlutterEntry { entry_request_id: entry.request_id.clone(), generation: owner.generation.clone() };
    assert!(ready.validate_operation(&poll).is_err());
    pending.validate_operation(&poll).unwrap();
    pending.validate_operation(&Op::ExecuteIos { operation: frontcap_core::native::ios_request::Operation::ObserveSystemAlerts }).unwrap();
    pending.validate_operation(&Op::Stop).unwrap();
    for op in [Op::Observe, begin, Op::RestartFlutter { generation: owner.generation.clone(), url: None },
        Op::SetAppState { generation: owner.generation.clone(), state: frontcap_core::native::app_state::State::Foreground },
        Op::DispatchUrl { request: entry.acceptance.request.clone() },
        Op::ExecuteFlutter { generation: owner.generation.clone(), operation: frontcap_core::native::flutter::Operation::Read {} },
        Op::PollFlutterEntry { entry_request_id: RequestId::new("other").unwrap(), generation: owner.generation.clone() },
        Op::PollFlutterEntry { entry_request_id: entry.request_id.clone(), generation: "c".repeat(48) }] {
        assert!(pending.validate_operation(&op).is_err(), "{op:?}");
    }
    let waiting = Outcome::FlutterEntryPolled { progress: Progress::Waiting { entry, readiness: Waiting::Process {} } };
    assert_eq!(pending.entry_transition(&waiting).unwrap(), Some(pending.clone()));
    assert!(ready.entry_transition(&waiting).is_err());
}
#[test]
fn flutter_generation_and_retirement_are_checked_before_admission() {
    use frontcap_core::native::flutter::{Operation, Owner, Platform};
    let ready = Ready::Flutter {
        restart: false,
        app_lifecycle: false,
        system_ui: None,
        external_url_target: None,
        owner: Owner {
            platform: Platform::Ios,
            device: "simulator".into(),
            generation: "a".repeat(48),
            source_sha256: "b".repeat(64),
        },
    };
    assert!(
        ready
            .validate_operation(&Op::ExecuteFlutter {
                generation: "a".repeat(48),
                operation: Operation::Observe {}
            })
            .is_ok()
    );
    assert!(
        ready
            .validate_operation(&Op::ExecuteFlutter {
                generation: "c".repeat(48),
                operation: Operation::Observe {}
            })
            .is_err()
    );
    assert!(
        ready
            .validate_operation(&Op::ExecuteFlutter {
                generation: "a".repeat(48),
                operation: Operation::Stop {}
            })
            .is_err()
    );
    assert!(ready.validate_operation(&Op::Stop).is_ok());
    assert!(ready.validate_operation(&Op::RestartFlutter { generation: "a".repeat(48), url: None }).is_err());
    let mut standalone = ready;
    if let Ready::Flutter { restart, .. } = &mut standalone { *restart = true; }
    assert!(standalone.validate_operation(&Op::RestartFlutter { generation: "a".repeat(48), url: None }).is_ok());
    assert!(standalone.validate_operation(&Op::RestartFlutter { generation: "b".repeat(48), url: None }).is_err());
    let cold = Op::RestartFlutter { generation: "a".repeat(48), url: Some("client-test://item/1".into()) };
    assert!(standalone.validate_operation(&cold).is_err()); // iOS confirmation cannot be hidden in restart.
    if let Ready::Flutter { owner, external_url_target, .. } = &mut standalone {
        owner.platform = frontcap_core::native::flutter::Platform::Android;
        owner.device = "emulator-5580".into();
        *external_url_target = Some(frontcap_core::native::external_entry::Target::Android {
            serial: owner.device.clone(), package: "org.example.client".into(),
        });
    }
    assert!(standalone.validate_operation(&cold).is_ok());
    assert!(standalone.validate_operation(&Op::RestartFlutter { generation: "a".repeat(48), url: Some("relative".into()) }).is_err());
    assert!(standalone.validate_operation(&Op::RestartFlutter { generation: "a".repeat(48), url: None }).is_ok());
    assert!(standalone.validate_operation(&Op::RestartFlutter { generation: "b".repeat(48), url: None }).is_err());
    if let Ready::Flutter { restart, .. } = &mut standalone { *restart = false; }
    assert!(standalone.validate_operation(&cold).is_err());
    assert!(standalone.validate_operation(&Op::RestartFlutter { generation: "a".repeat(48), url: None }).is_err());
}
