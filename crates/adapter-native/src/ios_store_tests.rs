use super::*;
use frontcap_core::native::{
    ios::{ClockCompletion, ClockIdentity, ClockStatus},
    ios_ui::UiIdentity,
};
fn owner() -> Owner {
    Owner {
        runner: UiIdentity {
            simulator: "abcd0000-0000-0000-0000-000000000001".into(),
            generation: "abcd0000-0000-0000-0000-000000000002".into(),
            pid: 123,
        },
        app: ClockIdentity {
            simulator: "abcd0000-0000-0000-0000-000000000001".into(),
            generation: "abcd0000-0000-0000-0000-000000000003".into(),
            pid: 124,
        },
        controller_id: "abcd0000-0000-0000-0000-000000000004".into(),
        source_sha256: "a".repeat(64),
    }
}
fn id(name: &str) -> RequestId {
    RequestId::new(name).unwrap()
}

fn press_completion(request: &Request, delivered: bool) -> Completion {
    use frontcap_core::native::{ios_request::PressClock, ios_ui::{UiCompletion,UiOperation,UiOutcome},
        ios_targets::NotAppliedReason, ios_press::Evidence};
    let Request::Press { ui, clock }=request else { panic!() };
    let UiOperation::PressTarget { reference,observation,.. }=&ui.operation else { panic!() };
    let outcome=if delivered {
        let mut raw:serde_json::Value=serde_json::from_str(include_str!("../../core/tests/fixtures/uikit-press.json")).unwrap();
        raw.as_object_mut().unwrap().remove("app");
        let mut evidence:Evidence=serde_json::from_value(raw).unwrap();
        evidence.reference=reference.clone();evidence.observation=observation.clone();
        evidence.clock_request=clock.clone();
        for boundary in [&mut evidence.before,&mut evidence.began,&mut evidence.ended] {
            boundary.generation=owner().app.generation;
        }
        evidence.clock_reply.generation=owner().app.generation;
        evidence.clock_reply.pid=owner().app.pid;
        evidence.clock_reply.request=clock.clone();evidence.clock_reply.last_id=clock.request_id;
        UiOutcome::TargetPressed { app:owner().app,evidence }
    } else {
        UiOutcome::TargetPressNotApplied { app:owner().app,reference:reference.clone(),observation:observation.clone(),
            clock_request:clock.clone(),reason:NotAppliedReason::StaleObservation }
    };
    let result=UiCompletion { request:ui.clone(),generation:ui.generation.clone(),pid:owner().runner.pid,
        last_id:ui.request_id,receipt_sha256:"a".repeat(64),outcome };
    let clock_result=if delivered {
        PressClock::Delivered { completion:ClockCompletion { request:clock.clone(),generation:clock.generation.clone(),
            pid:owner().app.pid,last_id:clock.request_id,status:ClockStatus::Ok,before_ns:500_000_000,
            now_ns:570_000_000,applied:true,advance_result:Some(0),drain_result:Some(0),receipt_sha256:"b".repeat(64) } }
    } else { PressClock::Released { request:clock.clone(),ui_receipt_sha256:result.receipt_sha256.clone() } };
    Completion::Press { ui:Box::new(result),clock:clock_result }
}

