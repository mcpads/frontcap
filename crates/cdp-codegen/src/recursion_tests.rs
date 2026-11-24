use super::*;
use crate::model::Protocol;

fn protocol(json: &str) -> Protocol {
    serde_json::from_str(json).unwrap()
}

#[test]
fn recursive_field_is_boxed() {
    let p = protocol(r#"{"domains":[{"domain":"Runtime","types":[
        {"id":"StackTrace","type":"object","properties":[
            {"name":"description","type":"string","optional":true},
            {"name":"parent","$ref":"StackTrace","optional":true}]}]}]}"#);
    let boxed = boxed_fields(&p);
    assert!(boxed.contains(&("Runtime".to_string(), "StackTrace".to_string(), "parent".to_string())));
}

#[test]
fn vec_edge_does_not_box() {
    let p = protocol(r#"{"domains":[{"domain":"DOM","types":[
        {"id":"Node","type":"object","properties":[
            {"name":"children","type":"array","items":{"$ref":"Node"},"optional":true}]}]}]}"#);
    assert!(boxed_fields(&p).is_empty());
}

#[test]
fn mutual_recursion_across_domains_is_boxed() {
    let p = protocol(r#"{"domains":[
        {"domain":"A","types":[{"id":"X","type":"object","properties":[{"name":"y","$ref":"B.Y","optional":true}]}]},
        {"domain":"B","types":[{"id":"Y","type":"object","properties":[{"name":"x","$ref":"A.X","optional":true}]}]}]}"#);
    let boxed = boxed_fields(&p);
    assert!(boxed.contains(&("A".into(), "X".into(), "y".into())));
    assert!(boxed.contains(&("B".into(), "Y".into(), "x".into())));
}
