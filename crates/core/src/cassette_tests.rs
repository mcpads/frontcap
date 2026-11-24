use super::*;
use std::collections::BTreeSet;

fn line(url: &str, status: u16) -> Line {
    Line { requires_progress: false, key: Key::of("get", url, None), response: Response { status, headers: BTreeMap::new(), body_base64: String::new() }, latency: Latency { virtual_ms: 10, wall_ms: 3 } }
}

#[test]
fn urls_compare_with_their_query_sorted() {
    assert_eq!(normalize_url("https://a.test/x?b=2&a=1#top"), "https://a.test/x?a=1&b=2");
    assert_eq!(Key::of("get", "https://a.test/x?b=2&a=1", None), Key::of("GET", "https://a.test/x?a=1&b=2", Some(b"")));
    assert_ne!(Key::of("POST", "https://a.test/x", Some(b"1")), Key::of("POST", "https://a.test/x", Some(b"2")));
}

#[test]
fn repeated_names_keep_their_order_and_a_torn_last_line_is_dropped() {
    assert_eq!(normalize_url("https://a.test/x?b=1&a=2&a=1"), "https://a.test/x?a=2&a=1&b=1");
    let whole = line("https://a.test/n", 200).to_line();
    let text = format!("{whole}\n{}", &whole[..whole.len() / 2]);
    assert_eq!(parse(&text).unwrap().len(), 1);
    assert!(parse(&format!("{}\n{whole}\n", &whole[..10])).is_err(), "a broken line in the middle is corruption");
}

#[test]
fn repeated_keys_are_answered_in_recorded_order() {
    let lines = vec![line("https://a.test/n", 200), line("https://a.test/m", 201), line("https://a.test/n", 202)];
    let key = Key::of("GET", "https://a.test/n", None);
    let mut served = BTreeSet::new();
    assert_eq!(next(&lines, &key, &served, false), Some(0));
    served.insert(0);
    assert_eq!(next(&lines, &key, &served, false), Some(2));
    served.insert(2);
    assert_eq!(next(&lines, &key, &served, false), None);
    assert_eq!(next(&lines, &key, &served, true), Some(2), "the last again");
    let text: String = lines.iter().map(|l| l.to_line() + "\n").collect();
    assert_eq!(parse(&text).unwrap(), lines);
}


#[test]
fn old_cassettes_preserve_immediate_delivery_and_progress_boundaries_round_trip() {
    let legacy = r#"{"key":{"method":"GET","url_normalized":"https://a.test/n"},"response":{"status":200,"headers":{},"body_base64":""},"latency":{"virtual_ms":0,"wall_ms":0}}
"#;
    let mut response = parse(legacy).unwrap().remove(0);
    assert!(!response.requires_progress);
    response.requires_progress = true;
    assert_eq!(parse(&(response.to_line() + "\n")).unwrap(), vec![response]);
}
