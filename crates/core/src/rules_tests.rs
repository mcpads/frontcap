use super::*;
use std::collections::BTreeMap;

fn view<'a>(method: &'a str, url: &'a str, headers: &'a BTreeMap<String, String>) -> RequestView<'a> {
    RequestView { method, url, headers, body: None }
}

#[test]
fn glob_matching() {
    let p = UrlPattern("https://api.example.test/v1/*".into());
    assert!(p.matches("https://api.example.test/v1/orders?x=1"));
    assert!(p.matches("https://api.example.test/v1/"));
    assert!(!p.matches("https://api.example.test/v2/orders"));
    assert!(UrlPattern("*".into()).matches(""));
    assert!(UrlPattern("a*b*c".into()).matches("aXXbYYc"));
    assert!(!UrlPattern("a*b*c".into()).matches("aXXbYY"));
}

#[test]
fn parses_rule_sets_and_rejects_unknown_fields() {
    let json = r#"[{"id":"orders","match":{"method":"POST","url":"https://api.example.test/orders"},
                    "action":{"type":"mock","status":201,"headers":{},"body":"{}","latency_ms":500}}]"#;
    let set = RuleSet::from_json(json).unwrap();
    assert_eq!(set.rules.len(), 1);
    assert!(RuleSet::from_json(&json.replace("\"latency_ms\":500", "\"latency_ms\":500,\"extra\":1")).is_err());
    assert!(RuleSet::from_json(r#"[{"id":"r","match":{"url":"*"},"action":{"type":"websocket_echo"}}]"#).is_err(), "no such action");
}

#[test]
fn duplicate_ids_and_empty_sequences_are_rejected() {
    let dup = r#"[{"id":"a","match":{"url":"*"},"action":{"type":"hold"}},{"id":"a","match":{"url":"*"},"action":{"type":"hold"}}]"#;
    assert!(matches!(RuleSet::from_json(dup), Err(RuleError::DuplicateId(_))));
    let empty = r#"[{"id":"s","match":{"url":"*"},"action":{"type":"sequence","steps":[],"after_last":"fail"}}]"#;
    assert!(matches!(RuleSet::from_json(empty), Err(RuleError::EmptySequence(_))));
}

#[test]
fn first_match_wins_and_method_filters() {
    let json = r#"[{"id":"get-only","match":{"method":"GET","url":"https://h.test/*"},"action":{"type":"hold"}},
                   {"id":"any","match":{"url":"https://h.test/*"},"action":{"type":"passthrough"}}]"#;
    let set = RuleSet::from_json(json).unwrap();
    let h = BTreeMap::new();
    assert_eq!(set.first_match(&view("GET", "https://h.test/a", &h)).unwrap().id.as_str(), "get-only");
    assert_eq!(set.first_match(&view("POST", "https://h.test/a", &h)).unwrap().id.as_str(), "any");
}

#[test]
fn header_names_match_case_insensitively() {
    let json = r#"[{"id":"auth","match":{"url":"*","headers":{"Authorization":"Bearer t"}},"action":{"type":"hold"}}]"#;
    let set = RuleSet::from_json(json).unwrap();
    let mut h = BTreeMap::new();
    h.insert("authorization".to_string(), "Bearer t".to_string());
    assert!(set.first_match(&view("GET", "https://x.test/", &h)).is_some());
    h.insert("authorization".to_string(), "bearer t".to_string());
    assert!(set.first_match(&view("GET", "https://x.test/", &h)).is_none());
}

#[test]
fn body_pointer_matching() {
    let json = r#"[{"id":"pay","match":{"url":"*","body":{"/amount":12000}},"action":{"type":"hold"}}]"#;
    let set = RuleSet::from_json(json).unwrap();
    let h = BTreeMap::new();
    let body = serde_json::json!({ "amount": 12000, "currency": "KRW" });
    let v = RequestView { method: "POST", url: "https://p.test/", headers: &h, body: Some(&body) };
    assert!(set.first_match(&v).is_some());
    let other = serde_json::json!({ "amount": 1 });
    let v = RequestView { body: Some(&other), ..v };
    assert!(set.first_match(&v).is_none());
}

