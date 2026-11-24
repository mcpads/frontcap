use super::*;

#[test]
fn retained_input_progress_preserves_pending_storage_uncertainty_and_native_order() {
    use frontcap_core::native::android_webview::Debugger;
    let consumer = Debugger {
        emulator: frontcap_core::process::ProcessIdentity::of(std::process::id()).unwrap().unwrap(),
        package: "dev.example.app".into(), activity: "dev.example.app.Main".into(),
        serial: "emulator-5580".into(), pid: 123, process_start_ticks: 42,
        socket: "webview_devtools_remote_123".into(), apk_sha256: "a".repeat(64), source_sha256: "b".repeat(64),
    };
    let owner = ContactDriver { short_tap: false, consumer: frontcap_core::native::android_webview::ContactConsumer::WebView(consumer), generation: "abcd0000-0000-0000-0000-000000000001".into(),
        pid: 456, process_start_ticks: 43, lifetime_ms: 30000, request_capacity: 4,
        guest_directory: "/data/local/tmp/test".into(), jar_sha256: "c".repeat(64) };
    let mut state = Session::new(owner);
    let id = |value: &str| RequestId::new(value).unwrap();
    let empty = state.inspect(false);
    assert!(empty.pending.is_none() && empty.last_completed.is_none());
    assert_eq!(empty.remaining_requests, 4);
    for (key, sequence, completed) in [("z-first", 1, true), ("a-second", 2, true), ("pending", 3, false)] {
        state.entries.insert(id(key), Entry {
            observation: id("basis"), operation: Operation::Cancel { expected_revision: sequence - 1 },
            request: json!({"request_id":sequence}), reply: completed.then(|| json!({"status":"injected","contact_revision":sequence,"contacts":[]})),
        });
    }
    state.next = 3; state.revision = 2; state.pending = Some(id("pending"));
    let original = serde_json::to_value(&state).unwrap();
    for _ in 0..2 {
        let progress = state.inspect(true);
        assert!(progress.storage_uncertain);
        assert_eq!(progress.revision, 2);
        assert_eq!(progress.remaining_requests, 1);
        assert_eq!(progress.last_completed.unwrap().id, id("a-second"));
        let pending = progress.pending.unwrap();
        assert_eq!(pending.id, id("pending"));
        assert_eq!(pending.native_request_id, 3);
        assert!(pending.reply.is_none());
    }
    assert_eq!(serde_json::to_value(&state).unwrap(), original);
    state.entries.get_mut(&id("pending")).unwrap().reply = Some(json!({"status":"injected","contact_revision":3,"contacts":[]}));
    state.pending = None; state.next = 4; state.revision = 3;
    let progress = state.inspect(false);
    assert!(!progress.storage_uncertain && progress.pending.is_none());
    assert_eq!(progress.remaining_requests, 1);
    assert_eq!(progress.last_completed.unwrap().id, id("pending"));
}

#[test]
fn short_tap_requires_both_native_events_with_one_point_and_bounded_age() {
    let point=json!({"x":600,"y":350});
    let frame=|action,at|json!({"action":action,"uptime_ms":at,"down_time_ms":1000,
        "accepted":true,"contacts":[{"id":1,"x":600,"y":350}]});
    let reply=json!({"frames":[frame(0,1001),frame(1,1020)]});
    validate_tap_frames(&point,&reply).unwrap();
    // An empty final contact set alone cannot prove dispatch, order, targeting or duration.
    let mut missing=reply.clone(); missing["frames"]=json!([]);
    let mut partial=reply.clone(); partial["frames"]=json!([frame(0,1001)]);
    let mut reversed=reply.clone(); reversed["frames"]=json!([frame(1,1001),frame(0,1020)]);
    let mut late=reply.clone(); late["frames"][1]["uptime_ms"]=json!(1101);
    let mut clock=reply.clone(); clock["frames"][1]["uptime_ms"]=json!(1000);
    let mut gesture=reply.clone(); gesture["frames"][1]["down_time_ms"]=json!(1001);
    let mut foreign=reply.clone(); foreign["frames"][1]["contacts"][0]["x"]=json!(601);
    let mut rejected=reply.clone(); rejected["frames"][1]["accepted"]=json!(false);
    for invalid in [missing,partial,reversed,late,clock,gesture,foreign,rejected] {
        assert!(validate_tap_frames(&point,&invalid).is_err(),"{invalid}");
    }
    for x in [f64::NAN,f64::INFINITY,-0.1,1.1] {
        assert!(Operation::Tap{expected_revision:0,x,y:0.5}.validate().is_err());
    }
    Operation::Tap{expected_revision:0,x:0.0,y:1.0}.validate().unwrap();
}
