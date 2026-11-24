use super::*;

fn sequence(id: &str, first: &str) -> RuleSet {
    RuleSet::from_json(&format!(r#"[{{"id":"{id}","match":{{"url":"*"}},"action":{{"type":"sequence","steps":[{{"status":200,"body":"{first}"}},{{"status":200,"body":"second"}}],"after_last":"fail"}}}}]"#)).unwrap()
}

fn request(id: &str, revision: u64, rule: &str) -> BehaviorEvent {
    BehaviorEvent { seq: 0, t_us: VirtualTime::ZERO, kind: EventKind::Request(RequestRecord {
        rules_revision: revision, id: id.into(), method: "GET".into(), url: "http://fixture/request".into(),
        rule: Some(RuleId::new(rule).unwrap()), entry: None, context: None, body: None,
    }) }
}

fn next_body(d: &mut Derived, rules: &RuleSet) -> Option<String> {
    let Action::Sequence { steps, after_last } = &rules.rules[0].action else { unreachable!() };
    d.cursors.next(&rules.rules[0].id, steps, *after_last).map(|s| s.body)
}

#[test]
fn a_changed_rule_starts_its_consumption_again_without_losing_old_requests() {
    let old = sequence("same", "first");
    let new = sequence("same", "changed");
    let installations = BTreeMap::from([(0, old), (1, new.clone())]);
    let events = vec![request("req-1", 0, "same"), request("req-2", 1, "same"), request("req-3", 0, "same")];
    let mut recovered = derive(&events, &new, &frontcap_core::rules::installed_since(&installations));
    assert_eq!(recovered.next_ordinal, 3);
    assert_eq!(recovered.unanswered, vec!["req-1", "req-2", "req-3"]);
    assert_eq!(next_body(&mut recovered, &new).as_deref(), Some("second"));
    assert!(next_body(&mut recovered, &new).is_none());
}

#[test]
fn an_unchanged_rule_continues_its_consumption_across_installations() {
    let kept = sequence("kept", "first");
    // Revision 1 added another rule in front; revision 2 removed it again. `kept` never changed.
    let mut with_other = RuleSet::from_json(r#"[{"id":"other","match":{"url":"*"},"action":{"type":"hold"}}]"#).unwrap();
    with_other.rules.extend(kept.rules.clone());
    let installations = BTreeMap::from([(0, kept.clone()), (1, with_other), (2, kept.clone())]);
    let since = frontcap_core::rules::installed_since(&installations);
    assert_eq!(since.get(&RuleId::new("kept").unwrap()), Some(&0));
    let events = vec![request("req-1", 0, "kept"), request("req-2", 1, "other")];
    let mut recovered = derive(&events, &kept, &since);
    assert_eq!(next_body(&mut recovered, &kept).as_deref(), Some("second"), "the step taken at revision 0 stays consumed");
}

fn event(kind: EventKind) -> BehaviorEvent {
    BehaviorEvent { seq: 0, t_us: VirtualTime::ZERO, kind }
}

fn focus(rseq: u64, document: u64, context: Option<&str>) -> BehaviorEvent {
    event(EventKind::Focus(frontcap_core::event::FocusRecord {
        target: "<input>".into(), focused: true, document, rseq, context: context.map(str::to_string),
    }))
}

/// A popup's documents and reports are its own: recovering the launch page's observer cursor from them
/// would skip the page's next reports or read its earlier ones again.
#[test]
fn a_popups_records_do_not_move_the_launch_pages_cursor() {
    let navigation = |document, context: Option<&str>| event(EventKind::Navigation(frontcap_core::event::NavigationRecord { rseq: 0,
        url: "https://app.example.test/".into(), document, context: context.map(str::to_string),
    }));
    let message = |rseq, context: Option<&str>| event(EventKind::Message(frontcap_core::event::MessageRecord {
        origin: "https://auth.example.test".into(), data: "\"hi\"".into(), document: 1, rseq, context: context.map(str::to_string),
    }));
    let events = vec![
        navigation(1, None), focus(3, 1, None), message(4, None),
        navigation(1, Some("popup-1")), focus(9, 1, Some("popup-1")), message(10, Some("popup-1")), navigation(2, Some("popup-1")),
    ];
    let recovered = derive(&events, &RuleSet::default(), &BTreeMap::new());
    assert_eq!((recovered.documents, recovered.max_rseq), (1, 4));
}