#[test]
fn press_consumes_both_sequences_only_on_delivery_and_reopen_never_reauthorizes_input() {
    let root=tempfile::tempdir().unwrap();let path=root.path().join("store");
    let owner=owner();let mut store=RequestStore::create(&path,owner.clone()).unwrap();
    let observation="abcd0000-0000-0000-0000-000000000009".to_owned();
    let op=Operation::PressTarget { reference:format!("{observation}-0"),observation,delta_ns:70_000_000 };
    for (name,delivered,ui_number) in [("rejected",false,2),("pressed",true,3)] {
        let key=id(name);
        let Recovery::Prepared(request)=store.prepare(key.clone(),op.clone()).unwrap() else { panic!() };
        let Request::Press { ui,clock }=&request else { panic!() };
        assert_eq!(ui.request_id,ui_number);assert_eq!(clock.request_id,1);
        assert!(store.prepare(id("other"),Operation::ReadClock).is_err());
        store.dispatch(&key).unwrap();drop(store);
        store=RequestStore::open(&path,&owner).unwrap();
        assert_eq!(store.recovery(&key).unwrap(),Recovery::Reconcile(request.clone()));
        assert!(store.dispatch(&key).is_err());
        let receipt=press_completion(&request,delivered);
        let mut foreign=receipt.clone();
        if let Completion::Press { clock,.. }=&mut foreign {
            match clock {
                frontcap_core::native::ios_request::PressClock::Delivered { completion } => completion.request.request_id+=1,
                frontcap_core::native::ios_request::PressClock::Released { ui_receipt_sha256,.. } => *ui_receipt_sha256="c".repeat(64),
            }
        }
        assert!(store.complete(&key,foreign).is_err());
        store.complete(&key,receipt.clone()).unwrap();drop(store);
        store=RequestStore::open(&path,&owner).unwrap();
        assert_eq!(store.prepare(key,op.clone()).unwrap(),Recovery::Completed(receipt));
    }
    let Recovery::Prepared(Request::Clock(read))=store.prepare(id("read"),Operation::ReadClock).unwrap() else { panic!() };
    assert_eq!(read.request_id,2);
}
fn clock_completion(request: &Request, failed: bool) -> Completion {
    let Request::Clock(request) = request else {
        panic!("clock required")
    };
    let advance = request.operation == frontcap_core::native::request::ClockOperation::Advance;
    Completion::Clock(ClockCompletion {
        request: request.clone(),
        generation: owner().app.generation,
        pid: 124,
        last_id: request.request_id,
        status: if failed {
            ClockStatus::Failed
        } else {
            ClockStatus::Ok
        },
        before_ns: 0,
        now_ns: if advance && !failed {
            request.delta_ns
        } else {
            0
        },
        applied: advance && !failed,
        advance_result: advance.then_some(if failed { 45 } else { 0 }),
        drain_result: (advance && !failed).then_some(0),
        receipt_sha256: "b".repeat(64),
    })
}
#[test]
fn consumed_clock_failure_keeps_independent_ui_sequence_and_exact_caller_binding() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let owner = owner();
    let mut store = RequestStore::create(&dir, owner.clone()).unwrap();
    assert!(matches!(
        RequestStore::open(&dir, &owner),
        Err(StoreError::Busy)
    ));
    let failed = id("failed-step");
    let operation = Operation::AdvanceClock { delta_ns: 1 };
    let Recovery::Prepared(request) = store.prepare(failed.clone(), operation.clone()).unwrap()
    else {
        panic!()
    };
    assert_eq!(request.number(), 1);
    assert!(matches!(
        store.complete(&failed, clock_completion(&request, true)),
        Err(StoreError::NotDispatched)
    ));
    store.dispatch(&failed).unwrap();
    let receipt = clock_completion(&request, true);
    store.complete(&failed, receipt.clone()).unwrap();
    assert_eq!(
        store.prepare(failed.clone(), operation.clone()).unwrap(),
        Recovery::Completed(receipt.clone())
    );
    assert!(matches!(
        store.prepare(failed.clone(), Operation::ReadClock),
        Err(StoreError::Conflict)
    ));
    let before = std::fs::read(dir.join("requests.json")).unwrap();
    assert_eq!(
        RequestStore::read_completion(&dir, &owner, &failed, &operation).unwrap(),
        Some(receipt)
    );
    assert_eq!(std::fs::read(dir.join("requests.json")).unwrap(), before);
    let next = id("read");
    let Recovery::Prepared(request) = store.prepare(next.clone(), Operation::ReadClock).unwrap()
    else {
        panic!()
    };
    assert_eq!(request.number(), 2);
    store.dispatch(&next).unwrap();
    store
        .complete(&next, clock_completion(&request, false))
        .unwrap();
    let observation = id("observe");
    let Recovery::Prepared(request) = store
        .prepare(observation.clone(), Operation::ObserveTargets)
        .unwrap()
    else {
        panic!()
    };
    assert!(!request.is_clock());
    assert_eq!(request.number(), 2);
    store.dispatch(&observation).unwrap();
    drop(store);
    let mut store = RequestStore::open(&dir, &owner).unwrap();
    assert_eq!(
        store.recovery(&observation).unwrap(),
        Recovery::Reconcile(request)
    );
    assert!(matches!(
        store.dispatch(&observation),
        Err(StoreError::AlreadyDispatched)
    ));
    assert!(matches!(
        store.prepare(id("another"), Operation::ReadClock),
        Err(StoreError::Pending)
    ));
    assert!(
        RequestStore::read_completion(&dir, &owner, &observation, &Operation::ObserveTargets)
            .unwrap()
            .is_none()
    );
}
#[test]
fn identity_source_controller_and_terminal_app_are_fenced() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let original = owner();
    let mut store = RequestStore::create(&dir, original.clone()).unwrap();
    let finish = id("finish");
    let Recovery::Prepared(Request::Ui(request)) =
        store.prepare(finish.clone(), Operation::Finish).unwrap()
    else {
        panic!()
    };
    store.dispatch(&finish).unwrap();
    let receipt = Completion::Ui(Box::new(frontcap_core::native::ios_ui::UiCompletion {
        request: request.clone(),
        generation: original.runner.generation.clone(),
        pid: 123,
        last_id: 2,
        outcome: frontcap_core::native::ios_ui::UiOutcome::Terminated {
            app: original.app.clone(),
            process_absent: true,
            port_closed: true,
        },
        receipt_sha256: "b".repeat(64),
    }));
    store.complete(&finish, receipt).unwrap();
    assert!(store.prepare(id("late"), Operation::ReadClock).is_err());
    drop(store);
    for change in 0..4 {
        let mut changed = original.clone();
        match change {
            0 => changed.app.pid += 1,
            1 => changed.runner.pid += 2,
            2 => changed.source_sha256 = "c".repeat(64),
            _ => changed.controller_id = "abcd0000-0000-0000-0000-000000000005".into(),
        }
        assert!(matches!(
            RequestStore::open(&dir, &changed),
            Err(StoreError::IdentityChanged)
        ));
        assert!(matches!(
            RequestStore::read_completion(&dir, &changed, &finish, &Operation::Finish),
            Err(StoreError::IdentityChanged)
        ));
    }
    assert!(RequestStore::open(&dir, &original).is_ok());
}
#[test]
fn invalid_intent_and_uncertain_publication_never_authorize_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let mut store = RequestStore::create(&dir, owner()).unwrap();
    assert!(
        store
            .prepare(
                id("bad"),
                Operation::TapTarget {
                    reference: "invalid".into(),
                    observation: "../capture".into()
                }
            )
            .is_err()
    );
    std::fs::remove_file(dir.join("requests.json")).unwrap();
    std::fs::create_dir(dir.join("requests.json")).unwrap();
    assert!(
        store
            .prepare(id("write-failure"), Operation::ReadClock)
            .is_err()
    );
    assert!(matches!(
        store.prepare(id("retry"), Operation::ReadClock),
        Err(StoreError::StorageUncertain)
    ));
    assert!(matches!(
        store.dispatch(&id("write-failure")),
        Err(StoreError::StorageUncertain)
    ));
}
#[test]
fn killed_store_owner_does_not_redispatch_or_lose_completed_result() {
    use std::{
        process::{Command, Stdio},
        time::{Duration, Instant},
    };
    for phase in ["prepared", "dispatched", "completed"] {
        let root = tempfile::tempdir().unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "ios_store::tests::store_owner_entry",
                "--nocapture",
            ])
            .env("FRONTCAP_IOS_STORE_TEST", root.path())
            .env("FRONTCAP_IOS_STORE_PHASE", phase)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !root.path().join("ready").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let ready = root.path().join("ready").exists();
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(ready);
        let mut store = RequestStore::open(&root.path().join("store"), &owner()).unwrap();
        let result = store.recovery(&id("request")).unwrap();
        match phase {
            "prepared" => assert!(matches!(result, Recovery::Prepared(_))),
            "dispatched" => assert!(matches!(result, Recovery::Reconcile(_))),
            _ => assert!(matches!(result, Recovery::Completed(_))),
        }
        if phase != "prepared" {
            assert!(matches!(
                store.dispatch(&id("request")),
                Err(StoreError::AlreadyDispatched)
            ));
        }
    }
}
#[test]
fn store_owner_entry() {
    let Some(root) = std::env::var_os("FRONTCAP_IOS_STORE_TEST") else {
        return;
    };
    let root = PathBuf::from(root);
    let phase = std::env::var("FRONTCAP_IOS_STORE_PHASE").unwrap();
    let mut store = RequestStore::create(&root.join("store"), owner()).unwrap();
    let Recovery::Prepared(request) = store.prepare(id("request"), Operation::ReadClock).unwrap()
    else {
        panic!()
    };
    if phase != "prepared" {
        store.dispatch(&id("request")).unwrap();
    }
    if phase == "completed" {
        store
            .complete(&id("request"), clock_completion(&request, false))
            .unwrap();
    }
    std::fs::write(root.join("ready"), b"ready").unwrap();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[test]
fn missing_state_and_sequence_gaps_never_reset_a_saved_session() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let mut store = RequestStore::create(&dir, owner()).unwrap();
    let key = id("read");
    let Recovery::Prepared(request) = store.prepare(key.clone(), Operation::ReadClock).unwrap()
    else {
        panic!()
    };
    store.dispatch(&key).unwrap();
    store
        .complete(&key, clock_completion(&request, false))
        .unwrap();
    drop(store);
    let mut state = RequestStore::read(&dir, &owner()).unwrap();
    state.next_clock += 1;
    save_atomic(&dir.join("requests.json"), &state).unwrap();
    assert!(RequestStore::open(&dir, &owner()).is_err());
    assert!(RequestStore::read_completion(&dir, &owner(), &key, &Operation::ReadClock).is_err());
    std::fs::remove_file(dir.join("requests.json")).unwrap();
    assert!(RequestStore::open(&dir, &owner()).is_err());
    assert!(RequestStore::create(&dir, owner()).is_err());
}

