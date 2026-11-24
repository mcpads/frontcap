use super::*;

fn t(ms: u64) -> VirtualTime {
    VirtualTime(ms * 1000)
}

#[test]
fn a_frame_shows_the_state_of_the_last_mark_before_it_was_rendered() {
    let mut tl = Timeline::default();
    tl.changed(1.0, t(0), 1, 0);
    tl.moving(2.0, 1, 0);
    tl.stopped(t(50));
    tl.moving(3.0, 1, 0);
    tl.stopped(t(100));
    // Rendered after time stopped at 50 and before it moved on: the state at 50.
    assert_eq!(tl.label(2.5).map(|m| m.t_us), Some(Some(t(50))));
    assert_eq!(tl.label(3.2).map(|m| m.t_us), Some(Some(t(100))));
    assert_eq!(tl.label(0.5).map(|m| m.t_us), Some(Some(t(0))), "before every mark: the first");
}

#[test]
fn a_frame_rendered_while_time_moves_is_unlabelled_until_it_stops() {
    let mut tl = Timeline::default();
    tl.changed(1.0, t(0), 1, 0);
    tl.moving(2.0, 1, 0);
    assert_eq!(tl.label(2.1).map(|m| m.t_us), Some(None));
    tl.stopped(t(16));
    assert_eq!(tl.label(2.1).map(|m| m.t_us), Some(Some(t(16))));
}

#[test]
fn a_frame_is_current_only_if_nothing_happened_after_its_mark() {
    let mut tl = Timeline::default();
    tl.changed(1.0, t(0), 1, 0);
    let m = tl.label(1.5).unwrap().clone();
    assert!(tl.is_latest(&m));
    tl.changed(2.0, t(0), 1, 1);
    assert!(!tl.is_latest(&m));
}

#[test]
fn a_frame_after_states_left_unrendered_is_not_exact() {
    let mut tl = Timeline::default();
    tl.changed(1.0, t(0), 1, 0);
    assert!(tl.attribute(1.5).unwrap().1, "one state, one frame: exact");
    // Three short segments in a few milliseconds: a frame arriving now may show any of them.
    tl.moving(10.000, 1, 0);
    tl.stopped(t(16));
    tl.moving(10.002, 1, 0);
    tl.stopped(t(32));
    tl.moving(10.004, 1, 0);
    tl.stopped(t(48));
    let (mark, exact) = tl.attribute(10.010).unwrap();
    assert_eq!((mark.t_us, exact), (Some(t(48)), false));
    // Once that window has passed, a frame shows the last state.
    let (mark, exact) = tl.attribute(10.200).unwrap();
    assert_eq!((mark.t_us, exact), (Some(t(48)), true));
}

#[test]
fn a_quiet_screencast_after_time_moved_shows_the_latest_state() {
    let mut tl = Timeline::default();
    tl.changed(1.0, t(0), 1, 0);
    tl.attribute(1.5).unwrap();
    // A click while frozen: its frame may still come, so the older frame is not taken for it.
    tl.changed(2.0, t(0), 1, 1);
    assert_eq!(tl.unchanged_since(1.5), None);
    // Time moved on and nothing was drawn: the frame at 1.5 shows the state at 300.
    tl.moving(3.0, 1, 1);
    tl.stopped(t(300));
    let m = tl.unchanged_since(1.5).expect("inferred");
    assert_eq!((m.t_us, tl.is_latest(&m)), (Some(t(300)), true));
    assert_eq!(tl.unchanged_since(1.5), None, "once");
}
