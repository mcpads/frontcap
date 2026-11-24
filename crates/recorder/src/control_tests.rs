use super::*;
use frontcap_chromium::generation::ObservationState;

fn observed() -> ObservationState {
    ObservationState { next_ref: 7, next_observation: 3, ..ObservationState::default() }
}

#[test]
fn a_saved_control_state_leaves_its_observations_to_their_own_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("control.json");
    let state = ControlState { recording: true, observations: observed(), ..ControlState::default() };
    frontcap_core::state::save_atomic(&path, &state).unwrap();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert!(saved.get("observations").is_none());
    let loaded: ControlState = frontcap_core::state::load(&path).unwrap().unwrap();
    assert_eq!(loaded, ControlState { observations: ObservationState::default(), ..state });
}

#[test]
fn a_control_state_saved_with_its_observations_still_loads_them() {
    let mut saved = serde_json::to_value(ControlState::default()).unwrap();
    saved["observations"] = serde_json::to_value(observed()).unwrap();
    let loaded: ControlState = serde_json::from_value(saved).unwrap();
    assert_eq!(loaded.observations, observed());
}