#[test]
fn generic_no_effect_input_is_durable_and_uncertain_input_cannot_redispatch() {
    use frontcap_core::native::{
        ios_targets::NotAppliedReason,
        ios_ui::{UiCompletion, UiOutcome},
    };
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let owner = owner();
    let mut store = RequestStore::create(&dir, owner.clone()).unwrap();
    let observation = "abcd0000-0000-0000-0000-000000000009".to_string();
    let reference = format!("{observation}-0");
    let operation = Operation::TapTarget {
        reference: reference.clone(),
        observation: observation.clone(),
    };
    let caller = id("tap");
    let Recovery::Prepared(Request::Ui(request)) =
        store.prepare(caller.clone(), operation.clone()).unwrap()
    else {
        panic!()
    };
    store.dispatch(&caller).unwrap();
    drop(store);
    let mut store = RequestStore::open(&dir, &owner).unwrap();
    assert_eq!(
        store.recovery(&caller).unwrap(),
        Recovery::Reconcile(Request::Ui(request.clone()))
    );
    assert!(matches!(
        store.dispatch(&caller),
        Err(StoreError::AlreadyDispatched)
    ));
    assert!(matches!(
        store.prepare(id("fresh-id"), operation.clone()),
        Err(StoreError::Pending)
    ));
    let receipt = Completion::Ui(Box::new(UiCompletion {
        request: request.clone(),
        generation: owner.runner.generation.clone(),
        pid: owner.runner.pid,
        last_id: request.request_id,
        receipt_sha256: "b".repeat(64),
        outcome: UiOutcome::TargetNotApplied {
            app: owner.app.clone(),
            reference,
            observation,
            reason: NotAppliedReason::ObservationChanged,
        },
    }));
    store.complete(&caller, receipt.clone()).unwrap();
    drop(store);
    let mut store = RequestStore::open(&dir, &owner).unwrap();
    assert_eq!(
        store.prepare(caller.clone(), operation.clone()).unwrap(),
        Recovery::Completed(receipt.clone())
    );
    assert_eq!(
        RequestStore::read_completion(&dir, &owner, &caller, &operation).unwrap(),
        Some(receipt)
    );
    assert!(matches!(
        store.prepare(caller, Operation::ObserveTargets),
        Err(StoreError::Conflict)
    ));
    let Recovery::Prepared(next) = store
        .prepare(id("observe"), Operation::ObserveTargets)
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(next.number(), request.request_id + 1);
}

