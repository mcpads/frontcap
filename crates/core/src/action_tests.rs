use super::*;
use crate::predicate::Locator;

fn css(s: &str) -> Locator {
    Locator::Css { selector: s.into() }
}

#[test]
fn actions_are_tagged_objects() {
    let a: Action = serde_json::from_str(r##"{"type":"type","target":{"by":"css","selector":"#q"},"text":"hi"}"##).unwrap();
    assert_eq!(a, Action::Type { target: css("#q"), text: "hi".into() });
    let p: Action = serde_json::from_str(r#"{"type":"press","key":"Enter"}"#).unwrap();
    assert_eq!(p, Action::Press { target: None, key: "Enter".into() });
    assert!(serde_json::from_str::<Action>(r#"{"type":"back","extra":1}"#).is_err());
    assert!(serde_json::from_str::<Action>(r#"{"type":"drag"}"#).is_err());
}

#[test]
fn name_target_and_args_describe_the_action() {
    let a = Action::Fill { target: css("#q"), value: "x".into() };
    assert_eq!((a.name(), a.target(), a.args()), ("fill", Some(&css("#q")), Some(serde_json::json!({"value": "x"}))));
    assert_eq!(Action::Click { target: css("b") }.args(), None);
    assert_eq!(Action::Press { target: None, key: "Enter".into() }.target(), None);
    assert_eq!(Action::WaitActionable { target: css("b"), max_us: 1 }.name(), "wait_actionable");
    assert_eq!(Action::Back {}.kind(), ActionKind::Navigation);
    assert_eq!(Action::Reload {}.args(), None);
    assert_eq!((Action::Close {}.name(), Action::Close {}.kind(), Action::Close {}.target()), ("close", ActionKind::Close, None));
    assert_eq!(Action::WaitActionable { target: css("b"), max_us: 1 }.kind(), ActionKind::Wait);
    assert_eq!(Action::Scroll { target: None, delta_x: 0, delta_y: 5 }.kind(), ActionKind::Input);
}
