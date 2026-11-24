use super::*;
use frontcap_core::{canonical::sha256_bytes_hex, native::observation::Artifact};

fn id(value: &str) -> RequestId {
    RequestId::new(value).unwrap()
}
fn owners() -> (Owner, UiIdentity) {
    let device = "ABCD0000-0000-0000-0000-000000000001".to_string();
    (
        Owner {
            platform: Platform::Ios,
            device: device.clone(),
            generation: "a".repeat(48),
            source_sha256: "b".repeat(64),
        },
        UiIdentity {
            simulator: device,
            generation: "abcd0000-0000-0000-0000-000000000002".into(),
            pid: 42,
        },
    )
}
fn observed(target: &Path, runner: &UiIdentity, request: UiRequest) -> UiCompletion {
    let catalog: Catalog =
        serde_json::from_str(include_str!("../../core/tests/fixtures/ios-system-ui.json")).unwrap();
    let client = client_directory(target);
    let artifacts = ["screen.png", "hierarchy.txt"]
        .into_iter()
        .map(|name| {
            let path = format!("observations/{}/{name}", catalog.observation);
            std::fs::create_dir_all(client.join(&path).parent().unwrap()).unwrap();
            std::fs::write(client.join(&path), b"fixture").unwrap();
            Artifact {
                path,
                bytes: 7,
                sha256: sha256_bytes_hex(b"fixture"),
            }
        })
        .collect::<Vec<_>>();
    let raw = serde_json::to_vec(
        &serde_json::json!({"request":request, "generation":runner.generation,
        "pid":runner.pid,"port":1234,"last_id":request.request_id,"status":"ok",
        "result":{"state":"system_observed","catalog":catalog,"artifacts":artifacts}}),
    )
    .unwrap();
    let digest = sha256_bytes_hex(&raw);
    std::fs::create_dir_all(client.join("receipts")).unwrap();
    std::fs::write(client.join("receipts").join(format!("{digest}.json")), raw).unwrap();
    UiCompletion {
        generation: runner.generation.clone(),
        pid: runner.pid,
        last_id: request.request_id,
        request,
        receipt_sha256: digest,
        outcome: UiOutcome::SystemAlertsObserved { catalog, artifacts },
    }
}

#[test]
fn system_result_recovery_is_read_only_bound_to_owner_and_verified_bytes() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path();
    let (owner, runner) = owners();
    let mut store =
        Store::create(&target.join("system-ui"), owner.clone(), runner.clone()).unwrap();
    let request = store
        .dispatch(id("observe"), &Operation::ObserveSystemAlerts)
        .unwrap();
    assert!(store.ensure_resolved().is_err());
    assert!(store.invalidate(&id("app")).is_err());
    assert!(
        store
            .dispatch(id("observe"), &Operation::ObserveSystemAlerts)
            .is_err()
    );
    let state = std::fs::read(target.join("system-ui/requests.json")).unwrap();
    drop(store);
    assert!(
        read_completion(
            target,
            &owner,
            &runner,
            &id("observe"),
            &Operation::ObserveSystemAlerts
        )
        .unwrap()
        .is_none()
    );
    let completion = observed(target, &runner, request);
    let results = target.join("session/worker/system-ui/results");
    std::fs::create_dir_all(&results).unwrap();
    save_atomic(
        &results.join("1.json"),
        &crate::ios::UiReply::Completed {
            completion: Box::new(completion.clone()),
        },
    )
    .unwrap();
    assert_eq!(
        read_completion(
            target,
            &owner,
            &runner,
            &id("observe"),
            &Operation::ObserveSystemAlerts
        )
        .unwrap(),
        Some(completion.clone())
    );
    assert_eq!(
        std::fs::read(target.join("system-ui/requests.json")).unwrap(),
        state
    );
    let mut wrong = owner.clone();
    wrong.generation = "c".repeat(48);
    assert!(
        read_completion(
            target,
            &wrong,
            &runner,
            &id("observe"),
            &Operation::ObserveSystemAlerts
        )
        .is_err()
    );
    let UiOutcome::SystemAlertsObserved { artifacts, .. } = &completion.outcome else {
        unreachable!()
    };
    let path = client_directory(target).join(&artifacts[0].path);
    std::fs::write(path, b"changed").unwrap();
    assert!(
        read_completion(
            target,
            &owner,
            &runner,
            &id("observe"),
            &Operation::ObserveSystemAlerts
        )
        .is_err()
    );
}

#[test]
fn app_actions_and_system_attempts_consume_bases_but_cached_results_do_not_restore_them() {
    let root = tempfile::tempdir().unwrap();
    let (owner, runner) = owners();
    let mut store = Store::create(&root.path().join("system-ui"), owner, runner.clone()).unwrap();
    let request = store
        .dispatch(id("observe"), &Operation::ObserveSystemAlerts)
        .unwrap();
    let completion = observed(root.path(), &runner, request);
    let UiOutcome::SystemAlertsObserved { catalog, .. } = &completion.outcome else {
        unreachable!()
    };
    let operation = Operation::TapSystemButton {
        observation: catalog.observation.clone(),
        reference: catalog.alerts[0].buttons[1].reference.clone(),
    };
    store.complete(&id("observe"), completion).unwrap();
    store.validate(&id("tap"), &operation).unwrap();
    store.invalidate(&id("app")).unwrap();
    store
        .validate(&id("observe"), &Operation::ObserveSystemAlerts)
        .unwrap();
    assert!(store.validate(&id("tap"), &operation).is_err());
    assert!(
        store
            .validate(&id("app"), &Operation::ObserveSystemAlerts)
            .is_err()
    );
    let request = store
        .dispatch(id("fresh"), &Operation::ObserveSystemAlerts)
        .unwrap();
    store
        .complete(&id("fresh"), observed(root.path(), &runner, request))
        .unwrap();
    let request = store.dispatch(id("tap"), &operation).unwrap();
    let Operation::TapSystemButton {
        reference,
        observation,
    } = operation.clone()
    else {
        unreachable!()
    };
    store
        .complete(
            &id("tap"),
            UiCompletion {
                generation: runner.generation,
                pid: runner.pid,
                last_id: request.request_id,
                request,
                receipt_sha256: "a".repeat(64),
                outcome: UiOutcome::SystemButtonNotApplied {
                    reference,
                    observation,
                    reason: ios_system_ui::NotAppliedReason::ObservationChanged,
                },
            },
        )
        .unwrap();
    assert!(store.validate(&id("repeat"), &operation).is_err());
    assert!(store.validate(&id("tap"), &operation).is_ok());
    let state: State = load(&root.path().join("system-ui/requests.json"))
        .unwrap()
        .unwrap();
    assert_eq!(state.cursor().unwrap(), (None, false));
}

