use super::*;
use frontcap_core::event::*;
use frontcap_core::ids::{RuleId, WatchName};
use frontcap_core::time::VirtualTime;

fn ev(seq: u64, t_ms: u64, kind: EventKind) -> BehaviorEvent {
    BehaviorEvent { seq, t_us: VirtualTime(t_ms * 1000), kind }
}

fn req(id: &str, url: &str, rule: &str) -> EventKind {
    EventKind::Request(RequestRecord { rules_revision: 0, id: id.into(), method: "GET".into(), url: url.into(), rule: Some(RuleId::new(rule).unwrap()), entry: None, context: None, body: None })
}

fn resp(id: &str, status: u16, source: ResponseSource, rule: &str) -> EventKind {
    EventKind::Response(ResponseRecord { id: id.into(), status, source, rule: Some(RuleId::new(rule).unwrap()) })
}

fn sample(name: &str, value: SampleValue, point: PointKind) -> EventKind {
    EventKind::Watch(WatchSample { name: WatchName::new(name).unwrap(), value, point, rseq: 0, document: 0, context: None })
}

fn events() -> Vec<BehaviorEvent> {
    vec![
        ev(0, 0, req("req-1", "http://127.0.0.1/index.html", "default-passthrough-0")),
        ev(1, 0, EventKind::Navigation(NavigationRecord { rseq: 0, url: "http://127.0.0.1/index.html".into(), document: 1, context: None })),
        ev(2, 5, resp("req-1", 200, ResponseSource::Passthrough, "default-passthrough-0")),
        ev(3, 10, req("req-2", "https://api.example.test/a", "api")),
        ev(4, 10, resp("req-2", 200, ResponseSource::Mock, "api")),
        ev(5, 20, req("req-3", "https://cdn.example.test/x.js", "default-block")),
        ev(6, 20, resp("req-3", 0, ResponseSource::Fail, "default-block")),
        ev(7, 30, req("req-4", "https://api.example.test/held", "held")),
        ev(8, 31, EventKind::Console(ConsoleRecord { level: "error".into(), text: "boom".into(), context: None, source: None })),
        ev(9, 32, EventKind::Console(ConsoleRecord { level: "warning".into(), text: "hm".into(), context: None, source: None })),
        ev(10, 33, EventKind::Console(ConsoleRecord { level: "log".into(), text: "x".into(), context: None, source: None })),
        ev(11, 40, sample("a", SampleValue::False, PointKind::Initial)),
        ev(12, 41, sample("b", SampleValue::True, PointKind::Initial)),
        ev(13, 42, sample("a", SampleValue::True, PointKind::Mutation)),
    ]
}

#[test]
fn effects_summarize_requests_by_outcome() {
    let e = effects_with(&events(), 0, None, |_| true);
    assert_eq!(
        e.requests,
        vec![
            "GET http://127.0.0.1/index.html → passthrough(rule=default-passthrough-0) 200".to_string(),
            "GET https://api.example.test/a → mock(rule=api) 200".to_string(),
            "GET https://cdn.example.test/x.js → fail(rule=default-block)".to_string(),
            "GET https://api.example.test/held → held(rule=held)".to_string(),
        ]
    );
    assert_eq!(e.unmatched_requests, vec!["GET https://cdn.example.test/x.js".to_string()]);
    assert_eq!(e.unmatched_next.as_deref(), Some("Blocked: let https://cdn.example.test through (launch passthrough_origins, or a passthrough rule), or answer with a rule"));
    assert_eq!(e.console, ConsoleCounts { error: 1, warn: 1 });
    assert_eq!(e.navigations, vec!["http://127.0.0.1/index.html".to_string()]);
}

#[test]
fn blocked_requests_name_each_origin_once_in_order() {
    let mut popup = RequestRecord { rules_revision: 0, id: "req-3".into(), method: "GET".into(), url: "http://localhost:8080/help.html?x=1".into(),
        rule: Some(RuleId::new("default-block").unwrap()), entry: None, context: Some("popup-1".into()), body: None };
    let mut events = vec![
        ev(0, 0, req("req-1", "https://cdn.example.test/a.js", "default-block")),
        ev(1, 0, req("req-2", "https://cdn.example.test/b.js", "default-block")),
        ev(2, 0, EventKind::Request(popup.clone())),
    ];
    popup.id = "req-4".into();
    popup.url = "https://cdn.example.test/c.js".into();
    events.push(ev(3, 0, EventKind::Request(popup)));
    let e = effects_with(&events, 0, None, |_| true);
    assert_eq!(e.unmatched_requests[2], "popup-1: GET http://localhost:8080/help.html?x=1");
    assert_eq!(e.unmatched_next.as_deref(), Some("Blocked: let https://cdn.example.test, http://localhost:8080 through (launch passthrough_origins, or a passthrough rule), or answer with a rule"));
    assert_eq!(effects_with(&events[..0], 0, None, |_| true).unmatched_next, None);
}