#[test]
fn read_only_and_transitioning_targets_reject_without_binding_the_caller_id() {
    for transitioning in [false, true] {
        rejected_target_does_not_bind(transitioning);
    }
}
fn rejected_target_does_not_bind(transitioning: bool) {
    use frontcap_core::native::{ios_targets::{Catalog, Role, SelectorKey, Target},
        ios_ui::{UiCompletion, UiOutcome}, observation::Artifact};
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let owner = owner();
    let mut store = RequestStore::create(&dir, owner.clone()).unwrap();
    let observed_id = id("observation");
    let Recovery::Prepared(Request::Ui(request)) = store.prepare(observed_id.clone(), Operation::ObserveTargets).unwrap() else { panic!() };
    store.dispatch(&observed_id).unwrap();
    let observation = "abcd0000-0000-0000-0000-000000000009".to_string();
    let reference = format!("{observation}-0");
    let catalog = Catalog { scrolls: vec![], transitioning, text_input: frontcap_core::native::ios_text::TextInput::Unfocused {}, observation: observation.clone(), keyboard: false, targets: vec![Target {
        role: if transitioning { Role::Button } else { Role::StaticText }, element_type: if transitioning { 9 } else { 48 }, identifier: "".into(), label: "Status".into(),
        value: None, selector_key: SelectorKey::Label, selector_value: "Status".into(),
        unique: true, enabled: true, selected: false, hittable: transitioning, intersects_screen: true,
        frame_mpt: [0,0,1000,1000], reference: reference.clone(),
    }] };
    let artifacts = ["screen.png", "hierarchy.txt"].into_iter().map(|name| Artifact {
        path: format!("observations/{observation}/{name}"), bytes: 1, sha256: "a".repeat(64),
    }).collect();
    store.complete(&observed_id, Completion::Ui(Box::new(UiCompletion {
        generation: owner.runner.generation.clone(), pid: owner.runner.pid,
        last_id: request.request_id, request, receipt_sha256: "b".repeat(64),
        outcome: UiOutcome::ObservedTargets { app: owner.app.clone(), now_ns: 0, catalog, artifacts },
    }))).unwrap();
    let caller = id("read-only-tap");
    let before = std::fs::read(dir.join("requests.json")).unwrap();
    assert!(RequestStore::validate_ui_input(&dir, &owner, &caller,
        &Operation::TapTarget { reference, observation }).is_err());
    assert_eq!(std::fs::read(dir.join("requests.json")).unwrap(), before);
    assert!(matches!(store.prepare(caller, Operation::ReadClock).unwrap(), Recovery::Prepared(_)));
}

