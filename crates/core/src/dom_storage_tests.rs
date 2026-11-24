use super::*;

fn area(value: Vec<u8>) -> DomStorageArea {
    DomStorageArea { origin: "https://app.test".into(), partition: None,
        entries: vec![DomStorageEntry { key: vec![0, 0, 0xd8], value }] }
}

#[test]
fn serialization_preserves_raw_bytes_and_same_origin_context_separation() {
    let state = DomStorageState {
        encoding: DomStorageEncoding::ChromiumLittleEndian,
        local: vec![area(vec![0, 0, 0xdc, 0xff])],
        sessions: vec![
            SessionStorageState { context: "page".into(), areas: vec![area(vec![0, 0xd8])] },
            SessionStorageState { context: "popup-1".into(), areas: vec![area(vec![0, 0xdc])] },
            SessionStorageState { context: "popup-2".into(), areas: vec![] },
        ],
    };
    state.validate().unwrap();
    let restored: DomStorageState = serde_json::from_slice(&serde_json::to_vec(&state).unwrap()).unwrap();
    assert_eq!(restored, state);
    assert_ne!(restored.sessions[0].areas, restored.sessions[1].areas);
    assert!(restored.sessions[2].areas.is_empty());
}

#[test]
fn ambiguous_ownership_and_overwrite_inputs_are_rejected() {
    let mut state = DomStorageState { encoding: DomStorageEncoding::ChromiumLittleEndian,
        local: vec![area(vec![1])], sessions: vec![] };
    state.local[0].entries.push(DomStorageEntry { key: vec![0, 0, 0xd8], value: vec![2] });
    assert!(state.validate().is_err());
    state.local = vec![area(vec![1]), area(vec![2])];
    assert!(state.validate().is_err());
    state.local.clear();
    state.sessions = vec![SessionStorageState { context: "page".into(), areas: vec![] }; 2];
    assert!(state.validate().is_err());
}

#[test]
fn unspecified_native_encoding_is_not_silently_accepted() {
    assert!(serde_json::from_str::<DomStorageState>(r#"{"local":[],"sessions":[]}"#).is_err());
    assert!(serde_json::from_str::<DomStorageState>(r#"{"encoding":"unknown","local":[],"sessions":[]}"#).is_err());
}