#[test]
fn console_effects_preserve_provenance_and_the_request_window() {
    let message = |seq, level: &str, text: &str| ev(seq, 50, EventKind::Console(ConsoleRecord {
        level: level.into(), text: text.into(), context: Some("popup-1".into()), source: Some("security".into()),
    }));
    let mut trace = vec![
        message(0, "error", "before"),
        message(1, "assert", "assertion failed"),
        message(2, "warning", "blocked resource"),
        message(3, "log", "ordinary output"),
        message(4, "warn", "page warning"),
        message(5, "error", "load failed"),
    ];
    let effects = effects_with(&trace, 1, Some(6), |_| false);
    assert_eq!(effects.console, ConsoleCounts { error: 2, warn: 2 });
    assert_eq!(effects.console_messages.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
        ["assertion failed", "blocked resource", "page warning", "load failed"]);
    for entry in &effects.console_messages {
        assert_eq!(entry.t_us, VirtualTime(50_000));
        assert_eq!(entry.context.as_deref(), Some("popup-1"));
        assert_eq!(entry.source.as_deref(), Some("security"));
    }
    trace.push(message(6, "error", "after at the same instant"));
    assert_eq!(effects_with(&trace, 1, Some(6), |_| false), effects);
    let history = console(&trace, &ConsoleFilter::default());
    assert_eq!(effects.console_messages[0], history[1]);
    assert_eq!(history[3].text, "ordinary output", "debug retains ordinary console output");
}

#[test]
fn input_effects_report_observed_events_without_inventing_older_evidence() {
    let action = |received_events| EventKind::Action(ActionRecord {
        received_events, pointer: None, keys: None, request_id: frontcap_core::ids::RequestId::new("cancel").unwrap(),
        action: "pointer_cancel".into(), target: None, args: None, delivered_to: None, context: Some("popup-1".into()),
    });
    let received = ReceivedInputEvent { step: 0, event: "pointercancel".into(), t_us: VirtualTime(50_000), trusted: true };
    let trace = vec![ev(0,50,action(None)), ev(1,50,action(Some(vec![received.clone()]))), ev(2,50,action(Some(vec![])))];
    assert!(effects_with(&trace,0,Some(1), |_|false).input.is_empty(), "older evidence is unknown");
    let effects = effects_with(&trace,1,Some(2), |_|false);
    assert_eq!(effects.input.len(),1);
    assert_eq!(effects.input[0].context.as_deref(),Some("popup-1"));
    assert_eq!(effects.input[0].events,vec![received]);
    let empty = effects_with(&trace,2,None, |_|false);
    assert_eq!(empty.input.len(),1);
    assert!(empty.input[0].events.is_empty(), "observed no matching event differs from unknown");
}

#[test]
fn effects_name_popup_contexts_and_refused_windows() {
    let popup_request = EventKind::Request(RequestRecord { rules_revision: 0, id: "req-1".into(), method: "GET".into(), url: "https://auth.example.test/login".into(), rule: Some(RuleId::new("login").unwrap()), entry: None, context: Some("popup-1".into()), body: None });
    let events = vec![
        ev(0, 0, EventKind::Context(ContextRecord { context: "popup-1".into(), event: ContextEvent::Opened, opener: Some("page".into()), target: Some("T".into()), no_opener: false })),
        ev(1, 0, popup_request),
        ev(2, 0, resp("req-1", 200, ResponseSource::Mock, "login")),
        ev(3, 0, EventKind::Navigation(NavigationRecord { rseq: 0, url: "https://auth.example.test/login".into(), document: 1, context: Some("popup-1".into()) })),
        ev(4, 5, EventKind::Context(ContextRecord { context: "popup-1".into(), event: ContextEvent::Closed, opener: None, target: None, no_opener: false })),
        ev(5, 6, EventKind::Taint(TaintRecord { source: TaintSource::NewWindow, detail: "a window opened without an opener was refused".into() })),
    ];
    let e = effects_with(&events, 0, None, |_| false);
    assert_eq!(e.requests, ["popup-1: GET https://auth.example.test/login → mock(rule=login) 200"]);
    assert_eq!(e.navigations, ["popup-1: https://auth.example.test/login"]);
    assert_eq!(e.contexts, ["popup-1 opened by page", "popup-1 closed", "refused: a window opened without an opener was refused"]);
}

