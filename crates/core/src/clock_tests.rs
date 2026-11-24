use super::*;
use crate::time::{VirtualDuration, VirtualTime};
use proptest::prelude::*;

fn bounds(goal: u64) -> StopBounds {
    StopBounds { goal: VirtualTime(goal), agenda_next: None, max_segment: None, frame_interval: None }
}

#[test]
fn stops_at_goal_without_other_bounds() {
    assert_eq!(next_stop(VirtualTime(0), &bounds(1000)), VirtualTime(1000));
}

#[test]
fn stops_at_agenda_before_goal() {
    let b = StopBounds { agenda_next: Some(VirtualTime(300)), ..bounds(1000) };
    assert_eq!(next_stop(VirtualTime(0), &b), VirtualTime(300));
}

#[test]
fn segments_are_bounded() {
    let b = StopBounds { max_segment: Some(VirtualDuration(100)), ..bounds(1000) };
    assert_eq!(next_stop(VirtualTime(250), &b), VirtualTime(350));
}

#[test]
fn frame_ticks_are_multiples_of_the_interval_rounded_up_to_whole_milliseconds() {
    let b = StopBounds { frame_interval: Some(VirtualDuration(16_667)), ..bounds(1_000_000) };
    assert_eq!(next_stop(VirtualTime(0), &b), VirtualTime(17_000));
    assert_eq!(next_stop(VirtualTime(16_667), &b), VirtualTime(17_000));
    assert_eq!(next_stop(VirtualTime(17_000), &b), VirtualTime(34_000));
    assert_eq!(next_stop(VirtualTime(20_000), &b), VirtualTime(34_000));
    assert_eq!(next_stop(VirtualTime(49_000), &b), VirtualTime(51_000));
    assert_eq!(frame_slot(VirtualTime(16_999), VirtualDuration(16_667)), 0);
    assert_eq!(frame_slot(VirtualTime(17_000), VirtualDuration(16_667)), 1);
    assert_eq!(frame_slot(VirtualTime(50_999), VirtualDuration(16_667)), 2);
    assert_eq!(frame_slot(VirtualTime(51_000), VirtualDuration(16_667)), 3);
}

#[test]
fn at_or_past_goal_stays() {
    assert_eq!(next_stop(VirtualTime(1000), &bounds(1000)), VirtualTime(1000));
    assert_eq!(next_stop(VirtualTime(1200), &bounds(1000)), VirtualTime(1200));
}

#[test]
fn quiescence_requires_idle_window_and_no_active_cause() {
    let mut q = Quiescence::new(VirtualDuration(250_000), VirtualTime(0));
    assert!(!q.is_quiescent(VirtualTime(100_000)));
    assert!(q.is_quiescent(VirtualTime(250_000)));
    q.begin("request:req-1", VirtualTime(260_000));
    assert!(!q.is_quiescent(VirtualTime(900_000)));
    assert_eq!(q.still_active(), vec!["request:req-1".to_string()]);
    q.end("request:req-1", VirtualTime(300_000));
    assert!(!q.is_quiescent(VirtualTime(549_999)));
    assert!(q.is_quiescent(VirtualTime(550_000)));
}

proptest! {
    #[test]
    fn next_stop_never_skips_agenda_time(
        now in 0u64..10_000,
        goal_off in 1u64..10_000,
        agenda in proptest::option::of(0u64..20_000),
        seg in proptest::option::of(1u64..5_000),
        frame in proptest::option::of(1u64..3_000),
    ) {
        let b = StopBounds {
            goal: VirtualTime(now + goal_off),
            agenda_next: agenda.map(VirtualTime),
            max_segment: seg.map(VirtualDuration),
            frame_interval: frame.map(VirtualDuration),
        };
        let stop = next_stop(VirtualTime(now), &b);
        if agenda.is_some_and(|a| a <= now) {
            prop_assert_eq!(stop, VirtualTime(now));
            return Ok(());
        }
        prop_assert!(stop > VirtualTime(now));
        prop_assert!(stop <= b.goal);
        if let Some(a) = agenda
            && a > now
        {
            prop_assert!(stop.0 <= a);
        }
        if let Some(s) = seg {
            prop_assert!(stop.0 <= now + s);
        }
    }

    /// Whole-millisecond bounds give whole-millisecond stops, whatever the frame interval, and every
    /// stop is at most the next frame tick: the run lands on each tick exactly.
    #[test]
    fn frame_ticks_keep_stops_on_whole_milliseconds(
        now_ms in 0u64..10_000,
        goal_off_ms in 1u64..10_000,
        agenda_ms in proptest::option::of(0u64..20_000),
        frame in 1u64..40_000,
    ) {
        let now = VirtualTime(now_ms * 1000);
        let interval = VirtualDuration(frame);
        let b = StopBounds {
            goal: VirtualTime((now_ms + goal_off_ms) * 1000),
            agenda_next: agenda_ms.map(|a| VirtualTime(a * 1000)),
            max_segment: None,
            frame_interval: Some(interval),
        };
        let stop = next_stop(now, &b);
        prop_assert_eq!(stop.0 % 1000, 0);
        let tick = frame_tick(frame_slot(now, interval) + 1, interval);
        prop_assert!(tick > now);
        prop_assert!(stop <= tick);
        prop_assert!(frame_slot(tick, interval) == frame_slot(now, interval) + 1);
    }
}

#[test]
fn an_agenda_event_due_now_stops_immediately() {
    let b = StopBounds { agenda_next: Some(VirtualTime(100)), ..bounds(1000) };
    assert_eq!(next_stop(VirtualTime(100), &b), VirtualTime(100));
    let b = StopBounds { agenda_next: Some(VirtualTime(50)), ..bounds(1000) };
    assert_eq!(next_stop(VirtualTime(100), &b), VirtualTime(100));
}

#[test]
fn settle_outcome_round_trips() {
    let o = SettleOutcome::BudgetExhausted { t: VirtualTime(5), still_active: vec!["timer".into()] };
    let back: SettleOutcome = serde_json::from_str(&serde_json::to_string(&o).unwrap()).unwrap();
    assert_eq!(back, o);
}
