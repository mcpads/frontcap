use super::*;
use frontcap_core::native::flutter::{Advance, OperationName};
fn owner() -> Owner {
    Owner {
        platform: Platform::Android,
        device: "emulator-5580".into(),
        generation: "a".repeat(48),
        source_sha256: "b".repeat(64),
    }
}
fn id(value: &str) -> RequestId {
    RequestId::new(value).unwrap()
}
fn receipt(request: &Request) -> Completion {
    Completion {
        generation: request.generation.clone(),
        id: request.id.clone(),
        operation: request.operation.name(),
        logical_us: 0,
        held: true,
        faulted: false,
        advance: None,
        surface: None,
        capture: None,
        not_applied: None,
    }
}
fn read(store: &mut RequestStore, name: &str) -> Completion {
    let id = id(name);
    store.prepare(id.clone(), Operation::Read {}).unwrap();
    let request = store.dispatch(&id).unwrap();
    let receipt = receipt(&request);
    store.complete(&id, receipt.clone()).unwrap();
    receipt
}
#[test]
fn lost_reply_reopens_as_reconcile_and_never_authorizes_second_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let owner = owner();
    let mut store = RequestStore::create(&dir, owner.clone()).unwrap();
    let id = id("advance");
    store
        .prepare(id.clone(), Operation::Advance { delta_us: 1000 })
        .unwrap();
    let request = store.dispatch(&id).unwrap();
    drop(store);
    let mut store = RequestStore::open(&dir, &owner).unwrap();
    assert!(store.validate_replacement(&RequestId::new("restart").unwrap()).is_err());
    assert_eq!(
        store.recovery(&id).unwrap(),
        Recovery::Reconcile(request.clone())
    );
    assert!(matches!(
        store.dispatch(&id),
        Err(StoreError::AlreadyDispatched)
    ));
    assert!(matches!(
        store.prepare(RequestId::new("other").unwrap(), Operation::Read {}),
        Err(StoreError::Pending)
    ));
    let mut result = receipt(&request);
    result.logical_us = 1000;
    result.advance = Some(Advance {
        before_us: 0,
        requested_us: 1000,
        after_us: 1000,
        time_changed: true,
        callbacks: 1,
        drain_completed: true,
        framework_frame_completed: true,
        failure: None,
        cancelled_on_failure: 0,
    });
    store.complete(&id, result.clone()).unwrap();
    drop(store);
    assert_eq!(
        RequestStore::read_completion(&dir, &owner, &id, &request.operation).unwrap(),
        Some(result.clone())
    );
    let mut store = RequestStore::open(&dir, &owner).unwrap();
    assert_eq!(
        store
            .prepare(id.clone(), request.operation.clone())
            .unwrap(),
        Recovery::Completed(Box::new(result))
    );
    assert!(matches!(
        store.prepare(id, Operation::Advance { delta_us: 1 }),
        Err(StoreError::Conflict)
    ));
}
#[test]
fn stale_basis_rejects_after_input_but_completed_input_remains_queryable() {
    for input in 0..6 {
        let text_edit = input == 1;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("store");
        let mut store = RequestStore::create(&dir, owner()).unwrap();
        let observe = id("observe");
        store
            .prepare(observe.clone(), Operation::Observe {})
            .unwrap();
        let request = store.dispatch(&observe).unwrap();
        let mut result = receipt(&request);
        result.surface = Some(Observation {
            observation: "o1".into(),
            revision: 1,
            framework_frame: 1,
            logical_us: 0,
            view: None,
            nodes: vec![frontcap_core::native::flutter::Node {
                id: "s1".into(),
                label: "button".into(),
                tooltip: "".into(),
                value: "".into(),
                scroll_actions: if input == 2 {
                    vec![frontcap_core::native::flutter::ScrollDirection::Up]
                } else {
                    vec![]
                },
                scroll_position: None,
                scroll_min: None,
                scroll_max: None,
                tappable: true,
                editable: text_edit,
                focused: text_edit,
                rect: [0.0, 0.0, 10.0, 10.0],
            }],
        });
        store.complete(&observe, result).unwrap();
        let tap = id("tap");
        let operation = if text_edit {
            Operation::ReplaceText {
                observation: "o1".into(),
                target: "s1".into(),
                start: 0,
                end: 0,
                text: "부산 여행".into(),
            }
        } else if input == 2 {
            Operation::Scroll {
                observation: "o1".into(),
                target: "s1".into(),
                direction: frontcap_core::native::flutter::ScrollDirection::Up,
            }
        } else {
            Operation::Tap {
                observation: "o1".into(),
                target: "s1".into(),
            }
        };
        if input >= 4 {
            if input == 4 { store.invalidate_for_external_entry(&id("entry")).unwrap(); }
            else { store.invalidate_for_system_operation(&id("entry")).unwrap(); }
            assert!(store.validate_external_entry(&observe).is_err());
            assert!(store.prepare(id("entry"), Operation::Read {}).is_err());
            drop(store);
            let mut store = RequestStore::open(&dir, &owner()).unwrap();
            assert!(store.prepare(tap, operation).is_err());
            assert!(matches!(store.prepare(observe, Operation::Observe {}).unwrap(), Recovery::Completed(_)));
            // A historical result cannot restore the invalidated targeting basis.
            assert!(store.prepare(id("capture"), Operation::Capture { observation: "o1".into() }).is_err());
            store.prepare(id("fresh"), Operation::Observe {}).unwrap();
            continue;
        }
        store.prepare(tap.clone(), operation.clone()).unwrap();
        let request = store.dispatch(&tap).unwrap();
        drop(store);
        let mut store = RequestStore::open(&dir, &owner()).unwrap();
        assert_eq!(
            store.recovery(&tap).unwrap(),
            Recovery::Reconcile(request.clone())
        );
        assert!(matches!(
            store.dispatch(&tap),
            Err(StoreError::AlreadyDispatched)
        ));
        let mut outcome = receipt(&request);
        if input == 3 {
            outcome.not_applied =
                Some(frontcap_core::native::flutter::NotApplied::ObservationChanged);
        }
        store.complete(&tap, outcome).unwrap();
        assert!(matches!(
            store.prepare(tap, operation.clone()).unwrap(),
            Recovery::Completed(_)
        ));
        assert!(store.prepare(id("stale-tap"), operation).is_err());
        assert!(
            store
                .prepare(
                    id("stale-capture"),
                    Operation::Capture {
                        observation: "o1".into()
                    }
                )
                .is_err()
        );
    }
}