#[test]
fn effects_list_focus_transitions_in_order() {
    let focus = |target: &str, focused: bool, context: Option<&str>| EventKind::Focus(FocusRecord { target: target.into(), focused, document: 1, rseq: 0, context: context.map(Into::into) });
    let events = vec![
        ev(0, 0, focus(r#"<input aria-label="Digit 1">"#, false, None)),
        ev(1, 0, focus(r#"<input aria-label="Digit 2">"#, true, None)),
        ev(2, 0, focus(r#"<input id="name">"#, true, Some("popup-1"))),
    ];
    let e = effects_with(&events, 0, None, |_| false);
    assert_eq!(e.focus, [r#"blur <input aria-label="Digit 1">"#, r#"focus <input aria-label="Digit 2">"#, r#"popup-1: focus <input id="name">"#]);
    assert!(effects_with(&events, 3, None, |_| false).focus.is_empty());
}

#[test]
fn effects_start_at_the_given_sequence_number() {
    let e = effects_with(&events(), 5, None, |_| true);
    assert_eq!(effects_with(&events(), 0, Some(3), |_| true).requests.len(), 1, "bounded above");
    assert_eq!(e.requests.len(), 2, "{:?}", e.requests);
    assert!(e.navigations.is_empty());
    // A response recorded after `after_seq` for a request issued before it is not an effect of this step.
    let late = effects_with(&events(), 4, None, |_| true);
    assert!(!late.requests.iter().any(|r| r.contains("/a ")), "{:?}", late.requests);
}

#[test]
fn history_is_the_named_watch_in_order() {
    let h = history(&events(), &WatchName::new("a").unwrap());
    assert_eq!(h.iter().map(|s| (s.seq, s.value)).collect::<Vec<_>>(), vec![(11, SampleValue::False), (13, SampleValue::True)]);
}

#[test]
fn a_response_after_the_window_does_not_rewrite_an_effect() {
    // req-4 is held inside the window [0, 9); a response recorded later must not change how it reads.
    let mut list = events();
    list.push(ev(20, 40, resp("req-4", 200, ResponseSource::Agent, "held")));
    let e = effects_with(&list, 0, Some(9), |_| true);
    assert!(e.requests.iter().any(|r| r == "GET https://api.example.test/held → held(rule=held)"), "{:?}", e.requests);
}

/// Fresh-consumer run 5: a request still loading under a rule that does not hold it is pending, not held.
#[test]
fn an_unanswered_request_is_held_only_under_a_hold_rule() {
    let e = effects_with(&events(), 0, None, |rule| rule == "elsewhere");
    assert!(e.requests.iter().any(|r| r == "GET https://api.example.test/held → pending(rule=held)"), "{:?}", e.requests);
}


#[test]
fn network_history_uses_actual_request_holds_instead_of_rule_names() {
    let mut trace = events();
    trace.push(ev(14, 45, req("req-5", "https://api.example.test/intercepted", "api")));
    trace.push(ev(15, 45, req("req-6", "https://api.example.test/loading", "api")));
    let held = ["req-5".to_string()].into_iter().collect();
    let entries = network(&trace, &NetworkFilter::default(), &held);
    assert_eq!(entries[3].request, "GET https://api.example.test/held → pending(rule=held)", "a released hold must not remain held just because its rule is named held");
    assert_eq!(entries[4].request, "GET https://api.example.test/intercepted → held(rule=api)", "an explicit stop owns this request independently of the rule action");
    assert_eq!(entries[5].request, "GET https://api.example.test/loading → pending(rule=api)", "another request under the same rule can remain in flight");
    assert_eq!(entries[4].answered_us, None, "a held request has no answer instant");
    trace.push(ev(16, 50, resp("req-5", 201, ResponseSource::Agent, "api")));
    let filter = NetworkFilter { url_contains: Some("intercepted".into()), ..Default::default() };
    let entries = network(&trace, &filter, &Default::default());
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].request, "GET https://api.example.test/intercepted → agent(rule=api) 201");
    assert_eq!((entries[0].t_us.0, entries[0].answered_us.map(|t| t.0)), (45_000, Some(50_000)), "issued at 45 ms, answered at 50 ms");
}

/// Consumer runs of scenarios 2–4 read what the page sent through hold rules: history shows it.
#[test]
fn network_history_shows_what_the_page_sent() {
    let mut trace = events();
    let mut post = req("req-8", "https://api.example.test/feedback", "feedback");
    if let EventKind::Request(r) = &mut post {
        r.method = "POST".into();
        r.body = RequestBody::of(Some(r#"{"rating":4}"#), Some(true));
    }
    trace.push(ev(19, 70, post));
    let mut upload = req("req-9", "https://api.example.test/avatar", "avatar");
    if let EventKind::Request(r) = &mut upload {
        r.body = RequestBody::of(None, Some(true));
    }
    trace.push(ev(20, 70, upload));
    let entries = network(&trace, &NetworkFilter { url_contains: Some("api.example.test/".into()), since_us: Some(70_000), ..Default::default() }, &Default::default());
    assert_eq!((entries[0].body.as_deref(), entries[0].body_bytes, entries[0].body_unavailable), (Some(r#"{"rating":4}"#), Some(12), false));
    assert_eq!((entries[1].body.as_deref(), entries[1].body_bytes, entries[1].body_unavailable), (None, None, true));
    let long = "é".repeat(3000);
    let cut = RequestBody::of(Some(&long), Some(true)).unwrap();
    assert_eq!((cut.text.unwrap().len(), cut.bytes), (4096, Some(6000)), "cut at a character boundary within 4 KiB");
}

/// Scenario 5 consumer run: a preflight the platform answered is told apart from the rule's own answer.
#[test]
fn a_platform_answered_preflight_is_described_as_one() {
    let mut trace = events();
    let mut preflight = req("req-7", "https://api.example.test/token", "token");
    if let EventKind::Request(r) = &mut preflight {
        r.method = "OPTIONS".into();
    }
    trace.push(ev(17, 60, preflight));
    trace.push(ev(18, 60, resp("req-7", 204, ResponseSource::Preflight, "token")));
    let e = effects_with(&trace, 17, None, |_| false);
    assert_eq!(e.requests, vec!["OPTIONS https://api.example.test/token → preflight 204".to_string()]);
}

/// A step's effects list the first entries of each list and count the rest; the request history lists
/// every request.
#[test]
fn effects_are_bounded_and_the_history_is_whole() {
    use frontcap_core::event::{ConsoleRecord, RequestRecord, ResponseRecord};
    let mut events = Vec::new();
    for i in 0..30u64 {
        events.push(BehaviorEvent { seq: 2 * i, t_us: VirtualTime(0), kind: EventKind::Request(RequestRecord {
            rules_revision: 0, id: format!("req-{i}"), method: "GET".into(), url: format!("https://a.test/{i}.png"),
            rule: Some(frontcap_core::ids::RuleId::new("img").unwrap()), entry: None, context: None, body: None }) });
        events.push(BehaviorEvent { seq: 2 * i + 1, t_us: VirtualTime(0), kind: EventKind::Response(ResponseRecord {
            id: format!("req-{i}"), status: 404, source: ResponseSource::Mock, rule: Some(frontcap_core::ids::RuleId::new("img").unwrap()) }) });
    }
    for i in 0..12u64 {
        events.push(BehaviorEvent { seq: 100 + i, t_us: VirtualTime(i), kind: EventKind::Console(ConsoleRecord {
            level: "error".into(), text: "Failed to load resource".into(), source: Some("network".into()), context: None }) });
    }
    let e = effects_with(&events, 0, None, |_| false);
    assert_eq!(e.requests.len(), Effects::ENTRIES);
    assert_eq!((e.console_messages.len(), e.console_messages[0].count, e.console_messages[0].t_us), (1, Some(12), VirtualTime(0)));
    let omitted = e.omitted.unwrap();
    assert_eq!((omitted.entries, omitted.next.as_str()), (BTreeMap::from([("requests".to_string(), 10)]), "debug requests list every entry"));
    assert_eq!(network(&events, &NetworkFilter::default(), &Default::default()).len(), 30);
}
