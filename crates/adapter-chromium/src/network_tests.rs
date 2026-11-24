use super::*;

fn at(ms: u64) -> VirtualTime {
    VirtualTime(ms * 1000)
}

#[test]
fn everything_commits_when_no_request_is_unhandled() {
    let (now, later) = split_at_horizon(vec![(at(30), 'c'), (at(10), 'a'), (at(20), 'b')], None);
    assert_eq!(now, vec![(at(10), 'a'), (at(20), 'b'), (at(30), 'c')]);
    assert!(later.is_empty());
}

#[test]
fn events_after_an_unhandled_requests_issue_wait_for_it() {
    let (now, later) = split_at_horizon(vec![(at(3010), 'c'), (at(2990), 'a'), (at(3000), 'b')], Some(at(3000)));
    assert_eq!(now, vec![(at(2990), 'a'), (at(3000), 'b')]);
    assert_eq!(later, vec![(at(3010), 'c')]);
}

#[test]
fn a_request_handled_before_its_issue_is_seen_does_not_hold_the_trace() {
    let mut i = Interceptions::default();
    assert!(!i.handled("n1"));
    i.issued("n1", "https://a.test/1", at(3000), Arrival(1));
    assert_eq!(i.horizon(), None);
    assert!(i.known("n1"), "a redirect's next hop is not waited for");
}

#[test]
fn a_waiting_request_holds_the_trace_until_handled_or_ended() {
    let mut i = Interceptions::default();
    i.issued("n1", "https://a.test/1", at(3000), Arrival(2));
    i.issued("n2", "https://a.test/2", at(2000), Arrival(1));
    assert_eq!(i.horizon(), Some(at(2000)));
    i.ended("n2");
    assert_eq!(i.horizon(), Some(at(3000)));
    assert!(!i.handled("n1"));
    assert_eq!(i.horizon(), None);
}

#[test]
fn a_request_given_up_on_is_reported_and_known_when_it_arrives() {
    let mut i = Interceptions::default();
    i.issued("n1", "https://a.test/1", at(3000), Arrival(1));
    assert_eq!(i.give_up(), vec![GivenUp { what: "the request to https://a.test/1".into(), t: at(3000) }]);
    assert_eq!(i.horizon(), None);
    assert!(i.known("n1"));
    assert!(i.handled("n1"), "a late interception is known as overdue");
    assert!(!i.handled("n1"));
}

#[test]
fn browser_announcements_hold_evidence_without_blocking_renderer_order() {
    let mut i = Interceptions::default();
    i.issued_by_browser("worker", "https://a.test/worker.js", at(0), Arrival(1));
    i.issued("fetch", "https://a.test/fetch", at(10), Arrival(2));
    assert_eq!(i.place("worker"), None);
    assert_eq!(i.first_request(), Some(Arrival(2)));
    assert!(!i.earlier_than(Arrival(2)), "the browser's announcement cannot order renderer work");
    assert_eq!(i.horizon(), Some(at(0)), "the script is still outstanding evidence");
    assert_eq!(i.give_up_before(Arrival(3), None), vec![GivenUp { what: "the request to https://a.test/fetch".into(), t: at(10) }]);
    assert_eq!(i.give_up(), vec![GivenUp { what: "the request to https://a.test/worker.js".into(), t: at(0) }]);
}

/// A frame's navigation and a fetch issued after it: the fetch waits for the navigation's interception,
/// whichever arrives first; a request issued before both waits for neither.
#[test]
fn requests_wait_for_those_issued_before_them() {
    let mut i = Interceptions::default();
    i.issued("early", "https://a.test/e", at(0), Arrival(1));
    i.navigation_requested("F", "https://w.test/", at(0), Arrival(5));
    i.issued("fetch", "https://a.test/f", at(0), Arrival(6));
    assert_eq!(i.place("fetch"), Some(Arrival(6)));
    assert!(i.earlier_than(Arrival(6)), "the navigation and `early`");
    assert!(!i.earlier_than(Arrival(1)));
    i.handled("early");
    assert!(i.earlier_than(Arrival(6)), "still the navigation");
    assert_eq!(i.navigation_place("F"), Some(Arrival(5)));
    assert!(!i.navigation_ended("F"));
    assert!(!i.earlier_than(Arrival(6)));
    assert_eq!(i.horizon(), Some(at(0)), "the fetch itself");
}

