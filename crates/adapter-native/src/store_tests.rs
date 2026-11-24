use super::*;
use crate::request::*;

const GENERATION: &str = "4d75203e-0a3e-4c68-8e3b-77f0a831ab83";
const CONTROLLER: &str = "7a30a129-7ab1-4f10-b218-9e178f794970";
const SOURCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
fn identity() -> Identity {
    Identity {
        target: Target::AndroidViews,
        device: "emulator-5580".into(),
        generation: GENERATION.into(),
        pid: 123,
    }
}
fn id(s: &str) -> RequestId {
    RequestId::new(s).unwrap()
}
fn session(dir: &Path) -> RequestStore {
    RequestStore::create(dir, identity(), CONTROLLER.into(), SOURCE.into()).unwrap()
}
fn completion(request: NativeRequest) -> Completion {
    match request {
        NativeRequest::ViewScroll(_) | NativeRequest::ViewTap(_) | NativeRequest::ViewText(_) => panic!("expected a clock request"),
        NativeRequest::Clock(request) => Completion::Clock {
            generation: GENERATION.into(),
            pid: 123,
            last_id: request.request_id,
            before_ns: 0,
            now_ns: request.delta_ns,
            applied: request.operation == ClockOperation::Advance,
            request,
            receipt_sha256: SOURCE.into(),
        },
    }
}

#[test]
fn lost_reply_reopens_exact_request_without_authorizing_another_input() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let mut store = session(&dir);
    store.prepare(id("undo"), Operation::Advance { by_us: 1 }).unwrap();
    let sent = store.dispatch(&id("undo")).unwrap();
    drop(store);
    let mut reopened = RequestStore::open(&dir, &identity(), SOURCE).unwrap();
    assert_eq!(
        reopened.prepare(id("undo"), Operation::Advance { by_us: 1 }).unwrap(),
        Recovery::Reconcile(sent.clone())
    );
    assert!(matches!(
        reopened.dispatch(&id("undo")),
        Err(StoreError::AlreadyDispatched)
    ));
    assert!(matches!(
        reopened.prepare(id("another"), Operation::ReadClock {}),
        Err(StoreError::Pending)
    ));
    let receipt = completion(sent);
    reopened.complete(&id("undo"), receipt.clone()).unwrap();
    drop(reopened);
    let mut reopened = RequestStore::open(&dir, &identity(), SOURCE).unwrap();
    assert_eq!(
        reopened.prepare(id("undo"), Operation::Advance { by_us: 1 }).unwrap(),
        Recovery::Completed(receipt)
    );
    let Recovery::Prepared(NativeRequest::Clock(next)) =
        reopened.prepare(id("next-undo"), Operation::Advance { by_us: 1 }).unwrap()
    else {
        panic!()
    };
    assert_eq!(next.request_id, 2);
}

#[test]
fn prepared_intent_survives_restart_and_conflicting_arguments_do_not_replace_it() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let mut store = session(&dir);
    let expected = store
        .prepare(id("advance"), Operation::Advance { by_us: 4_999_000 })
        .unwrap();
    drop(store);
    let mut store = RequestStore::open(&dir, &identity(), SOURCE).unwrap();
    assert!(matches!(
        store.prepare(id("advance"), Operation::Advance { by_us: 5_000_000 }),
        Err(StoreError::Conflict)
    ));
    assert!(matches!(
        store.prepare(id("advance"), Operation::Advance { by_us: u64::MAX }),
        Err(StoreError::Conflict)
    ));
    assert_eq!(store.recovery(&id("advance")).unwrap(), expected);
    let NativeRequest::Clock(request) = store.dispatch(&id("advance")).unwrap() else {
        panic!()
    };
    assert_eq!(request.delta_ns, 4_999_000_000);
    assert_eq!(request.request_id, 1);
}

#[test]
fn target_replacement_changed_source_and_second_owner_cannot_adopt_session() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let store = session(&dir);
    assert!(matches!(
        RequestStore::open(&dir, &identity(), SOURCE),
        Err(StoreError::Busy)
    ));
    drop(store);
    let mut replacement = identity();
    replacement.pid += 1;
    assert!(matches!(
        RequestStore::open(&dir, &replacement, SOURCE),
        Err(StoreError::IdentityChanged)
    ));
    replacement = identity();
    replacement.generation = CONTROLLER.into();
    assert!(matches!(
        RequestStore::open(&dir, &replacement, SOURCE),
        Err(StoreError::IdentityChanged)
    ));
    assert!(matches!(
        RequestStore::open(&dir, &identity(), &"b".repeat(64)),
        Err(StoreError::IdentityChanged)
    ));
    assert!(RequestStore::create(&dir, identity(), CONTROLLER.into(), SOURCE.into()).is_err());
}