#[test]
fn a_body_condition_that_is_not_a_pointer_is_refused() {
    let set = |key: &str| RuleSet::from_json(&format!(r#"[{{"id":"pay","match":{{"url":"*","body":{{{key:?}:1}}}},"action":{{"type":"hold"}}}}]"#));
    let e = set("amount").unwrap_err().to_string();
    assert!(e.contains("pay") && e.contains("\"/amount\""), "{e}");
    assert!(set("a/b").unwrap_err().to_string().contains("\"/a~1b\""));
    assert!(set("/a~2").is_err());
    for ok in ["", "/amount", "/items/0/id", "/a~1b~0c"] {
        assert!(set(ok).is_ok(), "{ok}");
    }
}

#[test]
fn effective_set_appends_explicit_defaults() {
    let set = RuleSet::effective(RuleSet::default(), &["http://127.0.0.1:8080".to_string()]).unwrap();
    let ids: Vec<&str> = set.rules.iter().map(|r| r.id.as_str()).collect();
    assert_eq!(ids, vec!["default-passthrough-0", "default-block"]);
    let h = BTreeMap::new();
    assert_eq!(set.first_match(&view("GET", "http://127.0.0.1:8080/api", &h)).unwrap().id.as_str(), "default-passthrough-0");
    assert_eq!(set.first_match(&view("GET", "https://cdn.example.test/x.js", &h)).unwrap().id.as_str(), "default-block");
}

#[test]
fn default_passthrough_is_origin_exact() {
    let set = RuleSet::effective(RuleSet::default(), &["http://127.0.0.1:8080".to_string()]).unwrap();
    let h = BTreeMap::new();
    for url in ["http://127.0.0.1:8081/api", "http://127.0.0.1:8080.evil.test/", "https://127.0.0.1:8080/"] {
        assert_eq!(set.first_match(&view("GET", url, &h)).unwrap().id.as_str(), "default-block", "{url}");
    }
    assert!(matches!(RuleSet::effective(RuleSet::default(), &["127.0.0.1:8080".to_string()]), Err(RuleError::BadOrigin(_))));
}

#[test]
fn sequence_cursor_advances_then_follows_after_last() {
    let step = |s: u16| MockSpec { status: s, headers: BTreeMap::new(), body: String::new(), body_base64: None, latency_ms: 0 };
    let steps = vec![step(200), step(500)];
    let id = RuleId::new("seq").unwrap();
    let mut c = SequenceCursors::default();
    assert_eq!(c.next(&id, &steps, AfterLast::RepeatLast).unwrap().status, 200);
    assert_eq!(c.next(&id, &steps, AfterLast::RepeatLast).unwrap().status, 500);
    assert_eq!(c.next(&id, &steps, AfterLast::RepeatLast).unwrap().status, 500);
    let mut c = SequenceCursors::default();
    c.next(&id, &steps, AfterLast::Fail);
    c.next(&id, &steps, AfterLast::Fail);
    assert!(c.next(&id, &steps, AfterLast::Fail).is_none());
    // Cursors are persisted control state.
    let restored: SequenceCursors = serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
    assert_eq!(restored, c);
}

#[test]
fn rule_set_hash_is_canonical() {
    let a = RuleSet::from_json(r#"[{"id":"a","match":{"url":"*"},"action":{"type":"hold"}}]"#).unwrap();
    let b = RuleSet::from_json(r#"[ { "action" : {"type":"hold"}, "match":{"url":"*"}, "id":"a" } ]"#).unwrap();
    assert_eq!(a.sha256(), b.sha256());
}

#[test]
fn latency_and_status_are_checked_when_rules_are_accepted() {
    let huge = r#"[{"id":"a","match":{"url":"*"},"action":{"type":"mock","status":200,"latency_ms":18446744073709552}}]"#;
    assert!(matches!(RuleSet::from_json(huge), Err(RuleError::BadMock { .. })), "latency must fit in microseconds");
    let zero = r#"[{"id":"a","match":{"url":"*"},"action":{"type":"mock","status":0}}]"#;
    assert!(matches!(RuleSet::from_json(zero), Err(RuleError::BadMock { .. })), "a status is 100–599");
    let step = r#"[{"id":"a","match":{"url":"*"},"action":{"type":"sequence","after_last":"fail","steps":[{"status":200},{"status":700}]}}]"#;
    assert!(matches!(RuleSet::from_json(step), Err(RuleError::BadMock { .. })));
    assert!(MockSpec { status: 204, headers: Default::default(), body: String::new(), body_base64: None, latency_ms: 5 }.validate().is_ok());
}

#[test]
fn header_names_and_values_are_checked_when_rules_are_accepted() {
    let spec = |name: &str, value: &str| MockSpec { status: 200, headers: [(name.to_string(), value.to_string())].into(), body: String::new(), body_base64: None, latency_ms: 0 };
    assert!(spec("content-type", "text/plain").validate().is_ok());
    assert!(spec("bad name", "x").validate().is_err(), "a header name is a token");
    assert!(spec("", "x").validate().is_err());
    assert!(spec("x-a", "one\r\ntwo").validate().is_err(), "a value has no line breaks");
    assert!(spec("x-a", "nul\0").validate().is_err());
    let rule = r#"[{"id":"a","match":{"url":"*"},"action":{"type":"mock","status":200,"headers":{"a:b":"x"}}}]"#;
    assert!(matches!(RuleSet::from_json(rule), Err(RuleError::BadMock { .. })));
}

#[test]
fn a_route_match_ignores_header_and_body_conditions() {
    let rules = RuleSet::from_json(
        r#"[{"id":"auth","match":{"url":"https://api.example.test/*","method":"POST","headers":{"authorization":"Bearer t"},"body":{"/kind":"x"}},
             "action":{"type":"mock","status":200}}]"#,
    )
    .unwrap();
    assert_eq!(rules.first_route_match("POST", "https://api.example.test/save").map(|r| r.id.as_str()), Some("auth"));
    assert!(rules.first_route_match("GET", "https://api.example.test/save").is_none());
}

#[test]
fn unchanged_rules_keep_their_sequence_position_across_installations() {
    let seq = r#"{"id":"s","match":{"url":"*"},"action":{"type":"sequence","steps":[{"status":200},{"status":503}],"after_last":"repeat_last"}}"#;
    let old = RuleSet::from_json(&format!("[{seq}]")).unwrap();
    let added = RuleSet::from_json(&format!(r#"[{{"id":"h","match":{{"url":"*"}},"action":{{"type":"hold"}}}},{seq}]"#)).unwrap();
    let Action::Sequence { steps, after_last } = &old.rules[0].action else { unreachable!() };
    let id = old.rules[0].id.clone();
    let mut cursors = SequenceCursors::default();
    assert_eq!(cursors.next(&id, steps, *after_last).unwrap().status, 200);

    let mut kept = cursors.carried(&old, &added);
    assert_eq!(kept.next(&id, steps, *after_last).unwrap().status, 503, "an unrelated insertion keeps the position");

    let edited = RuleSet::from_json(&format!("[{}]", seq.replace("503", "504"))).unwrap();
    let mut fresh = cursors.carried(&old, &edited);
    assert_eq!(fresh.next(&id, steps, *after_last).unwrap().status, 200, "a changed rule starts again");

    let removed = RuleSet::from_json(r#"[{"id":"h","match":{"url":"*"},"action":{"type":"hold"}}]"#).unwrap();
    assert!(!unchanged(&old, &removed, &id));
    let history = BTreeMap::from([(0, old.clone()), (1, added), (2, edited), (3, removed.clone())]);
    let since = installed_since(&history);
    assert_eq!(since.get(&RuleId::new("h").unwrap()), Some(&3), "h was removed at 2, so 3 is a new installation");
    assert!(!since.contains_key(&id));
}
