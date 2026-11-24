use super::*;
use serde_json::json;
#[test]
fn differences_preserve_unknowns_escape_paths_and_bound_large_values() {
    let a = json!({"nullable":null,"same":1,"a/b~":false,"events":[1]});
    let b = json!({"same":1,"a/b~":true,"events":[1,2],"large":"x".repeat(3000)});
    let mut out = vec![];
    let mut omitted = 0;
    changes("", Some(&a), Some(&b), &mut out, &mut omitted);
    assert_eq!(
        out.iter().map(|d| d.path.as_str()).collect::<Vec<_>>(),
        vec!["/a~1b~0", "/events", "/large", "/nullable"]
    );
    assert_eq!(out[1].after.value, Some(json!([1, 2])));
    assert!(out[2].after.present && out[2].after.truncated && out[2].after.value.is_none());
    assert!(out[3].before.present && !out[3].after.present);
    assert_eq!(out[3].before.value, Some(Value::Null));
    let a = json!({});
    let b: Value = (0..40).map(|i| (format!("k{i}"), json!(i))).collect();
    out.clear();
    changes("", Some(&a), Some(&b), &mut out, &mut omitted);
    assert_eq!((out.len(), omitted), (32, 8));
}

#[test]
fn android_comparison_uses_completed_observations_and_historical_captures_without_writes() {
    use frontcap_core::{native::android_webview::{Completion as A, Debugger, Snapshot}, process::ProcessIdentity};
    use frontcap_recorder_protocol::native::Ready;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("journal.jsonl");
    let mut journal = Journal::open(&path).unwrap();
    let id = |value: &str| RequestId::new(value).unwrap();
    let ready = Ready::AndroidWebview { contacts: false, owner: Debugger {
        emulator: ProcessIdentity::of(std::process::id()).unwrap().unwrap(),
        package: "dev.example.app".into(), activity: "dev.example.app.Main".into(),
        serial: "emulator-5580".into(), pid: 123, process_start_ticks: 42,
        socket: "webview_devtools_remote_123".into(), apk_sha256: "a".repeat(64), source_sha256: "b".repeat(64),
    }};
    let before = Snapshot { starting_randomness: None, canvas_kit: None, media: None, connection_console: None, page_events: None, timer_activity: None, frame_activity: None, navigation: None, href: "http://localhost/first".into(), ready_state: "complete".into(),
        visibility: "visible".into(), focused: true, performance_ms: 10.0, date_ms: 1000.0 };
    let navigation = serde_json::json!({"document":"abcd0000-0000-0000-0000-000000000001",
        "current":{"href":"http://localhost/second","hrefTruncated":false,"focused":false},
        "historyObserved":true,"failure":null,"received":1,"dropped":0,"events":[
            {"sequence":1,"elapsedMs":10.0,"kind":"push_state","href":"http://localhost/second",
             "hrefTruncated":false,"focused":false,"trusted":null,"persisted":null}]});
    let timer_activity = serde_json::json!({"document":"abcd0000-0000-0000-0000-000000000001","sampledMs":42.0,
                "methodsObserved":true,"failure":null,"registered":2,"invoked":1,"canceled":0,
                "executing":0,"untrackedRegistrations":1,"uncertainCancellations":0,
                "tracked":1,"omitted":0,"timers":[{"id":4,"registration":2,"kind":"interval",
                    "registeredMs":20.0,"requestedDelayMs":10.0,"calls":1}]});
    let frame_activity = json!({"document":"abcd0000-0000-0000-0000-000000000001",
        "pending":2,"dispatching":false,"lastTimestamp":0.0,"delivered":1});
    let after = Snapshot { frame_activity: Some(serde_json::from_value(frame_activity.clone()).unwrap()), timer_activity: Some(serde_json::from_value(timer_activity.clone()).unwrap()), href: "http://localhost/second".into(), focused: false,
        navigation: Some(serde_json::from_value(navigation.clone()).unwrap()), ..before.clone() };
    let pointers = frontcap_core::native::android_pointers::Snapshot {
        document: "abcd0000-0000-0000-0000-000000000001".into(), received: 0, dropped: 0,
        active: 0, maximum_active: 0, events: vec![],
    };
    let capture = A::Observed { snapshot: after, screen: root.path().join("historical.png"),
        bytes: 12, sha256: "c".repeat(64), pointers, basis: None };
    let rows = [
        ("before", "before", Outcome::AndroidWebviewCompleted { completion: A::Read { snapshot: before } }),
        ("after", "after", Outcome::AndroidWebviewCompleted { completion: capture.clone() }),
        ("wrong-id", "another", Outcome::AndroidWebviewCompleted { completion: capture }),
        ("uncertain", "uncertain", Outcome::AndroidWebviewUnresolved { error: "unknown".into(), lifecycle: None }),
        ("other-host", "other-host", Outcome::WebviewUnresolved { error: "unknown".into() }),
    ];
    for (key, reply_id, outcome) in rows {
        journal.begin(&id(key), "arguments").unwrap();
        journal.complete(&id(key), serde_json::to_value(Answer::Ok(Reply::Native {
            body: NativeReply::Done { request_id: id(reply_id), outcome: Box::new(outcome) },
        })).unwrap()).unwrap();
    }
    use frontcap_core::native::{android_progress::Progress, android_timers, webview_progress::{Basis, Service}};
    let Ready::AndroidWebview { owner, .. } = &ready else { panic!() };
    let mut progress = Progress {
        owner: owner.clone(), page_id: "page".into(), basis: Basis::RetainedReceipts,
        connection_closed: true, connection_navigation: None, frames_need_reconciliation: false, timers_need_pause: false,
        frames: None, contacts: None, lifecycle: None, service: Service::NotInspected, limitations: vec![],
        timers: Some(android_timers::State::Pending { document: "document".into(), request: id("timer"),
            action: android_timers::Action::Advance { milliseconds: 200 } }),
    };
    let mut historical = serde_json::to_value(&progress).unwrap();
    historical.as_object_mut().unwrap().remove("lifecycle");
    assert!(serde_json::from_value::<Progress>(historical).unwrap().lifecycle.is_none());
    for key in ["progress-pending", "progress-paused"] {
        if key == "progress-paused" {
            progress.lifecycle = Some(frontcap_core::native::android_progress::Lifecycle {
                uncertain: true, last_completed: None,
                pending: Some(frontcap_core::native::android_progress::Transition {
                    id: id("host-home"), operation: frontcap_core::native::android_webview::WebViewLifecycle::Home,
                    owner: owner.clone(), receipt: None,
                }),
            });
            progress.timers = Some(android_timers::State::Paused { completion: android_timers::Completion {
                document: "document".into(), action: android_timers::Action::Advance { milliseconds: 200 },
                ticks_base: 12.0, budget_expired: true,
            }});
        }
        journal.begin(&id(key), "arguments").unwrap();
        journal.complete(&id(key), serde_json::to_value(Answer::Ok(Reply::Native {
            body: NativeReply::Done { request_id: id(key), outcome: Box::new(Outcome::AndroidWebviewCompleted {
                completion: A::Progress { observation: Box::new(progress.clone()) },
            }) },
        })).unwrap()).unwrap();
    }
    let Outcome::AndroidWebviewCompared { comparison } = compare_for_ready(&ready, &journal,
        &id("progress-pending"), &id("progress-paused"), None, None).unwrap() else { panic!() };
    assert!(comparison.changes.iter().any(|c| c.path == "/progress/timers/state"
        && c.before.value == Some(json!("pending")) && c.after.value == Some(json!("paused"))));
    assert!(comparison.changes.iter().any(|c| c.path == "/progress/lifecycle"
        && c.before.value == Some(Value::Null)
        && c.after.value == Some(serde_json::to_value(&progress.lifecycle).unwrap())));
    journal.begin(&id("pending"), "arguments").unwrap();
    journal.dispatched(&id("pending")).unwrap();
    let original = std::fs::read(&path).unwrap();
    let op = frontcap_recorder_protocol::native::Op::CompareWebviewProgress {
        before: id("before"), after: id("after"), before_capture: None, after_capture: Some(id("after")),
    };
    ready.validate_operation(&op).unwrap();
    let compare = || compare_for_ready(&ready, &journal, &id("before"), &id("after"), None, Some(&id("after"))).unwrap();
    let first = compare();
    assert_eq!(first, compare());
    let Outcome::AndroidWebviewCompared { comparison } = first else { panic!() };
    assert_eq!(comparison.changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
        vec!["/pointers", "/snapshot/focused", "/snapshot/frame_activity", "/snapshot/href", "/snapshot/navigation", "/snapshot/timer_activity"]);
    assert!(!comparison.changes[2].before.present);
    assert_eq!(comparison.changes[2].after.value, Some(frame_activity));
    assert!(!comparison.changes[4].before.present);
    assert_eq!(comparison.changes[4].after.value, Some(navigation));
    assert!(!comparison.changes[5].before.present);
    assert_eq!(comparison.changes[5].after.value, Some(timer_activity));
    assert!(!comparison.changes[0].before.present && comparison.changes[0].after.present);
    assert_eq!(comparison.changes[1].after.value, Some(json!(false)));
    assert_eq!(comparison.capture_content_changed, None);
    assert_eq!(comparison.after.capture.unwrap().request_id, id("after"));
    for key in ["missing", "pending", "uncertain", "wrong-id", "other-host"] {
        assert!(compare_for_ready(&ready, &journal, &id(key), &id("after"), None, None).is_err());
    }
    assert!(compare_for_ready(&ready, &journal, &id("before"), &id("after"), Some(&id("before")), None).is_err());
    let Outcome::AndroidWebviewCompared { comparison } = compare_for_ready(&ready, &journal,
        &id("after"), &id("after"), Some(&id("after")), Some(&id("after"))).unwrap() else { panic!() };
    assert!(comparison.changes.is_empty());
    assert_eq!(comparison.capture_content_changed, Some(false));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(!root.path().join("historical.png").exists());
}