#[test]
fn invalid_or_unsupported_intent_does_not_consume_a_native_number() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let mut unsupported = identity();
    unsupported.pid = 0;
    assert!(RequestStore::create(&dir, unsupported, CONTROLLER.into(), SOURCE.into()).is_err());
    assert!(!dir.exists());
    let mut store = session(&dir);
    assert!(
        store
            .prepare(id("overflow"), Operation::Advance { by_us: u64::MAX })
            .is_err()
    );
    assert!(
        serde_json::from_str::<Operation>(r#"{"operation":"evaluate","expression":"1"}"#).is_err()
    );
    assert!(serde_json::from_str::<Operation>(r#"{"operation":"read_clock","by_us":1}"#).is_err());
    let Recovery::Prepared(NativeRequest::Clock(request)) =
        store.prepare(id("valid"), Operation::ReadClock {}).unwrap()
    else {
        panic!()
    };
    assert_eq!(request.request_id, 1);
}

#[test]
fn mismatched_receipts_cannot_clear_pending_or_change_committed_result() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let mut store = session(&dir);
    let Recovery::Prepared(request) = store
        .prepare(id("advance"), Operation::Advance { by_us: 1000 })
        .unwrap()
    else {
        panic!()
    };
    let good = completion(request.clone());
    assert!(matches!(
        store.complete(&id("advance"), good.clone()),
        Err(StoreError::NotDispatched)
    ));
    store.dispatch(&id("advance")).unwrap();
    for variant in 0..5 {
        let mut bad = good.clone();
        if let Completion::Clock {
            pid,
            request,
            now_ns,
            applied,
            generation,
            ..
        } = &mut bad
        {
            match variant {
                0 => *pid += 1,
                1 => request.request_id += 1,
                2 => *now_ns += 1,
                3 => *applied = false,
                _ => *generation = CONTROLLER.into(),
            }
        }
        assert!(store.complete(&id("advance"), bad).is_err());
        assert_eq!(
            store.recovery(&id("advance")).unwrap(),
            Recovery::Reconcile(request.clone())
        );
    }
    store.complete(&id("advance"), good.clone()).unwrap();
    store.complete(&id("advance"), good.clone()).unwrap();
    let mut changed = good.clone();
    if let Completion::Clock { receipt_sha256, .. } = &mut changed {
        *receipt_sha256 = "b".repeat(64);
    }
    assert!(matches!(
        store.complete(&id("advance"), changed),
        Err(StoreError::Conflict)
    ));
    assert_eq!(
        store.recovery(&id("advance")).unwrap(),
        Recovery::Completed(good)
    );
}

#[test]
fn corrupt_or_missing_state_is_not_replaced_by_an_empty_history() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let mut store = session(&dir);
    store
        .prepare(id("pending"), Operation::ReadClock {})
        .unwrap();
    drop(store);
    let path = dir.join("requests.json");
    let original = std::fs::read(&path).unwrap();
    let mut saved: serde_json::Value = serde_json::from_slice(&original).unwrap();
    saved["bindings"]["pending"]["request"]["request"]["request_id"] = 2.into();
    std::fs::write(&path, serde_json::to_vec(&saved).unwrap()).unwrap();
    assert!(RequestStore::open(&dir, &identity(), SOURCE).is_err());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap()).unwrap(),
        saved
    );
    std::fs::remove_file(&path).unwrap();
    assert!(RequestStore::open(&dir, &identity(), SOURCE).is_err());
    assert!(!path.exists());
}

#[test]
fn storage_failure_stops_further_dispatch_until_reopened() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let mut store = session(&dir);
    store.prepare(id("read"), Operation::ReadClock {}).unwrap();
    let path = dir.join("requests.json");
    let saved = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(matches!(
        store.dispatch(&id("read")),
        Err(StoreError::Io(_))
    ));
    assert!(matches!(
        store.dispatch(&id("read")),
        Err(StoreError::StorageUncertain)
    ));
    assert!(matches!(
        store.prepare(id("read"), Operation::ReadClock {}),
        Err(StoreError::StorageUncertain)
    ));
    std::fs::remove_dir(&path).unwrap();
    std::fs::write(&path, saved).unwrap();
    drop(store);
    let reopened = RequestStore::open(&dir, &identity(), SOURCE).unwrap();
    assert!(matches!(
        reopened.recovery(&id("read")),
        Ok(Recovery::Prepared(_))
    ));
}

