use super::*;

#[test]
fn pointer_lifetime_ids_are_acquisition_but_position_and_target_remain_behavior() {
    let event = |frame: &str, x: i32, target: &str| serde_json::json!({
        "seq":0,"t_us":250000,"kind":"action","payload":{"action":"pointer_begin","target":target,
        "pointer":{"request_id":"begin","document_id":1,"point":[x,40],"button":"left",
        "native":{"frame_id":frame,"loader_id":format!("loader-{frame}")}}}
    }).to_string();
    let first=event("one",40,"child");
    let second=event("two",40,"child");
    assert_eq!(behavior_line(&first).unwrap(),behavior_line(&second).unwrap());
    assert_eq!(compare_behavior(&first,&second),Ok(()));
    assert_eq!(compare_replay(&first,&second,&[],&[]),Ok(()));
    assert!(compare_behavior(&first,&event("two",41,"child")).is_err());
    assert!(compare_replay(&first,&event("two",40,"other"),&[],&[]).is_err());
}

#[test]
fn replay_event_stops_ignore_controller_shifted_cursors_but_keep_response_identity() {
    let stopped = |seq: u64, ordinal: &str, t: u64| serde_json::json!({
        "seq":seq+1,"t_us":t,"kind":"clock","payload":{"op":"next","phase":"end",
        "outcome":{"type":"event","event":"response","triggers":[{"kind":"response","seq":seq,"ordinal":ordinal,"t_us":t}]}}
    }).to_string();
    let source = stopped(25, "req-4", 1200000);
    assert_eq!(compare_replay(&source, &stopped(22, "req-4", 1200000), &[], &[]), Ok(()));
    assert!(compare_replay(&source, &stopped(22, "req-5", 1200000), &[], &[]).is_err());
    assert!(compare_replay(&source, &stopped(22, "req-4", 1201000), &[], &[]).is_err());
    assert!(compare_behavior(&source, &stopped(22, "req-4", 1200000)).is_err());
    // Every trigger kind, a popup's included.
    let opened = |seq: u64, context: &str| serde_json::json!({
        "seq":seq+1,"t_us":500000,"kind":"clock","payload":{"op":"next","phase":"end",
        "outcome":{"type":"event","event":"popup","triggers":[{"kind":"popup","seq":seq,"t_us":500000,"context":context,"opener":"page"}]}}
    }).to_string();
    assert_eq!(compare_replay(&opened(30, "popup-1"), &opened(27, "popup-1"), &[], &[]), Ok(()));
    assert!(compare_replay(&opened(30, "popup-1"), &opened(27, "popup-2"), &[], &[]).is_err());
}

const A: &str = "{\"kind\":\"clock\",\"payload\":{\"op\":\"settle\",\"phase\":\"end\",\"wall_ms\":12},\"seq\":0,\"t_us\":0}\n\
{\"kind\":\"watch\",\"payload\":{\"name\":\"t\",\"point\":\"mutation\",\"value\":\"true\"},\"seq\":1,\"t_us\":3500000}\n";

#[test]
fn removes_wall_time_only() {
    let line = A.lines().next().unwrap();
    assert_eq!(behavior_line(line).unwrap(), "{\"kind\":\"clock\",\"payload\":{\"op\":\"settle\",\"phase\":\"end\"},\"seq\":0,\"t_us\":0}");
}

#[test]
fn runs_differing_only_in_wall_time_compare_equal() {
    let b = A.replace("\"wall_ms\":12", "\"wall_ms\":907");
    assert_eq!(compare_behavior(A, &b), Ok(()));
}

#[test]
fn a_different_virtual_time_is_a_difference() {
    let b = A.replace("3500000", "3500001");
    let d = compare_behavior(A, &b).unwrap_err();
    assert_eq!(d.index, 1);
}

#[test]
fn a_missing_event_is_a_difference() {
    let b: String = A.lines().take(1).map(|l| format!("{l}\n")).collect();
    let d = compare_behavior(A, &b).unwrap_err();
    assert_eq!(d.index, 1);
    assert!(d.right.is_none());
}

