use super::*;
use crate::time::VirtualTime;
use proptest::prelude::*;

#[test]
fn pops_due_events_in_time_order() {
    let mut a = Agenda::new();
    a.schedule(VirtualTime(30), "c");
    a.schedule(VirtualTime(10), "a");
    a.schedule(VirtualTime(20), "b");
    assert_eq!(a.next_time(), Some(VirtualTime(10)));
    assert_eq!(a.pop_due(VirtualTime(20)), vec![(VirtualTime(10), "a"), (VirtualTime(20), "b")]);
    assert_eq!(a.len(), 1);
}

#[test]
fn same_instant_keeps_registration_order() {
    let mut a = Agenda::new();
    a.schedule(VirtualTime(5), "first");
    a.schedule(VirtualTime(5), "second");
    assert_eq!(a.pop_due(VirtualTime(5)), vec![(VirtualTime(5), "first"), (VirtualTime(5), "second")]);
}

#[test]
fn cancel_removes_one_event() {
    let mut a = Agenda::new();
    let k = a.schedule(VirtualTime(5), "x");
    a.schedule(VirtualTime(6), "y");
    assert_eq!(a.cancel(k), Some("x"));
    assert_eq!(a.cancel(k), None);
    assert_eq!(a.next_time(), Some(VirtualTime(6)));
}

proptest! {
    #[test]
    fn pop_order_is_time_then_registration(times in proptest::collection::vec(0u64..50, 0..40)) {
        let mut a = Agenda::new();
        for (i, t) in times.iter().enumerate() {
            a.schedule(VirtualTime(*t), i);
        }
        let popped: Vec<usize> = a.pop_due(VirtualTime(u64::MAX)).into_iter().map(|(_, i)| i).collect();
        let mut expected: Vec<usize> = (0..times.len()).collect();
        expected.sort_by_key(|i| (times[*i], *i));
        prop_assert_eq!(popped, expected);
    }
}

#[test]
fn agenda_round_trips_and_keeps_registration_order() {
    let mut a = Agenda::new();
    a.schedule(VirtualTime(5), "first".to_string());
    let k = a.schedule(VirtualTime(5), "second".to_string());
    a.schedule(VirtualTime(1), "early".to_string());
    a.cancel(k);
    let mut restored: Agenda<String> = serde_json::from_str(&serde_json::to_string(&a).unwrap()).unwrap();
    restored.schedule(VirtualTime(5), "third".to_string());
    let order: Vec<String> = restored.pop_due(VirtualTime(10)).into_iter().map(|(_, e)| e).collect();
    assert_eq!(order, vec!["early", "first", "third"]);
}
