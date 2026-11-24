use super::*;

#[test]
fn system_receipts_bind_the_runner_without_fabricating_an_app_clock() {
    use super::super::ios_ui::{UiCompletion, UiIdentity, UiOperation, UiOutcome, UiRequest};
    use super::super::observation::Artifact;
    let runner = UiIdentity { simulator: "abcd0000-0000-0000-0000-000000000001".into(),
        generation: "abcd0000-0000-0000-0000-000000000002".into(), pid: 123 };
    let catalog: Catalog = serde_json::from_str(include_str!("../../tests/fixtures/ios-system-ui.json")).unwrap();
    let request = UiRequest { generation: runner.generation.clone(), request_id: 1,
        operation: UiOperation::ObserveSystemAlerts {} };
    let artifacts = ["screen.png", "hierarchy.txt"].iter().map(|name| Artifact {
        path: format!("observations/{}/{name}", catalog.observation), bytes: 42, sha256: "a".repeat(64),
    }).collect();
    let mut receipt = UiCompletion { request: request.clone(), generation: runner.generation.clone(),
        pid: runner.pid, last_id: 1, outcome: UiOutcome::SystemAlertsObserved { catalog, artifacts }, receipt_sha256: "b".repeat(64) };
    receipt.validate_system(&runner, &request).unwrap();
    // An app-bound UIKit validation still requires its actual acquired app.
    assert!(receipt.validate(&runner, &request, None).is_err());
    receipt.pid += 1;
    assert!(receipt.validate_system(&runner, &request).is_err());
    receipt.pid = runner.pid;
    receipt.receipt_sha256 = "bad".into();
    assert!(receipt.validate_system(&runner, &request).is_err());
    receipt.receipt_sha256 = "b".repeat(64);
    let app_request = UiRequest { operation: UiOperation::ObserveTargets {}, ..request };
    assert!(receipt.validate_system(&runner, &app_request).is_err());
}

#[test]
fn system_buttons_require_bounded_stable_unique_observations() {
    let catalog: Catalog =
        serde_json::from_str(include_str!("../../tests/fixtures/ios-system-ui.json")).unwrap();
    let observation = &catalog.observation;
    let reference = &catalog.alerts[0].buttons[1].reference;
    assert_eq!(
        catalog.target(observation, reference).unwrap().label,
        "Open"
    );
    for change in [
        "unstable",
        "disabled",
        "unhittable",
        "wrong-owner",
        "oversized",
        "duplicate",
    ] {
        let mut changed = catalog.clone();
        match change {
            "unstable" => changed.stable = false,
            "disabled" => {
                changed.alerts[0].buttons[1].enabled = false;
                changed.alerts[0].buttons[1].hittable = false;
            }
            "unhittable" => changed.alerts[0].buttons[1].hittable = false,
            "wrong-owner" => changed.bundle = "dev.example.app".into(),
            "oversized" => changed.alerts[0].texts.push("a".repeat(1025)),
            _ => {
                changed.alerts[0].buttons[0] = changed.alerts[0].buttons[1].clone();
                changed.alerts[0].buttons[0].reference = format!("{observation}-0");
                for button in &mut changed.alerts[0].buttons {
                    button.unique = false;
                    button.hittable = false;
                }
            }
        }
        assert!(changed.target(observation, reference).is_err(), "{change}");
    }
    assert!(
        catalog
            .target("abcd0000-0000-0000-0000-000000000009", reference)
            .is_err()
    );
    let mut overlapping = catalog.clone();
    overlapping.alerts.push(overlapping.alerts[0].clone());
    for (i, button) in overlapping
        .alerts
        .iter_mut()
        .flat_map(|a| &mut a.buttons)
        .enumerate()
    {
        button.reference = format!("{observation}-{i}");
        button.unique = false;
        button.hittable = false;
    }
    overlapping.validate().unwrap();
    assert!(overlapping.target(observation, reference).is_err());
    let mut absent = catalog;
    absent.alerts.clear();
    absent.validate().unwrap();
    assert!(
        absent
            .target(&absent.observation, &format!("{}-0", absent.observation))
            .is_err()
    );
}