#[test]
fn failed_advance_retains_applied_time_and_fences_new_mutation() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let mut store = RequestStore::create(&dir, owner()).unwrap();
    let advance = id("advance");
    store
        .prepare(advance.clone(), Operation::Advance { delta_us: 2000 })
        .unwrap();
    let request = store.dispatch(&advance).unwrap();
    let mut result = receipt(&request);
    result.logical_us = 1000;
    result.faulted = true;
    result.advance = Some(Advance {
        before_us: 0,
        requested_us: 2000,
        after_us: 1000,
        time_changed: true,
        callbacks: 8,
        drain_completed: false,
        framework_frame_completed: false,
        failure: Some("callback limit".into()),
        cancelled_on_failure: 1,
    });
    store.complete(&advance, result).unwrap();
    assert!(store.validate_replacement(&id("restart")).is_ok());
    assert!(store.validate_replacement(&advance).is_err());
    assert!(
        store
            .prepare(id("more"), Operation::Advance { delta_us: 1 })
            .is_err()
    );
    let read = id("read");
    store.prepare(read.clone(), Operation::Read {}).unwrap();
    let request = store.dispatch(&read).unwrap();
    let mut result = receipt(&request);
    result.logical_us = 1000;
    assert!(store.complete(&read, result.clone()).is_err());
    result.faulted = true;
    store.complete(&read, result).unwrap();
}
#[test]
fn owner_lock_missing_state_and_changed_generation_do_not_authorize_adoption() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let owner = owner();
    let mut store = RequestStore::create(&dir, owner.clone()).unwrap();
    assert!(matches!(
        RequestStore::open(&dir, &owner),
        Err(StoreError::Busy)
    ));
    read(&mut store, "read");
    drop(store);
    let mut other = owner.clone();
    other.generation = "c".repeat(48);
    assert!(matches!(
        RequestStore::open(&dir, &other),
        Err(StoreError::IdentityChanged)
    ));
    std::fs::remove_file(dir.join("requests.json")).unwrap();
    assert!(RequestStore::open(&dir, &owner).is_err());
}
#[test]
fn storage_failure_poisoning_cannot_be_used_as_dispatch_authority() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let mut store = RequestStore::create(&dir, owner()).unwrap();
    let id = id("read");
    store.prepare(id.clone(), Operation::Read {}).unwrap();
    std::fs::rename(dir.join("requests.json"), dir.join("preserved.json")).unwrap();
    std::fs::create_dir(dir.join("requests.json")).unwrap();
    assert!(store.dispatch(&id).is_err());
    assert!(matches!(
        store.dispatch(&id),
        Err(StoreError::StorageUncertain)
    ));
    assert!(matches!(
        store.recovery(&id),
        Err(StoreError::StorageUncertain)
    ));
}
#[test]
fn corrupt_receipts_and_completed_stop_fence_history() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let owner = owner();
    let mut store = RequestStore::create(&dir, owner.clone()).unwrap();
    read(&mut store, "read");
    let stop = id("stop");
    store.prepare(stop.clone(), Operation::Stop {}).unwrap();
    let request = store.dispatch(&stop).unwrap();
    store.complete(&stop, receipt(&request)).unwrap();
    assert!(store.prepare(id("late"), Operation::Read {}).is_err());
    drop(store);
    let path = dir.join("events/2.json");
    let mut event: history::Event = load(&path).unwrap().unwrap();
    let history::Event::Request { entry, .. } = &mut event else { panic!() };
    let Progress::Completed { receipt } = &mut entry.progress else { panic!() };
    receipt.operation = OperationName::Tap;
    save_atomic(&path, &event).unwrap();
    assert!(RequestStore::open(&dir, &owner).is_err());
}

