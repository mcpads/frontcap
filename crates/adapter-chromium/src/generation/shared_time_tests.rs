use super::*;

fn receipt(target: &str, instant: &str, closed: bool) -> page::FrontcapWindowClosure {
    page::FrontcapWindowClosure {
        target_id: target.into(),
        unix_time_us: instant.into(),
        closed,
    }
}

#[test]
fn native_closure_requires_the_completed_target_at_the_held_instant() {
    assert!(shared_popup_closure_matches(
        &[receipt("popup", "1000", true)],
        "popup",
        "1000"
    ));
    assert!(!shared_popup_closure_matches(&[], "popup", "1000"));
    assert!(!shared_popup_closure_matches(
        &[receipt("other", "1000", true)],
        "popup",
        "1000"
    ));
    assert!(!shared_popup_closure_matches(
        &[receipt("popup", "999", true)],
        "popup",
        "1000"
    ));
    assert!(!shared_popup_closure_matches(
        &[receipt("popup", "1000", false)],
        "popup",
        "1000"
    ));
    assert!(shared_popup_closure_matches(
        &[
            receipt("other", "999", false),
            receipt("popup", "1000", true)
        ],
        "popup",
        "1000"
    ));
}

#[test]
fn duplicate_closure_receipts_do_not_establish_a_unique_completed_close() {
    assert!(!shared_popup_closure_matches(
        &[
            receipt("popup", "1000", true),
            receipt("popup", "1000", true)
        ],
        "popup",
        "1000"
    ));
    assert!(!shared_popup_closure_matches(
        &[
            receipt("popup", "1000", true),
            receipt("popup", "1000", false)
        ],
        "popup",
        "1000"
    ));
}