#[test]
fn non_consuming_rejection_survives_restart_and_keeps_the_native_sequence_available() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let mut store = session(&dir);
    store
        .prepare(id("overflow"), Operation::Advance { by_us: 1000 })
        .unwrap();
    let NativeRequest::Clock(request) = store.dispatch(&id("overflow")).unwrap() else {
        panic!()
    };
    let rejected = Completion::ClockRejected {
        request,
        generation: GENERATION.into(),
        pid: 123,
        last_id: 0,
        before_ns: i64::MAX as u64,
        now_ns: i64::MAX as u64,
        applied: false,
        error: ClockRejection::ClockOverflow,
        receipt_sha256: SOURCE.into(),
    };
    let mut uncertain = rejected.clone();
    if let Completion::ClockRejected { applied, .. } = &mut uncertain {
        *applied = true;
    }
    assert!(store.complete(&id("overflow"), uncertain).is_err());
    store.complete(&id("overflow"), rejected.clone()).unwrap();
    drop(store);
    let mut store = RequestStore::open(&dir, &identity(), SOURCE).unwrap();
    assert_eq!(
        store.recovery(&id("overflow")).unwrap(),
        Recovery::Completed(rejected.clone())
    );
    let Recovery::Prepared(NativeRequest::Clock(next)) = store
        .prepare(id("read-after-rejection"), Operation::ReadClock {})
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(next.request_id, 1);
    let sent = store.dispatch(&id("read-after-rejection")).unwrap();
    store
        .complete(&id("read-after-rejection"), completion(sent))
        .unwrap();
    drop(store);
    let store = RequestStore::open(&dir, &identity(), SOURCE).unwrap();
    assert_eq!(
        store.recovery(&id("overflow")).unwrap(),
        Recovery::Completed(rejected)
    );
}

#[cfg(unix)]
fn release_while_child_retains_descriptor(release: impl FnOnce()) {
    let mut pipe = [0; 2];
    assert_eq!(unsafe { libc::pipe(pipe.as_mut_ptr()) }, 0);
    let child = unsafe { libc::fork() };
    assert!(child >= 0);
    if child == 0 {
        // No Rust runtime, allocation or destructors after fork in a multithreaded test process.
        unsafe {
            libc::close(pipe[1]);
            let mut byte = 0u8;
            libc::read(pipe[0], (&mut byte as *mut u8).cast(), 1);
            libc::_exit(0);
        }
    }
    unsafe {
        libc::close(pipe[0]);
    }
    let observed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(release));
    unsafe {
        let byte = 1u8;
        libc::write(pipe[1], (&byte as *const u8).cast(), 1);
        libc::close(pipe[1]);
        libc::waitpid(child, std::ptr::null_mut(), 0);
    }
    if let Err(error) = observed { std::panic::resume_unwind(error); }
}

#[cfg(unix)]
#[test]
fn closing_owner_releases_lock_even_while_forked_child_holds_inherited_descriptor() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let store = session(&dir);
    release_while_child_retains_descriptor(|| {
        drop(store);
        assert!(RequestStore::open(&dir, &identity(), SOURCE).is_ok(),
            "normal owner release must not depend on a child's future exec/exit");
    });
}

#[cfg(unix)]
#[test]
fn failed_store_preparation_releases_lock_while_child_retains_descriptor() {
    let temp = tempfile::tempdir().unwrap();
    // The lease is acquired before validation or publication can reject construction.
    let lease = lock(temp.path()).unwrap();
    release_while_child_retains_descriptor(|| {
        fn reject_preparation<T>(_lease: T) -> Result<(), StoreError> {
            Err(StoreError::IdentityChanged)
        }
        let rejected = reject_preparation(lease);
        assert!(matches!(rejected, Err(StoreError::IdentityChanged)));
        assert!(lock(temp.path()).is_ok(),
            "failed preparation must release ownership before an unrelated child exits");
    });
}

#[test]
fn offline_completion_lookup_requires_matching_evidence_and_never_changes_history() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path().join("session");
    let mut store = session(&dir);
    let request_id = id("recover-clock");
    let operation = Operation::ReadClock {};
    let lookup = || {
        RequestStore::read_completion(
            &dir,
            &identity(),
            CONTROLLER,
            SOURCE,
            &request_id,
            &operation,
        )
    };
    assert_eq!(lookup().unwrap(), None);
    store
        .prepare(request_id.clone(), operation.clone())
        .unwrap();
    let prepared = std::fs::read(dir.join("requests.json")).unwrap();
    assert_eq!(lookup().unwrap(), None);
    assert_eq!(std::fs::read(dir.join("requests.json")).unwrap(), prepared);
    let sent = store.dispatch(&request_id).unwrap();
    let dispatched = std::fs::read(dir.join("requests.json")).unwrap();
    assert_eq!(lookup().unwrap(), None);
    assert_eq!(
        std::fs::read(dir.join("requests.json")).unwrap(),
        dispatched
    );
    let receipt = completion(sent);
    store.complete(&request_id, receipt.clone()).unwrap();
    let completed = std::fs::read(dir.join("requests.json")).unwrap();
    // A read-only lookup can inspect committed evidence while the owner still holds its lock.
    assert_eq!(lookup().unwrap(), Some(receipt));
    assert!(
        RequestStore::read_completion(
            &dir,
            &identity(),
            CONTROLLER,
            SOURCE,
            &request_id,
            &Operation::Advance { by_us: 1 },
        )
        .is_err()
    );
    assert!(
        RequestStore::read_completion(
            &dir,
            &identity(),
            GENERATION,
            SOURCE,
            &request_id,
            &operation,
        )
        .is_err()
    );
    let mut foreign = identity();
    foreign.pid += 1;
    assert!(
        RequestStore::read_completion(&dir, &foreign, CONTROLLER, SOURCE, &request_id, &operation,)
            .is_err()
    );
    assert!(
        RequestStore::read_completion(
            &dir,
            &identity(),
            CONTROLLER,
            &"b".repeat(64),
            &request_id,
            &operation,
        )
        .is_err()
    );
    assert_eq!(std::fs::read(dir.join("requests.json")).unwrap(), completed);
}