#[test]
fn text_completion_must_match_the_retained_observation_basis() {
    use frontcap_core::native::{ios_edit::{Delivery, Receipt, SourceResult}, ios_text::TextInput,
        ios_targets::{Catalog, Role, SelectorKey, Target}, ios_ui::{UiCompletion, UiOutcome}, observation::Artifact};
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let owner = owner();
    let mut store = RequestStore::create(&dir, owner.clone()).unwrap();
    let mut source: Receipt = serde_json::from_str(include_str!("../../core/tests/fixtures/uikit-edit.json")).unwrap();
    source.pid = owner.app.pid;
    let observation = "abcd0000-0000-0000-0000-000000000009".to_string();
    let reference = format!("{observation}-0");
    let catalog = Catalog { scrolls: vec![], observation: observation.clone(), keyboard: false, transitioning: false,
        text_input: source.request.arguments.basis.clone(), targets: vec![Target {
            role: Role::TextField, element_type: 49, identifier: "".into(), label: "Field".into(),
            value: None, selector_key: SelectorKey::Label, selector_value: "Field".into(), unique: true,
            enabled: true, selected: false, hittable: true, intersects_screen: true,
            frame_mpt: [0,0,1000,1000], reference: reference.clone(),
        }] };
    let key = id("observe");
    let Recovery::Prepared(Request::Ui(request)) = store.prepare(key.clone(), Operation::ObserveTargets).unwrap() else { panic!() };
    store.dispatch(&key).unwrap();
    let artifacts = ["screen.png", "hierarchy.txt"].into_iter().map(|name| Artifact {
        path: format!("observations/{observation}/{name}"), bytes: 1, sha256: "a".repeat(64),
    }).collect();
    store.complete(&key, Completion::Ui(Box::new(UiCompletion {
        generation: request.generation.clone(), pid: owner.runner.pid, last_id: request.request_id,
        request, receipt_sha256: "b".repeat(64),
        outcome: UiOutcome::ObservedTargets { app: owner.app.clone(), now_ns: 0, catalog, artifacts },
    }))).unwrap();
    let key = id("edit");
    let op = Operation::ReplaceText { reference: reference.clone(), observation: observation.clone(),
        start: 0, end: 0, text: source.request.arguments.text.clone() };
    RequestStore::validate_ui_input(&dir, &owner, &key, &op).unwrap();
    let Recovery::Prepared(Request::Ui(request)) = store.prepare(key.clone(), op.clone()).unwrap() else { panic!() };
    store.dispatch(&key).unwrap();
    let complete = |receipt| Completion::Ui(Box::new(UiCompletion {
        generation: request.generation.clone(), pid: owner.runner.pid, last_id: request.request_id,
        request: request.clone(), receipt_sha256: "c".repeat(64),
        outcome: UiOutcome::TextEdit { app: owner.app.clone(), reference: reference.clone(),
            observation: observation.clone(), now_ns: 0, delivery: Delivery::UikitTextControl { receipt:Box::new(receipt) } },
    }));
    let mut changed = source.clone();
    if let TextInput::Focused { token, .. } = &mut changed.request.arguments.basis {
        *token = "abcd0000-0000-0000-0000-000000000008".into();
    }
    if let SourceResult::UikitTextInput { before, .. } = &mut changed.result {
        *before = changed.request.arguments.basis.clone();
    }
    assert!(store.complete(&key, complete(changed)).is_err());
    store.complete(&key, complete(source.clone())).unwrap();
    drop(store);
    let mut reopened = RequestStore::open(&dir, &owner).unwrap();
    assert_eq!(reopened.prepare(key, op).unwrap(), Recovery::Completed(complete(source)));
}

