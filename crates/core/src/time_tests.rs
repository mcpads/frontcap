use super::*;

#[test]
fn engine_milliseconds_round_to_microseconds() {
    assert_eq!(VirtualTime::from_engine_ms(0.0), VirtualTime(0));
    assert_eq!(VirtualTime::from_engine_ms(16.6667), VirtualTime(16_667));
    assert_eq!(VirtualTime::from_engine_ms(503.9000000059605), VirtualTime(503_900));
    assert_eq!(VirtualTime::from_engine_ms(-1.0), VirtualTime(0));
}

#[test]
fn arithmetic_saturates() {
    let t = VirtualTime(10);
    assert_eq!(t.saturating_add(VirtualDuration::from_ms(1)), VirtualTime(1010));
    assert_eq!(VirtualTime(5).since(VirtualTime(10)), VirtualDuration(0));
    assert_eq!(VirtualTime(10).since(VirtualTime(4)), VirtualDuration(6));
    assert_eq!(VirtualTime(u64::MAX).saturating_add(VirtualDuration(1)), VirtualTime(u64::MAX));
}

#[test]
fn serializes_as_plain_integers() {
    assert_eq!(serde_json::to_string(&VirtualTime(42)).unwrap(), "42");
    assert_eq!(serde_json::to_string(&VirtualDuration::from_micros(7)).unwrap(), "7");
}

#[test]
fn utc_iso_names_the_calendar_instant() {
    assert_eq!(utc_iso(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(utc_iso(946_684_800_500), "2000-01-01T00:00:00.500Z");
    assert_eq!(utc_iso(951_782_400_000), "2000-02-29T00:00:00.000Z");
    assert_eq!(utc_iso(1_792_659_600_000 + 3_723_004), "2026-10-22T10:02:03.004Z");
}
