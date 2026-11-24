use super::*;
use frontcap_core::ids::{RequestId, WatchName};
use frontcap_core::predicate::{Locator, Predicate};

#[test]
fn ops_roundtrip_with_snake_case_tags() {
    let call = Call::Mutate {
        lease: Some(LeaseToken::new("0123456789abcdef0123456789abcdef").unwrap()),
        request_id: RequestId::new("r-1").unwrap(),
        op: Op::WatchAdd {
            name: WatchName::new("shown").unwrap(),
            predicate: Predicate::Visible { target: Locator::Css { selector: ".t".into() } },
            context: None,
        },
    };
    let text = serde_json::to_string(&call).unwrap();
    assert!(text.contains(r#""call":"mutate""#) && text.contains(r#""op":"watch_add""#), "{text}");
    assert_eq!(serde_json::from_str::<Call>(&text).unwrap(), call);
}

#[test]
fn op_classes() {
    assert_eq!(Op::Advance { by_us: 1 }.class(), OpClass::Clock);
    let act = |action| Op::Act { action, then: None, context: None };
    let b = || Locator::Css { selector: "b".into() };
    assert_eq!(act(frontcap_core::action::Action::Click { target: b() }).class(), OpClass::Input);
    assert_eq!(act(frontcap_core::action::Action::Reload {}).class(), OpClass::Navigation);
    assert_eq!(act(frontcap_core::action::Action::Close {}).class(), OpClass::Navigation);
    assert_eq!(act(frontcap_core::action::Action::WaitActionable { target: b(), max_us: 1 }).class(), OpClass::Clock);
    assert_eq!(Op::WatchRemove { name: WatchName::new("w").unwrap() }.class(), OpClass::RecorderState);
    assert_eq!(Op::Stop.class(), OpClass::Lifecycle);
}

#[test]
fn lease_tokens_are_32_lowercase_hex() {
    assert!(LeaseToken::new("0123456789abcdef0123456789abcdef").is_ok());
    assert!(LeaseToken::new("0123").is_err());
    assert!(LeaseToken::new("0123456789ABCDEF0123456789abcdef").is_err());
    let t = LeaseToken::generate();
    assert_eq!(t.as_str().len(), 32);
    assert_ne!(t, LeaseToken::generate());
    assert!(serde_json::from_str::<LeaseToken>("\"nope\"").is_err());
}

#[test]
fn every_error_code_has_a_recovery() {
    for code in ErrorCode::ALL {
        let e = ErrorReply::new(*code, "m");
        assert!(!e.recovery.is_empty(), "{code:?}");
    }
}

#[test]
fn answers_roundtrip() {
    let ok = Answer::Ok(Reply::Released);
    let err = Answer::Err(ErrorReply::new(ErrorCode::Busy, "running r-1"));
    for a in [ok, err] {
        let t = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<Answer>(&t).unwrap(), a);
    }
}