#[test]
fn external_entry_invalidates_retained_inputs_without_consuming_ui_sequence() {
    use frontcap_core::native::{ios_targets::{Catalog, Role, SelectorKey, Target},
        ios_ui::{UiCompletion, UiOutcome}, observation::Artifact};
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let owner = owner();
    let mut store = RequestStore::create(&dir, owner.clone()).unwrap();
    let observe = |store: &mut RequestStore, key: RequestId, suffix: u8| {
        let Recovery::Prepared(Request::Ui(request)) = store.prepare(key.clone(), Operation::ObserveTargets).unwrap() else { panic!() };
        store.dispatch(&key).unwrap();
        let observation = format!("abcd0000-0000-0000-0000-00000000000{suffix}");
        let reference = format!("{observation}-0");
        let catalog = Catalog { scrolls: vec![], transitioning: false,
            text_input: frontcap_core::native::ios_text::TextInput::Unfocused {},
            observation: observation.clone(), keyboard: false, targets: vec![Target {
                role: Role::Button, element_type: 9, identifier: "".into(), label: "Open".into(),
                value: None, selector_key: SelectorKey::Label, selector_value: "Open".into(),
                unique: true, enabled: true, selected: false, hittable: true, intersects_screen: true,
                frame_mpt: [0,0,1000,1000], reference: reference.clone(),
            }] };
        let artifacts = ["screen.png", "hierarchy.txt"].into_iter().map(|name| Artifact {
            path: format!("observations/{observation}/{name}"), bytes: 1, sha256: "a".repeat(64),
        }).collect();
        let number = request.request_id;
        store.complete(&key, Completion::Ui(Box::new(UiCompletion {
            generation: owner.runner.generation.clone(), pid: owner.runner.pid,
            last_id: number, request, receipt_sha256: "b".repeat(64),
            outcome: UiOutcome::ObservedTargets { app: owner.app.clone(), now_ns: 0, catalog, artifacts },
        }))).unwrap();
        (Operation::TapTarget { reference, observation }, number)
    };
    let (old, first_number) = observe(&mut store, id("before"), 8);
    RequestStore::validate_ui_input(&dir, &owner, &id("tap"), &old).unwrap();
    assert!(matches!(store.invalidate_for_external_entry(&id("before")), Err(StoreError::Conflict)));
    store.invalidate_for_external_entry(&id("url")).unwrap();
    drop(store);
    let mut store = RequestStore::open(&dir, &owner).unwrap();
    assert!(RequestStore::read_completion(&dir, &owner, &id("before"), &Operation::ObserveTargets).unwrap().is_some());
    assert!(RequestStore::validate_ui_input(&dir, &owner, &id("tap"), &old).is_err());
    let (fresh, next_number) = observe(&mut store, id("after"), 9);
    assert_eq!(next_number, first_number + 1);
    RequestStore::validate_ui_input(&dir, &owner, &id("tap"), &fresh).unwrap();
    store.prepare(id("pending"), Operation::ReadClock).unwrap();
    assert!(matches!(store.invalidate_for_external_entry(&id("another-url")), Err(StoreError::Pending)));
}

