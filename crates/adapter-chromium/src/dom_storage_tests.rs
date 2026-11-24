use super::*;
#[test]
fn context_topology_is_validated_even_without_storage_seeds() {
    use frontcap_core::profile::{DeterminismProfile, InitialContext, PopupPolicy};
    let mut profile = DeterminismProfile {
        initial_contexts: vec![InitialContext {
            opener: "page".into(), url: "https://app.test/child".into(),
            name: "child".into(), no_opener: false, closed: false,
        }],
        ..Default::default()
    };
    assert!(validate_initial_dom_storage(&profile).is_err());
    profile.popups = PopupPolicy::Controlled;
    validate_initial_dom_storage(&profile).unwrap();
    profile.initial_contexts[0].opener = "popup-1".into();
    assert!(validate_initial_dom_storage(&profile).is_err());
    profile.initial_contexts[0].opener = "page".into();
    profile.initial_contexts[0].url = "javascript:alert(1)".into();
    assert!(validate_initial_dom_storage(&profile).is_err());
}

fn area() -> storage::FrontcapDOMStorageArea {
    storage::FrontcapDOMStorageArea {
        storage_key: storage::SerializedStorageKey("https://app.test/".into()),
        origin: "https://app.test".into(), top_level_site: "https://app.test".into(),
        has_cross_site_ancestor: false, nonce_partition: false, first_party: true,
        entries: vec![storage::FrontcapDOMStorageEntry {
            key: cdp::Binary("AXg=".into()), value: cdp::Binary("AADY".into()) }],
    }
}
#[test]
fn native_surrogate_bytes_survive_conversion() {
    let decoded = decode_area(area()).unwrap();
    assert_eq!(decoded.entries[0].key, [1, b'x']);
    assert_eq!(decoded.entries[0].value, [0, 0, 0xd8]);
    assert_eq!(decoded.partition.unwrap().has_cross_site_ancestor, Some(false));
}
#[test]
fn inconsistent_or_unrestorable_native_data_is_rejected() {
    let mut native = area(); native.nonce_partition = true;
    assert!(decode_area(native).is_err());
    let mut native = area(); native.has_cross_site_ancestor = true;
    assert!(decode_area(native).is_err());
    let mut native = area(); native.entries.push(native.entries[0].clone());
    assert!(decode_area(native).is_err());
    let mut native = area(); native.entries[0].value = cdp::Binary("AADY====".into());
    assert!(decode_area(native).is_err());
}
