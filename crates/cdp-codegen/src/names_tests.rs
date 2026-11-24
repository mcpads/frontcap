use super::*;

#[test]
fn module_names() {
    assert_eq!(module_name("Emulation"), "emulation");
    assert_eq!(module_name("DOM"), "dom");
    assert_eq!(module_name("DOMSnapshot"), "dom_snapshot");
    assert_eq!(module_name("IndexedDB"), "indexed_db");
    assert_eq!(module_name("CSS"), "css");
}

#[test]
fn field_names() {
    assert_eq!(field_ident("virtualTimeTicksBase").to_string(), "virtual_time_ticks_base");
    assert_eq!(field_ident("type").to_string(), "r#type");
    assert_eq!(field_ident("self").to_string(), "self_");
}

#[test]
fn enum_variant_edge_cases() {
    let values = vec!["pauseIfNetworkFetchesPending".to_string(), "-webkit-box".to_string(),
                      "2d".to_string(), "Foo".to_string(), "foo".to_string()];
    let names: Vec<String> = enum_variants(&values).into_iter().map(|(ident, _)| ident.to_string()).collect();
    assert_eq!(names, vec!["PauseIfNetworkFetchesPending", "WebkitBox", "V2d", "Foo", "Foo2"]);
}

#[test]
fn command_and_event_names() {
    assert_eq!(params_name("setVirtualTimePolicy"), "SetVirtualTimePolicyParams");
    assert_eq!(returns_name("setVirtualTimePolicy"), "SetVirtualTimePolicyReturns");
    assert_eq!(event_name("virtualTimeBudgetExpired"), "VirtualTimeBudgetExpiredEvent");
}

#[test]
fn enum_variant_named_self_is_suffixed() {
    let names: Vec<String> =
        enum_variants(&["self".to_string()]).into_iter().map(|(ident, _)| ident.to_string()).collect();
    assert_eq!(names, vec!["Self_"]);
}