#[test]
fn host_state_blocks_app_work_but_preserves_original_results_and_system_observation() {
    use frontcap_core::native::{app_state::State, ios_lifecycle::StateChange,
        ios_ui::{UiCompletion, UiOutcome}};
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("requests");
    let owner = owner();
    let mut store = RequestStore::create(&path, owner.clone()).unwrap();
    let key = id("old-read");
    let Recovery::Prepared(read) = store.prepare(key.clone(), Operation::ReadClock).unwrap() else { panic!() };
    store.dispatch(&key).unwrap();
    let original = clock_completion(&read, false);
    store.complete(&key, original.clone()).unwrap();
    for (name, state, before_state, app_state, now_ns) in [
        ("background", State::Background, 4, 2, None),
        ("foreground", State::Foreground, 3, 4, Some(0)),
    ] {
        let operation = Operation::SetAppState { generation: owner.app.generation.clone(), state };
        let key = id(name);
        let Recovery::Prepared(Request::Ui(request)) = store.prepare(key.clone(), operation.clone()).unwrap() else { panic!() };
        store.dispatch(&key).unwrap();
        let receipt = Completion::Ui(Box::new(UiCompletion { generation: request.generation.clone(), pid: owner.runner.pid,
            last_id: request.request_id, request, receipt_sha256: "a".repeat(64),
            outcome: UiOutcome::AppStateChanged { app: owner.app.clone(), change: StateChange { requested: state, before_state, app_state, now_ns } } }));
        store.complete(&key, receipt.clone()).unwrap();
        drop(store);
        store = RequestStore::open(&path, &owner).unwrap();
        assert_eq!(store.prepare(key, operation.clone()).unwrap(), Recovery::Completed(receipt));
        assert!(store.prepare(id("duplicate-state"), operation).is_err());
        if state == State::Background {
            let before = std::fs::read(path.join("requests.json")).unwrap();
            for operation in [Operation::ReadClock, Operation::AdvanceClock { delta_ns: 1 }, Operation::ObserveTargets] {
                assert!(store.prepare(id("forbidden"), operation.clone()).is_err());
                assert!(RequestStore::validate_operation(&path, &owner, &id("forbidden"), &operation).is_err());
            }
            assert!(store.validate_external_entry(&id("url")).is_err());
            assert!(RequestStore::validate_replacement(&path, &owner, &id("restart")).is_err());
            assert!(RequestStore::validate_operation(&path, &owner, &id("system"), &Operation::ObserveSystemAlerts).is_ok());
            assert!(RequestStore::validate_operation(&path, &owner, &id("finish"), &Operation::Finish).is_ok());
            assert_eq!(store.prepare(id("old-read"), Operation::ReadClock).unwrap(), Recovery::Completed(original.clone()));
            assert_eq!(std::fs::read(path.join("requests.json")).unwrap(), before);
        }
    }
    assert!(store.prepare(id("fresh"), Operation::ObserveTargets).is_ok());
}

