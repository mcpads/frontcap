use super::*;

fn rules(json: &str) -> DialogRules {
    serde_json::from_str(json).unwrap()
}

#[test]
fn without_rules_dialogs_return_defaults_and_navigation_is_accepted() {
    let none = DialogRules::default();
    assert_eq!(none.answer(DialogKind::Confirm, "Delete?", None).unwrap(), DialogReply::Confirm(false));
    assert_eq!(none.answer(DialogKind::Prompt, "Name?", Some("x")).unwrap(), DialogReply::Prompt(None));
    assert_eq!(none.answer(DialogKind::Alert, "Hi", None).unwrap(), DialogReply::Alert);
    assert_eq!(none.answer(DialogKind::Beforeunload, "", None).unwrap(), DialogReply::Beforeunload(true));
}

#[test]
fn the_first_matching_rule_answers() {
    let r = rules(r#"[{"kind":"confirm","message":"Delete *","answer":"accept"},{"answer":"dismiss"}]"#);
    assert_eq!(r.answer(DialogKind::Confirm, "Delete \"Ideas\"?", None).unwrap(), DialogReply::Confirm(true));
    assert_eq!(r.answer(DialogKind::Confirm, "Leave?", None).unwrap(), DialogReply::Confirm(false), "the catch-all dismisses");
}

#[test]
fn an_accepted_prompt_gets_the_rules_text_or_its_default() {
    let r = rules(r#"[{"kind":"prompt","message":"Name*","answer":"accept","text":"Ada"},{"kind":"prompt","answer":"accept"}]"#);
    assert_eq!(r.answer(DialogKind::Prompt, "Name?", Some("x")).unwrap(), DialogReply::Prompt(Some("Ada".into())));
    assert_eq!(r.answer(DialogKind::Prompt, "Age?", Some("42")).unwrap(), DialogReply::Prompt(Some("42".into())));
    assert_eq!(r.answer(DialogKind::Prompt, "Age?", None).unwrap(), DialogReply::Prompt(Some(String::new())));
}

#[test]
fn a_dismissed_beforeunload_keeps_the_page() {
    let r = rules(r#"[{"kind":"beforeunload","answer":"dismiss"}]"#);
    assert_eq!(r.answer(DialogKind::Beforeunload, "", None).unwrap(), DialogReply::Beforeunload(false));
}

#[test]
fn the_page_receives_what_the_record_says() {
    assert_eq!(DialogReply::Confirm(true).recorded(), "true");
    assert_eq!(DialogReply::Prompt(Some("Ada".into())).recorded(), "\"Ada\"");
    assert_eq!(DialogReply::Prompt(None).recorded(), "null");
    assert_eq!(DialogReply::Alert.recorded(), "returned");
    assert_eq!(DialogReply::Beforeunload(false).recorded(), "dismissed");
}

#[test]
fn a_rule_without_an_answer_is_refused() {
    assert!(serde_json::from_str::<DialogRules>(r#"[{"kind":"confirm"}]"#).is_err());
}

#[test]
fn a_hold_is_pending_and_first_matching_rule_still_wins() {
    let rules: DialogRules = serde_json::from_str(r#"[{"message":"held*","answer":"hold"},{"answer":"accept"}]"#).unwrap();
    for kind in [DialogKind::Alert, DialogKind::Confirm, DialogKind::Prompt, DialogKind::Beforeunload] {
        assert_eq!(rules.answer(kind, "held question", Some("default")), None);
    }
    assert_eq!(rules.answer(DialogKind::Confirm, "other", None), Some(DialogReply::Confirm(true)));
}