#[test]
fn replay_ignores_controller_events_and_their_seq() {
    let controller = "{\"kind\":\"controller\",\"payload\":{\"change\":\"attached\"},\"seq\":1,\"t_us\":0}";
    let mut lines: Vec<&str> = A.lines().collect();
    lines.insert(1, controller);
    let shifted = lines.join("\n").replace("\"seq\":1,\"t_us\":3500000", "\"seq\":2,\"t_us\":3500000");
    assert_eq!(compare_replay(A, &shifted, &[], &[]), Ok(()));
    assert!(compare_behavior(A, &shifted).is_err());
    let moved = A.replace("3500000", "3600000");
    assert_eq!(compare_replay(A, &moved, &[], &[]).unwrap_err().index, 1);
}

#[test]
fn replay_rewrites_the_recorded_origin() {
    let at = |port: u16| format!("{{\"kind\":\"navigation\",\"payload\":{{\"url\":\"http://127.0.0.1:{port}/a.html\"}},\"seq\":0,\"t_us\":0}}\n");
    assert!(compare_replay(&at(1), &at(2), &[], &[]).is_err());
    assert_eq!(compare_replay(&at(1), &at(2), &[("http://127.0.0.1:1".into(), "http://127.0.0.1:2".into())], &[]), Ok(()));
}