#[test]
fn native_system_authority_binds_process_and_never_invents_a_flutter_generation() {
    let root = tempfile::tempdir().unwrap();
    let (_, runner) = owners();
    let owner = frontcap_core::native::ios_host::Owner {
        simulator:runner.simulator.clone(), process:frontcap_core::process::ProcessIdentity {pid:43,started:"original process".into()},
        attachment_sha256:"c".repeat(64),
    };
    let mut store = Store::create(&root.path().join("system-ui"),owner.clone(),runner.clone()).unwrap();
    let request = store.dispatch_ui(id("observe"),&UiOperation::ObserveSystemAlerts {}).unwrap();
    assert!(store.invalidate(&id("app")).is_err());
    let completion = observed(root.path(),&runner,request);
    store.complete(&id("observe"),completion.clone()).unwrap();
    let UiOutcome::SystemAlertsObserved {catalog,..} = &completion.outcome else {panic!()};
    let button = catalog.alerts.iter().flat_map(|a| &a.buttons).find(|b| b.unique && b.enabled && b.hittable).unwrap();
    let tap = UiOperation::TapSystemButton {observation:catalog.observation.clone(),reference:button.reference.clone()};
    store.validate_ui(&id("tap"),&tap).unwrap();
    // Original lookup does not revalidate or restore its consumed input basis.
    store.invalidate(&id("url")).unwrap();
    assert!(store.validate_ui(&id("tap"),&tap).is_err());
    let state = root.path().join("system-ui/requests.json");
    let before = std::fs::read(&state).unwrap();
    let client = client_directory(root.path()); let results = root.path().join("session/worker/system-ui/results");
    assert_eq!(read_at(&state,&client,&results,&owner,&runner,&id("observe"),&UiOperation::ObserveSystemAlerts {}).unwrap(),Some(completion));
    assert_eq!(std::fs::read(&state).unwrap(),before);
    let mut replacement=owner.clone();replacement.process.started="replacement".into();
    assert!(read_at(&state,&client,&results,&replacement,&runner,&id("observe"),&UiOperation::ObserveSystemAlerts {}).is_err());
    replacement.process.pid=runner.pid;
    assert!(Store::create(&root.path().join("invalid"),replacement,runner).is_err());
    let saved=String::from_utf8(before).unwrap();
    assert!(!saved.contains("source_sha256"));
}

#[test]
fn system_budget_preserves_unpublished_results_and_cached_completions() {
    use frontcap_core::{ids::LaunchId,write_budget::Limits};
    for exhaustion in [0,1,2] {
        let temp=tempfile::tempdir().unwrap();let target=temp.path();let (owner,runner)=owners();
        let budget=WriteBudget::create(&target.join("budget"),LaunchId::new("g-system").unwrap(),
            Limits {ordinary:65536,retirement:4096}).unwrap();
        let mut store=Store::create_with_budget(&target.join("system-ui"),owner.clone(),runner.clone(),Some(&budget)).unwrap();
        let path=target.join("system-ui/requests.json");
        assert_eq!(budget.charged().unwrap().ordinary,std::fs::metadata(&path).unwrap().len());
        if exhaustion==0 {
            budget.charge(Class::Ordinary,65536-budget.charged().unwrap().ordinary).unwrap();
            let before=std::fs::read(&path).unwrap();
            assert!(store.dispatch(id("observe"),&Operation::ObserveSystemAlerts).is_err());
            assert!(!store.contains(&id("observe")));store.ensure_resolved().unwrap();
            assert_eq!(std::fs::read(&path).unwrap(),before);
            continue;
        }
        let request=store.dispatch(id("observe"),&Operation::ObserveSystemAlerts).unwrap();
        let completion=observed(target,&runner,request);
        let results=target.join("session/worker/system-ui/results");std::fs::create_dir_all(&results).unwrap();
        save_atomic(&results.join("1.json"),&crate::ios::UiReply::Completed {completion:Box::new(completion.clone())}).unwrap();
        if exhaustion==2 {store.complete(&id("observe"),completion.clone()).unwrap();}
        budget.charge(Class::Ordinary,65536-budget.charged().unwrap().ordinary).unwrap();
        let before=std::fs::read(&path).unwrap();let charged=budget.charged().unwrap();
        let result=store.complete(&id("observe"),completion.clone());
        if exhaustion==1 {assert!(result.is_err());assert!(store.ensure_resolved().is_err());}
        else {result.unwrap();store.ensure_resolved().unwrap();}
        assert_eq!(read_completion(target,&owner,&runner,&id("observe"),&Operation::ObserveSystemAlerts).unwrap(),Some(completion));
        assert_eq!(std::fs::read(&path).unwrap(),before);
        assert_eq!(budget.charged().unwrap(),charged);assert_eq!(charged.retirement,0);
    }
}