#[test]
fn prepared_restart_preserves_one_dispatch_and_long_caller_identity() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let owner = owner();
    let mut store = RequestStore::create(&dir, owner.clone()).unwrap();
    let caller = RequestId::new("a".repeat(128)).unwrap();
    let Recovery::Prepared(original) = store.prepare(caller.clone(), Operation::Read {}).unwrap()
    else {
        panic!()
    };
    assert!(original.id.len() <= 100);
    drop(store);
    let mut store = RequestStore::open(&dir, &owner).unwrap();
    assert_eq!(store.dispatch(&caller).unwrap(), original);
    assert!(matches!(
        store.dispatch(&caller),
        Err(StoreError::AlreadyDispatched)
    ));
    store.complete(&caller, receipt(&original)).unwrap();
    assert!(
        RequestStore::read_completion(&dir, &owner, &caller, &Operation::Read {})
            .unwrap()
            .is_some()
    );
}

#[test]
fn shared_allowance_refuses_writes_without_losing_original_request_state() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::new("g-budget").unwrap(),
        Limits { ordinary: 1 << 20, retirement: 4096 }).unwrap();
    let mut store = RequestStore::create_with_budget(&dir, owner(), Some(&budget)).unwrap();
    let original = read(&mut store, "original");
    let pending = id("pending");
    store.prepare(pending.clone(), Operation::Read {}).unwrap();
    let request = store.dispatch(&pending).unwrap();
    let saved = std::fs::read(dir.join("requests.json")).unwrap();
    budget.charge(Class::Ordinary, (1 << 20) - budget.charged().unwrap().ordinary).unwrap();
    assert!(store.complete(&pending, receipt(&request)).is_err());
    assert_eq!(store.recovery(&pending).unwrap(), Recovery::Reconcile(request));
    assert_eq!(std::fs::read(dir.join("requests.json")).unwrap(), saved);
    assert_eq!(store.prepare(id("original"), Operation::Read {}).unwrap(), Recovery::Completed(Box::new(original.clone())));
    assert!(store.prepare(id("new"), Operation::Read {}).is_err());
    assert_eq!(budget.charged().unwrap().ordinary, 1 << 20);
    assert_eq!(budget.charged().unwrap().retirement, 0);
    drop(store);
    let reopened = RequestStore::open_with_budget(&dir, &owner(), Some(&budget)).unwrap();
    assert!(matches!(reopened.recovery(&pending).unwrap(), Recovery::Reconcile(_)));
    assert_eq!(RequestStore::read_completion(&dir, &owner(), &id("original"), &Operation::Read {}).unwrap(), Some(original));
    drop(reopened);

    let empty = root.path().join("empty");
    let mut store = RequestStore::create(&empty, owner()).unwrap();
    drop(store);
    store = RequestStore::open_with_budget(&empty, &owner(), Some(&budget)).unwrap();
    let before = std::fs::read(empty.join("requests.json")).unwrap();
    assert!(store.prepare(id("refused"), Operation::Read {}).is_err());
    assert!(!store.contains(&id("refused")));
    assert!(store.validate_operation(&id("refused"), &Operation::Read {}).is_ok());
    assert_eq!(std::fs::read(empty.join("requests.json")).unwrap(), before);
}

