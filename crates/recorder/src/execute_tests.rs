use super::*;
use frontcap_core::ids::WatchName;
use frontcap_core::time::VirtualTime;

#[test]
fn clock_goals_are_absolute() {
    let now = VirtualTime(1_000);
    assert_eq!(goal_of(&Op::Advance { by_us: 500 }, now), Some(VirtualTime(1_500)));
    assert_eq!(goal_of(&Op::Until { watch: WatchName::new("w").unwrap(), max_us: 7 }, now), Some(VirtualTime(1_007)));
    assert_eq!(goal_of(&Op::Settle { idle_us: 1, max_us: 9 }, now), Some(VirtualTime(1_009)));
    assert_eq!(goal_of(&Op::Stop, now), None);
}

#[test]
fn chromium_errors_map_to_protocol_codes() {
    use frontcap_chromium::ChromiumError as E;
    assert_eq!(code_of(&E::NotActionable("disabled".into())), ErrorCode::NotActionable);
    assert_eq!(code_of(&E::Ambiguous { locator: "x".into(), count: 2 }), ErrorCode::AmbiguousLocator);
    assert_eq!(code_of(&E::NotFound("x".into())), ErrorCode::NotFound);
    assert_eq!(code_of(&E::AtomicityViolation { delivered_to: "x".into() }), ErrorCode::AtomicityViolation);
    assert_eq!(code_of(&E::Unresponsive { goal_us: 1000 }), ErrorCode::PageUnresponsive);
    assert_eq!(code_of(&E::RendererCrashed), ErrorCode::PageUnresponsive);
    assert_eq!(code_of(&E::Unsupported("x".into())), ErrorCode::UnsupportedCapability);
    assert_eq!(code_of(&E::DocumentNotStarted), ErrorCode::UnsupportedCapability);
    assert_eq!(code_of(&E::Launch(browser_pin::LaunchError::SandboxRestricted)), ErrorCode::UnsupportedCapability);
    assert_eq!(code_of(&E::NoSuchHold("req-1".into())), ErrorCode::NoSuchHold);
    assert_eq!(code_of(&E::ForeignRef("e1-abcdef".into())), ErrorCode::InvalidArguments);
    let duplicate = frontcap_core::predicate::PredicateError::Duplicate(frontcap_core::ids::WatchName::new("w").unwrap());
    assert_eq!(code_of(&E::Predicate(duplicate)), ErrorCode::InvalidArguments);
}