#[test]
fn incremental_requests_bind_allowance_and_keep_retirement_independent() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let root = tempfile::tempdir().unwrap(); let dir = root.path().join("requests");
    let limits = Limits { ordinary: 1024 * 1024, retirement: 64 * 1024 };
    let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::new("g-store").unwrap(), limits).unwrap();
    let mut store = RequestStore::create_epoch_with_budget(&dir, owner(), 2, Some(&budget)).unwrap();
    let mut half = 0;
    for number in 0..40 {
        let request_id = id(&format!("read-{number}"));
        let Recovery::Prepared(request) = store.prepare(request_id.clone(), Operation::ReadClock).unwrap() else { panic!() };
        store.dispatch(&request_id).unwrap();
        store.complete(&request_id, clock_completion(&request, false)).unwrap();
        if number == 19 { half = budget.charged().unwrap().ordinary; }
    }
    let charged = budget.charged().unwrap();
    assert!(charged.ordinary < half * 3, "new transitions must not rewrite accumulated history");
    assert_eq!(charged.retirement, 0);
    let count = store.head.as_ref().unwrap().event_count;
    let before = std::fs::read(dir.join("requests.json")).unwrap();
    drop(store);
    assert!(RequestStore::open(&dir, &owner()).is_err(), "missing allowance must not disable admission");
    assert!(RequestStore::read_completion(&dir, &owner(), &id("read-0"), &Operation::ReadClock).unwrap().is_some());
    let other = WriteBudget::create(&root.path().join("other"), LaunchId::new("g-other").unwrap(), limits).unwrap();
    assert!(RequestStore::open_with_budget(&dir, &owner(), Some(&other)).is_err());
    let mut store = RequestStore::open_with_budget(&dir, &owner(), Some(&budget)).unwrap();
    budget.charge(Class::Ordinary, limits.ordinary - charged.ordinary).unwrap();
    assert!(store.prepare(id("refused"), Operation::ReadClock).is_err());
    assert_eq!(std::fs::read(dir.join("requests.json")).unwrap(), before);
    assert!(!dir.join("events").join(format!("{count}.json")).exists());
    assert!(matches!(store.prepare(id("finish"), Operation::Finish).unwrap(), Recovery::Prepared(_)));
    let Request::Ui(request) = store.dispatch(&id("finish")).unwrap() else { panic!() };
    store.complete(&id("finish"), Completion::Ui(Box::new(frontcap_core::native::ios_ui::UiCompletion {
        generation: owner().runner.generation, pid: owner().runner.pid, last_id: request.request_id,
        request, outcome: frontcap_core::native::ios_ui::UiOutcome::Terminated {
            app: owner().app, process_absent: true, port_closed: true }, receipt_sha256: "b".repeat(64),
    }))).unwrap();
    assert!(budget.charged().unwrap().retirement > 0);
    drop(store);
    std::fs::remove_file(dir.join("events/0.json")).unwrap();
    assert!(RequestStore::read_completion(&dir, &owner(), &id("read-0"), &Operation::ReadClock).is_err());
}

#[test]
fn legacy_snapshot_remains_readable_and_keeps_its_layout_when_extended() {
    let root = tempfile::tempdir().unwrap(); let dir = root.path().join("requests");
    let mut store = RequestStore::create(&dir, owner()).unwrap();
    let request_id = id("legacy");
    let Recovery::Prepared(request) = store.prepare(request_id.clone(), Operation::ReadClock).unwrap() else { panic!() };
    store.dispatch(&request_id).unwrap(); store.complete(&request_id, clock_completion(&request, false)).unwrap();
    let state = store.state.clone(); drop(store);
    save_atomic(&dir.join("requests.json"), &state).unwrap();
    let mut legacy = RequestStore::open(&dir, &owner()).unwrap();
    assert!(legacy.head.is_none());
    assert!(matches!(legacy.recovery(&request_id).unwrap(), Recovery::Completed(_)));
    legacy.prepare(id("new"), Operation::ReadClock).unwrap(); drop(legacy);
    let saved: State = load(&dir.join("requests.json")).unwrap().unwrap();
    assert!(saved.bindings.contains_key(&id("new")));
}

#[test]
fn inherited_allowance_reaches_a_fresh_uikit_writer_process() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let root = tempfile::tempdir().unwrap(); let dir = root.path().join("requests");
    let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::new("g-child").unwrap(),
        Limits { ordinary: 64 * 1024, retirement: 64 * 1024 }).unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "ios_store::tests::inherited_allowance_writer_child", "--nocapture"])
        .env("FRONTCAP_IOS_STORE_CHILD", &dir);
    WriteBudget::configure_child(&mut command, Some(&budget)).unwrap();
    assert!(command.output().unwrap().status.success());
    assert!(budget.charged().unwrap().ordinary > 0);
    assert!(RequestStore::read_completion(&dir, &owner(), &id("child"), &Operation::ReadClock).unwrap().is_some());
    assert!(RequestStore::open(&dir, &owner()).is_err());
}
#[test]
fn inherited_allowance_writer_child() {
    let Some(directory) = std::env::var_os("FRONTCAP_IOS_STORE_CHILD") else { return; };
    let dir = PathBuf::from(directory);
    let mut store = RequestStore::create(&dir, owner()).unwrap();
    let Recovery::Prepared(request) = store.prepare(id("child"), Operation::ReadClock).unwrap() else { panic!() };
    store.dispatch(&id("child")).unwrap();
    store.complete(&id("child"), clock_completion(&request, false)).unwrap();
    drop(store);
    assert!(matches!(RequestStore::open(&dir, &owner()).unwrap().recovery(&id("child")).unwrap(), Recovery::Completed(_)));
}