#[test]
fn event_history_has_bounded_write_cost_and_preserves_legacy_reads() {
    use frontcap_core::{ids::LaunchId, write_budget::Limits};
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::new("g-history").unwrap(),
        Limits { ordinary: 1 << 20, retirement: 4096 }).unwrap();
    let mut store = RequestStore::create_with_budget(&dir, owner(), Some(&budget)).unwrap();
    let start = budget.charged().unwrap().ordinary;
    for n in 10..30 { read(&mut store, &format!("read-{n}")); }
    let middle = budget.charged().unwrap().ordinary;
    let original = std::fs::read(dir.join("events/2.json")).unwrap();
    for n in 30..50 { read(&mut store, &format!("read-{n}")); }
    let end = budget.charged().unwrap().ordinary;
    // Equal-size batches must not rewrite the previous batch's accumulated receipts.
    assert!(end - middle < (middle - start) * 11 / 10);
    assert_eq!(std::fs::read(dir.join("events/2.json")).unwrap(), original);
    let legacy = root.path().join("legacy");
    std::fs::create_dir(&legacy).unwrap();
    save_atomic(&legacy.join("requests.json"), &store.state).unwrap();
    let expected = RequestStore::read_completion(&dir, &owner(), &id("read-10"), &Operation::Read {}).unwrap();
    drop(store);
    assert_eq!(RequestStore::read_completion(&dir, &owner(), &id("read-10"), &Operation::Read {}).unwrap(), expected);
    let mut reopened = RequestStore::open_with_budget(&dir, &owner(), Some(&budget)).unwrap();
    read(&mut reopened, "after-reopen");
    let mut old = RequestStore::open(&legacy, &owner()).unwrap();
    assert_eq!(old.prepare(id("read-10"), Operation::Read {}).unwrap(), Recovery::Completed(Box::new(expected.unwrap())));
    read(&mut old, "legacy-next");
    assert!(load::<State>(&legacy.join("requests.json")).unwrap().is_some());
}

#[test]
fn unpublished_or_missing_events_never_authorize_replay_or_repair_evidence() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("store");
    let mut store = RequestStore::create(&dir, owner()).unwrap();
    read(&mut store, "original");
    drop(store);
    let orphan = dir.join("events/3.json");
    std::fs::write(&orphan, b"unpublished evidence").unwrap();
    let mut reopened = RequestStore::open(&dir, &owner()).unwrap();
    assert!(reopened.prepare(id("new"), Operation::Read {}).is_err());
    assert_eq!(std::fs::read(&orphan).unwrap(), b"unpublished evidence");
    drop(reopened);
    assert!(RequestStore::read_completion(&dir, &owner(), &id("original"), &Operation::Read {}).unwrap().is_some());
    std::fs::remove_file(dir.join("events/1.json")).unwrap();
    assert!(RequestStore::read_completion(&dir, &owner(), &id("original"), &Operation::Read {}).is_err());
    assert!(RequestStore::open(&dir, &owner()).is_err());
    assert!(!dir.join("events/1.json").exists());
}