#[test]
fn budgeted_store_reopen_requires_original_allowance_but_reads_do_not() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    let temp=tempfile::tempdir().unwrap();let root=temp.path();let dir=root.join("requests");
    let budget=WriteBudget::create(&root.join("budget"),LaunchId::new("g-store").unwrap(),
        Limits {ordinary:65536,retirement:4096}).unwrap();
    let mut store=RequestStore::create_with_budget(&dir,identity(),CONTROLLER.into(),SOURCE.into(),Some(&budget)).unwrap();
    store.prepare(id("read"),Operation::ReadClock {}).unwrap();
    let request=store.dispatch(&id("read")).unwrap();let receipt=completion(request);
    store.complete(&id("read"),receipt.clone()).unwrap();drop(store);
    let bytes=std::fs::read(dir.join("requests.json")).unwrap();
    assert!(RequestStore::open_with_budget(&dir,&identity(),SOURCE,None).is_err());
    let other=WriteBudget::create(&root.join("other"),LaunchId::new("g-other").unwrap(),
        Limits {ordinary:65536,retirement:4096}).unwrap();
    assert!(RequestStore::open_with_budget(&dir,&identity(),SOURCE,Some(&other)).is_err());
    budget.charge(Class::Ordinary,65536-budget.charged().unwrap().ordinary).unwrap();
    let mut store=RequestStore::open_with_budget(&dir,&identity(),SOURCE,Some(&budget)).unwrap();
    store.complete(&id("read"),receipt.clone()).unwrap();
    assert_eq!(RequestStore::read_completion(&dir,&identity(),CONTROLLER,SOURCE,&id("read"),&Operation::ReadClock {}).unwrap(),Some(receipt));
    assert!(store.prepare(id("new"),Operation::ReadClock {}).is_err());
    assert!(matches!(store.recovery(&id("new")),Err(StoreError::Unknown)));
    assert_eq!(std::fs::read(dir.join("requests.json")).unwrap(),bytes);
    assert_eq!(budget.charged().unwrap(),Limits {ordinary:65536,retirement:0});
}

#[test]
fn completion_refusal_retains_dispatched_identity_and_legacy_extension_binds_budget() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    let temp=tempfile::tempdir().unwrap();let root=temp.path();let dir=root.join("requests");
    drop(session(&dir));
    let budget=WriteBudget::create(&root.join("budget"),LaunchId::new("g-legacy").unwrap(),
        Limits {ordinary:65536,retirement:4096}).unwrap();
    let mut store=RequestStore::open_with_budget(&dir,&identity(),SOURCE,Some(&budget)).unwrap();
    store.prepare(id("read"),Operation::ReadClock {}).unwrap();
    let mut expected=std::fs::metadata(dir.join("requests.json")).unwrap().len();
    let request=store.dispatch(&id("read")).unwrap();
    expected+=std::fs::metadata(dir.join("requests.json")).unwrap().len();
    assert_eq!(budget.charged().unwrap().ordinary,expected);
    budget.charge(Class::Ordinary,65536-expected).unwrap();
    let bytes=std::fs::read(dir.join("requests.json")).unwrap();
    assert!(store.complete(&id("read"),completion(request.clone())).is_err());
    assert_eq!(store.recovery(&id("read")).unwrap(),Recovery::Reconcile(request));
    assert!(store.dispatch(&id("read")).is_err());
    assert_eq!(std::fs::read(dir.join("requests.json")).unwrap(),bytes);drop(store);
    assert!(RequestStore::open_with_budget(&dir,&identity(),SOURCE,None).is_err());
    assert_eq!(budget.charged().unwrap().retirement,0);
}
