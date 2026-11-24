use super::*;

fn toast() -> Locator {
    Locator::Css { selector: ".toast".into() }
}

#[test]
fn predicate_schema() {
    let p: Predicate = serde_json::from_value(serde_json::json!({
        "is": "opacity_at_least", "target": { "by": "css", "selector": ".toast" }, "milli": 1000
    })).unwrap();
    assert_eq!(p, Predicate::OpacityAtLeast { target: toast(), milli: 1000 });
    assert!(serde_json::from_value::<Predicate>(serde_json::json!({ "is": "visible", "target": { "by": "css", "selector": "a" }, "extra": 1 })).is_err());
}

#[test]
fn validation_bounds() {
    assert!(Predicate::OpacityAtLeast { target: toast(), milli: 1001 }.validate().is_err());
    assert!(Predicate::Visible { target: Locator::Css { selector: "  ".into() } }.validate().is_err());
    assert!(Predicate::Visible { target: Locator::Role { role: "button".into(), name: Some("Pay".into()) } }.validate().is_ok());
}

#[test]
fn registry_rejects_duplicates_and_lists_sorted() {
    let mut r = WatchRegistry::default();
    let b = WatchName::new("b").unwrap();
    let a = WatchName::new("a").unwrap();
    r.add(b.clone(), Predicate::Visible { target: toast() }).unwrap();
    r.add(a.clone(), Predicate::Attached { target: toast() }).unwrap();
    assert!(matches!(r.add(a.clone(), Predicate::Attached { target: toast() }), Err(PredicateError::Duplicate(_))));
    let names: Vec<&str> = r.list().iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["a", "b"]);
    assert!(r.remove(&a).is_ok());
    assert!(matches!(r.remove(&a), Err(PredicateError::NotFound(_))));
}

#[test]
fn invalid_predicate_is_not_registered() {
    let mut r = WatchRegistry::default();
    let n = WatchName::new("x").unwrap();
    assert!(r.add(n.clone(), Predicate::OpacityAtLeast { target: toast(), milli: 5000 }).is_err());
    assert!(r.get(&n).is_none());
}

#[test]
fn watch_registry_round_trips() {
    let mut r = WatchRegistry::default();
    r.add(WatchName::new("toast").unwrap(), Predicate::Visible { target: toast() }).unwrap();
    let back: WatchRegistry = serde_json::from_str(&serde_json::to_string(&r).unwrap()).unwrap();
    assert_eq!(back.list().len(), 1);
}

#[test]
fn ref_locators_have_their_own_tag_and_shape() {
    let l: Locator = serde_json::from_str(r#"{"by":"ref","id":"e3"}"#).unwrap();
    assert_eq!(l, Locator::Ref { id: "e3".into() });
    let p = Predicate::Visible { target: Locator::Ref { id: "x".into() } };
    assert!(p.validate().is_err(), "a reference is e<digits>");
    assert!(Predicate::Visible { target: Locator::Ref { id: "e12".into() } }.validate().is_ok());
}

#[test]
fn frame_locators_nest_and_validate_both_parts() {
    let l: Locator = serde_json::from_str(r##"{"by":"frame","frame":{"by":"css","selector":"#billing"},"target":{"by":"frame","frame":{"by":"ref","id":"e4"},"target":{"by":"text","text":"Pay"}}}"##).unwrap();
    assert_eq!(l.refs(), vec!["e4"]);
    assert!(!l.has_role());
    let p = Predicate::Visible { target: l };
    assert!(p.validate().is_ok());
    let empty: Locator = serde_json::from_str(r##"{"by":"frame","frame":{"by":"css","selector":" "},"target":{"by":"text","text":"Pay"}}"##).unwrap();
    assert_eq!(Predicate::Visible { target: empty }.validate(), Err(PredicateError::EmptySelector));
    let css: Locator = serde_json::from_str(r##"{"by":"frame","frame":{"by":"css","selector":"iframe"},"target":{"by":"css","selector":"#x"}}"##).unwrap();
    assert!(!css.has_role());
}

#[test]
fn document_selectors_validate_and_preserve_root_locators() {
    let root: Locator = serde_json::from_str(r#"{"by":"document"}"#).unwrap();
    assert_eq!(root, Locator::Document { selector: None });
    assert_eq!(serde_json::to_string(&root).unwrap(), r#"{"by":"document"}"#);
    for (selector, valid) in [("#save", true), (" ", false)] {
        let predicate = Predicate::Attached { target: Locator::Document { selector: Some(selector.into()) } };
        assert_eq!(predicate.validate().is_ok(), valid);
    }
}

#[test]
fn references_may_name_the_generation_that_issued_them() {
    for id in ["e3", "e12-a0b1c2"] {
        assert!(Predicate::Visible { target: Locator::Ref { id: id.into() } }.validate().is_ok(), "{id}");
    }
    for id in ["e", "e12-", "e12-xyz123", "e12-A0B1C2", "e12-a0b1c2d", "x12-a0b1c2"] {
        assert!(Predicate::Visible { target: Locator::Ref { id: id.into() } }.validate().is_err(), "{id}");
    }
}

#[test]
fn a_popup_watch_keeps_its_context_and_a_page_watch_reads_as_before() {
    let mut r = WatchRegistry::default();
    let p = Predicate::Attached { target: Locator::Css { selector: "#ok".into() } };
    r.add(WatchName::new("page").unwrap(), p.clone()).unwrap();
    r.add_in(WatchName::new("allowed").unwrap(), p.clone(), Some("popup-1".into())).unwrap();
    assert_eq!(r.context(&WatchName::new("allowed").unwrap()), Some("popup-1"));
    assert_eq!(r.context(&WatchName::new("page").unwrap()), None);
    assert!(r.any_in(Some("popup-1")) && r.any_in(None) && !r.any_in(Some("popup-2")));
    let json = serde_json::to_value(&r).unwrap();
    assert_eq!(json["page"], serde_json::to_value(&p).unwrap(), "a page watch is stored as its predicate");
    assert_eq!(json["allowed"]["context"], "popup-1");
    assert_eq!(serde_json::from_value::<WatchRegistry>(json).unwrap(), r);
}