/// A frame's newer navigation replaces the one it cancels; one given up on is known when it arrives.
#[test]
fn a_frames_navigation_is_replaced_and_given_up_on() {
    let mut i = Interceptions::default();
    i.navigation_requested("F", "https://a.test/", at(10), Arrival(3));
    i.navigation_requested("F", "https://b.test/", at(10), Arrival(7));
    assert_eq!(i.navigation_place("F"), Some(Arrival(7)));
    i.issued("later", "https://a.test/l", at(10), Arrival(9));
    // Announced just now: not yet due under a wall-time bound that has not passed.
    if let Some(by) = std::time::Instant::now().checked_sub(std::time::Duration::from_secs(60)) {
        assert_eq!(i.give_up_before(Arrival(9), Some(by)), vec![]);
    }
    assert_eq!(i.give_up_before(Arrival(9), Some(std::time::Instant::now())), vec![GivenUp { what: "a frame's navigation to https://b.test/".into(), t: at(10) }]);
    assert!(i.place("later").is_some(), "only what was issued before");
    assert!(i.navigation_ended("F"), "its late interception is known as overdue");
}

/// A replaced navigation's late interception leaves the one that replaced it pending.
#[test]
fn a_replaced_navigations_interception_leaves_the_newer_pending() {
    let mut i = Interceptions::default();
    i.navigation_requested("F", "https://a.test/", at(10), Arrival(3));
    i.navigation_requested("F", "https://b.test/#top", at(10), Arrival(7));
    assert!(!i.navigation_ended_at("F", "https://a.test/"));
    assert_eq!(i.navigation_place("F"), Some(Arrival(7)));
    assert!(!i.navigation_ended_at("F", "https://b.test/"));
    assert_eq!(i.navigation_place("F"), None);
}

/// A child navigation the platform answers with a redirect keeps its place for the next hop: requests
/// the page issued after it wait for that hop, whatever its URL, as they waited for the first.
#[test]
fn a_redirected_navigation_keeps_its_place_for_its_next_hop() {
    let mut i = Interceptions::default();
    i.navigation_requested("R", "https://a.test/r", at(0), Arrival(3));
    i.navigation_requested("N", "https://a.test/n", at(0), Arrival(4));
    assert!(!i.navigation_ended_at("R", "https://a.test/r"));
    assert!(!i.earlier_than(Arrival(4)), "the first hop was handled");
    i.navigation_redirected("R");
    assert_eq!(i.navigation_place("R"), Some(Arrival(3)), "the next hop keeps the place");
    assert!(i.earlier_than(Arrival(4)), "the later navigation waits for it");
    assert!(!i.navigation_ended_at("R", "https://a.test/elsewhere"));
    assert!(!i.earlier_than(Arrival(4)));
    // Without a handled hop, or once the frame's navigation ended, nothing is put back.
    i.navigation_redirected("N");
    assert_eq!(i.navigation_place("N"), Some(Arrival(4)));
    i.navigation_ended("R");
    i.navigation_redirected("R");
    assert_eq!(i.navigation_place("R"), None);
}

/// An interception of a navigation the page replaced before its interception belongs to the browser's
/// timing: it is recognized once, and never for the navigation pending now.
#[test]
fn a_replaced_navigations_interception_is_known_as_replaced() {
    let mut i = Interceptions::default();
    i.navigation_requested("F", "https://w.test/c", at(100), Arrival(3));
    i.navigation_requested("F", "https://w.test/c2", at(100), Arrival(4));
    assert!(!i.replaced_navigation("F", "https://w.test/c2"), "the pending one");
    assert!(!i.replaced_navigation("G", "https://w.test/c"), "another frame's");
    assert!(i.replaced_navigation("F", "https://w.test/c#top"));
    assert!(!i.replaced_navigation("F", "https://w.test/c"), "consumed");
    // Requested again, it is the pending navigation, not a replaced one.
    i.navigation_requested("F", "https://w.test/c", at(200), Arrival(9));
    assert!(!i.replaced_navigation("F", "https://w.test/c"));
    assert!(i.replaced_navigation("F", "https://w.test/c2"));
}
