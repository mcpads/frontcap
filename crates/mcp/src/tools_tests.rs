use super::*;

#[test]
fn tool_names_are_the_implemented_surface() {
    let names: Vec<String> = descriptions().into_iter().map(|d| d.name).collect();
    assert_eq!(names, ["native", "bootstrap", "launch", "status", "act", "wait", "observe", "capture", "stop", "network", "watch", "session", "state", "input", "debug"]);
}

#[test]
fn every_tool_has_an_object_schema() {
    for d in descriptions() {
        let v = serde_json::to_value(&d.input_schema).unwrap();
        assert_eq!(v["type"], "object", "{}: {v}", d.name);
        assert!(!d.description.is_empty());
    }
}

#[test]
fn malformed_arguments_are_a_tool_error() {
    let bad = read_arguments::<WaitArgs>(r#"{"launch_id":"g-1","op":{"type":"warp"}}"#).unwrap_err();
    let result = invalid_arguments("wait", &bad);
    assert!(result.is_error);
    let v: serde_json::Value = serde_json::from_str(result.structured_content.get()).unwrap();
    assert_eq!(v["code"], "invalid_arguments");
    assert!(v["message"].as_str().unwrap().contains("warp"), "{v}");
    assert!(!v["recovery"].as_str().unwrap().is_empty());
    assert!(serde_json::from_str::<WaitArgs>(r#"{"launch_id":"g-1","op":{"type":"now"},"extra":1}"#).is_err(), "unknown fields are refused");
}

#[test]
fn wait_now_needs_no_lease_or_request_id() {
    let a: WaitArgs = serde_json::from_str(r#"{"launch_id":"g-1","op":{"type":"now"}}"#).unwrap();
    assert!(a.lease.is_none() && a.request_id.is_none());
    assert!(matches!(a.op, WaitOp::Now {}));
}

#[test]
fn act_leaves_its_settle_to_the_server_by_default() {
    let a: ActArgs = serde_json::from_str(
        r##"{"launch_id":"g-1","lease":"0123456789abcdef0123456789abcdef","request_id":"a-1","action":{"type":"click","target":{"by":"css","selector":"#go"}}}"##,
    )
    .unwrap();
    assert!(a.then.is_none(), "omitted: the server settles by default");
}

#[test]
fn launch_defaults_to_time_on_the_local_host() {
    let a: LaunchArgs = serde_json::from_str(r#"{"request_id":"l-1","url":"http://127.0.0.1/"}"#).unwrap();
    assert_eq!(a.determinism, Determinism::Time);
    assert_eq!(a.host, Host::Local);
    assert!(a.rules.rules.is_empty());
}

#[test]
fn every_act_action_parses_from_its_documented_shape() {
    let t = r##"{"by":"css","selector":"#q"}"##;
    for action in [
        format!(r#"{{"type":"click","target":{t}}}"#),
        format!(r#"{{"type":"dblclick","target":{t}}}"#),
        format!(r#"{{"type":"hover","target":{t}}}"#),
        format!(r#"{{"type":"type","target":{t},"text":"hi"}}"#),
        r#"{"type":"press","key":"Enter"}"#.to_string(),
        format!(r#"{{"type":"press","target":{t},"key":"Shift+Tab"}}"#),
        format!(r#"{{"type":"fill","target":{t},"value":""}}"#),
        format!(r#"{{"type":"select","target":{t},"values":["a"]}}"#),
        r#"{"type":"scroll","delta_x":0,"delta_y":300}"#.to_string(),
        r#"{"type":"navigate","url":"http://127.0.0.1/a"}"#.to_string(),
        r#"{"type":"back"}"#.to_string(),
        r#"{"type":"reload"}"#.to_string(),
        format!(r#"{{"type":"wait_actionable","target":{t},"max_ms":500}}"#),
    ] {
        serde_json::from_str::<ActAction>(&action).unwrap_or_else(|e| panic!("{action}: {e}"));
    }
    assert!(serde_json::from_str::<ActAction>(r#"{"type":"drag"}"#).is_err(), "drag requires a target");
}

#[test]
fn drawers_list_a_compact_schema_with_their_operations() {
    for d in descriptions().into_iter().filter(|d| ["network", "watch", "session", "state", "input", "debug"].contains(&d.name.as_str())) {
        let v = serde_json::to_value(&d).unwrap();
        assert!(v.to_string().len() <= 1024, "{}: {} bytes", d.name, v.to_string().len());
        let ops: Vec<String> = serde_json::from_value(v["inputSchema"]["properties"]["operation"]["enum"].clone()).unwrap();
        assert!(ops.contains(&"describe".to_string()), "{}: {ops:?}", d.name);
    }
}

/// A field of the wrong shape is named, with the operation whose fields describe shows.
#[test]
fn an_argument_error_names_its_field_and_where_its_shape_is_described() {
    let Err(result) = drawer::<StateOp>("state", r##"{"operation":"read","launch_id":"g-1","target":{"by":"document"},"field":"storage"}"##) else { panic!("refused") };
    let v: serde_json::Value = serde_json::from_str(result.structured_content.get()).unwrap();
    assert_eq!(v["message"], "state: field: invalid type: string \"storage\", expected an object with its \"type\"", "{v}");
    assert_eq!(v["recovery"], "call state with operation describe and of \"read\" for its fields");
    let bad = read_arguments::<WaitArgs>(r#"{"launch_id":"g-1","op":{"type":"advance","by_ms":"soon"}}"#).unwrap_err();
    let v: serde_json::Value = serde_json::from_str(invalid_arguments("wait", &bad).structured_content.get()).unwrap();
    assert!(v["message"].as_str().unwrap().starts_with("wait: op: invalid type: string \"soon\""), "{v}");
}