#[test]
fn rewriting_an_origin_never_touches_a_longer_one() {
    let r = [("http://127.0.0.1:1".to_string(), "http://127.0.0.1:7".to_string())];
    assert_eq!(rewrite(r#"{"a":"http://127.0.0.1:1","b":"http://127.0.0.1:1/x","c":"http://127.0.0.1:10/x"}"#, &r), r#"{"a":"http://127.0.0.1:7","b":"http://127.0.0.1:7/x","c":"http://127.0.0.1:10/x"}"#);
}

#[test]
fn request_ids_and_server_made_names_compare_by_first_appearance() {
    // Two runs of the same commands, one with ids the server generated, one with the agent's.
    let a = r#"{"seq":1,"t_us":0,"kind":"action","payload":{"request_id":"mcp-1a2b","action":"click"}}
{"seq":2,"t_us":0,"kind":"watch_added","payload":{"name":"mcp-wait-9f9f","request_id":"mcp-wait-add-1a2b"}}
{"seq":3,"t_us":5,"kind":"action","payload":{"request_id":"mcp-1a2b","action":"type"}}"#;
    let b = r#"{"seq":1,"t_us":0,"kind":"action","payload":{"request_id":"a-1","action":"click"}}
{"seq":2,"t_us":0,"kind":"watch_added","payload":{"name":"mcp-wait-0000","request_id":"mcp-wait-add-a-1"}}
{"seq":3,"t_us":5,"kind":"action","payload":{"request_id":"a-1","action":"type"}}"#;
    assert_eq!(compare_behavior(a, b), Ok(()));
    // The same id reused where the other run used a new one differs.
    let c = b.replacen(r#""request_id":"a-1","action":"type""#, r#""request_id":"a-2","action":"type""#, 1);
    assert_eq!(compare_behavior(a, &c).unwrap_err().index, 2);
    // Agents' watch names are kept.
    let d = b.replace("mcp-wait-0000", "toast");
    assert_eq!(compare_behavior(a, &d).unwrap_err().index, 1);
    // Text a page or an agent wrote is compared as it is, whatever it looks like.
    let typed = |t: &str| format!(r#"{{"seq":1,"t_us":0,"kind":"console","payload":{{"text":"{t}"}}}}"#);
    assert_eq!(compare_behavior(&typed("mcp-alpha"), &typed("mcp-beta")).unwrap_err().index, 0);
}

#[test]
fn native_dialog_projection_preserves_logical_identity_answers_and_resolution_source() {
    let event = |id: &str, source: &str, answer: &str| serde_json::json!({
        "seq":3,"t_us":1000,"kind":"dialog","payload":{"kind":"confirm","message":"question",
        "answer":answer,"native":{"id":id,"source":source}}
    }).to_string();
    let a = event("dialog-1", "answered", "true");
    assert_eq!(compare_replay(&a, &a, &[], &[]), Ok(()));
    for changed in [event("dialog-2", "answered", "true"), event("dialog-1", "external", "true"), event("dialog-1", "answered", "false")] {
        assert!(compare_behavior(&a, &changed).is_err());
        assert!(compare_replay(&a, &changed, &[], &[]).is_err());
    }
}


#[test]
fn exception_comparison_ignores_native_identity_but_preserves_occurrences() {
    let event = |context: &str, id: u64| serde_json::json!({
        "seq":1,"t_us":250000,"kind":"exception",
        "payload":{"text":"Uncaught Error: same", "native":{"context":context,"id":id}, "receipt":{"id":context,"frame":context}}
    }).to_string();
    let a = event("original-context", 1);
    let b = event("replay-context", 4);
    assert_eq!(compare_behavior(&a, &b), Ok(()));
    assert_eq!(compare_replay(&a, &b, &[], &[]), Ok(()));
    assert!(compare_replay(&format!("{a}\n{a}"), &b, &[], &[]).is_err());
    assert!(compare_behavior(&a, &b.replace("Error: same", "Error: different")).is_err());
    let nested = |event: &str| format!("{{\"kind\":\"intervention\",\"payload\":{event}}}");
    assert!(compare_behavior(&nested(&a), &nested(&b)).is_err());
}

#[test]
fn replay_record_normalization_follows_selected_installation_and_request() {
    let request = |id: &str, revision: u64| serde_json::json!({"kind":"request","payload":{
        "id":id,"rule":"api","rules_revision":revision}});
    let response = |id: &str, source: &str| serde_json::json!({"kind":"response","payload":{
        "id":id,"rule":"api","source":source}});
    let trace = |old: &str, new: &str| [request("old",0), request("new",1),
        response("new",new), response("old",old)].map(|v|v.to_string()).join("\n");
    let recorded = [(1, "api".to_string())];
    let source = trace("mock","passthrough");
    assert_eq!(compare_replay(&source, &trace("mock","replay"), &[], &recorded), Ok(()));
    assert!(compare_replay(&source, &trace("replay","replay"), &[], &recorded).is_err(),
        "an older request using the same rule id must retain its source");
    let wrong_revision = trace("mock","replay").replace("\"rules_revision\":1", "\"rules_revision\":2");
    assert!(compare_replay(&source, &wrong_revision, &[], &recorded).is_err());
}

#[test]
fn stream_receipt_identity_does_not_hide_call_or_payload_differences() {
    let event=|identity:&str|serde_json::json!({"seq":2,"t_us":12345,"kind":"stream",
        "payload":{"stream":"req-3","direction":"out","event":"message","data":"hello",
        "receipt":{"id":identity,"frame":identity,"document":identity}}}).to_string();
    let source=event("source");let replay=event("replay");
    assert_eq!(compare_behavior(&source,&replay),Ok(()));
    assert_eq!(compare_replay(&source,&replay,&[],&[]),Ok(()));
    for changed in [replay.replace("req-3","req-4"),replay.replace("hello","different"),
        replay.replace("12345","12346"),replay.replace("message","close"),replay.replace("out","in"),
        format!("{replay}\n{replay}")] {
        assert!(compare_behavior(&source,&changed).is_err());
        assert!(compare_replay(&source,&changed,&[],&[]).is_err());
    }
    let parsed:crate::event::BehaviorEvent=serde_json::from_str(&source).unwrap();
    let serialized=serde_json::to_value(parsed).unwrap();
    assert_eq!(serialized["payload"]["receipt"]["id"],"source");
    let mut legacy:serde_json::Value=serde_json::from_str(&source).unwrap();
    legacy["payload"].as_object_mut().unwrap().remove("receipt");
    let parsed:crate::event::BehaviorEvent=serde_json::from_value(legacy.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(),legacy);
}

#[test]
fn a_running_replay_matches_the_beginning_of_its_recording() {
    let console = |seq: u64, text: &str| serde_json::json!({"seq":seq,"t_us":seq*1000,"kind":"console","payload":{"level":"log","text":text}}).to_string();
    let recorded = [console(0, "a"), console(1, "b"), console(2, "after the prefix")].join("\n");
    let running = [console(0, "a"), console(1, "b")].join("\n");
    assert_eq!(compare_replay_prefix(&recorded, &running, &[], &[]), Ok(()));
    assert!(compare_replay(&recorded, &running, &[], &[]).is_err(), "a whole-run comparison still needs the tail");
    let wrong = [console(0, "a"), console(1, "c")].join("\n");
    assert_eq!(compare_replay_prefix(&recorded, &wrong, &[], &[]).unwrap_err().index, 1);
    let longer = [console(0, "a"), console(1, "b"), console(2, "after the prefix"), console(3, "extra")].join("\n");
    assert_eq!(compare_replay_prefix(&recorded, &longer, &[], &[]).unwrap_err().index, 3);
}

/// A pass-through document's body ends when the network delivers it: its response and taint fall before
/// or after the worker load its script started at the same instant, as wall time decides (MCP `progress`
/// worker interception replay, 2026-10-19). What the page did keeps its order.
#[test]
fn a_pass_through_completion_has_no_place_among_its_instants_page_records() {
    let line = |t: u64, kind: &str, payload: serde_json::Value| serde_json::json!({"seq":0,"t_us":t,"kind":kind,"payload":payload}).to_string();
    let request = |t, id: &str| line(t, "request", serde_json::json!({"id":id,"method":"GET","url":format!("http://app/{id}"),"rule":"default-passthrough-0"}));
    let response = |t, id: &str| line(t, "response", serde_json::json!({"id":id,"rule":"default-passthrough-0","source":"passthrough","status":200}));
    let taint = |t, id: &str| line(t, "taint", serde_json::json!({"source":"passthrough_content","detail":id}));
    let log = |lines: &[String]| lines.join("\n");
    let recorded = log(&[request(0, "req-1"), response(0, "req-1"), taint(0, "req-1"), request(0, "req-2"), request(0, "req-3")]);
    let replayed = log(&[request(0, "req-1"), request(0, "req-2"), response(0, "req-1"), taint(0, "req-1"), request(0, "req-3")]);
    assert_eq!(compare_replay(&recorded, &replayed, &[], &[]), Ok(()));
    // The page's own order still counts, and so does the instant.
    let swapped = log(&[request(0, "req-1"), request(0, "req-3"), response(0, "req-1"), taint(0, "req-1"), request(0, "req-2")]);
    assert!(compare_replay(&recorded, &swapped, &[], &[]).is_err());
    let later = log(&[request(0, "req-1"), request(0, "req-2"), request(0, "req-3"), response(1000, "req-1"), taint(1000, "req-1")]);
    assert!(compare_replay(&recorded, &later, &[], &[]).is_err());
    // A running replay's last instant may be unfinished.
    let running = log(&[request(0, "req-1"), request(0, "req-2"), response(0, "req-1"), taint(0, "req-1")]);
    assert_eq!(compare_replay_prefix(&recorded, &running, &[], &[]), Ok(()));
    let foreign = log(&[request(0, "req-1"), request(0, "req-2"), response(0, "req-9")]);
    assert!(compare_replay_prefix(&recorded, &foreign, &[], &[]).is_err());
    let out_of_order = log(&[request(0, "req-1"), request(0, "req-3")]);
    assert!(compare_replay_prefix(&recorded, &out_of_order, &[], &[]).is_err());
}

/// A message from a frame awaiting storage replies varied its place among an instant's records between
/// runs (MCP `indexed_db` partition replay, 2026-10-19): its place in the instant, and the receipt
/// numbers it shifts, are not compared.
#[test]
fn a_message_between_windows_has_no_place_among_its_instants_reports() {
    let watch = |rseq: u64| serde_json::json!({"seq":0,"t_us":8000,"kind":"watch","payload":{"name":"w","value":"unknown","point":"mutation","rseq":rseq,"document":5}}).to_string();
    let message = |rseq: u64| serde_json::json!({"seq":0,"t_us":8000,"kind":"message","payload":{"origin":"https://child.test","data":"\"ready\"","document":5,"rseq":rseq}}).to_string();
    let recorded = [watch(5), message(6)].join("\n");
    let replayed = [message(5), watch(6)].join("\n");
    assert_eq!(compare_replay(&recorded, &replayed, &[], &[]), Ok(()));
    let other = serde_json::json!({"seq":0,"t_us":9000,"kind":"message","payload":{"origin":"https://child.test","data":"\"ready\"","document":5,"rseq":5}}).to_string();
    assert!(compare_replay(&recorded, &[other, watch(6)].join("\n"), &[], &[]).is_err(), "another instant still differs");
}
