use super::*;

#[test]
fn event_stream_fields_make_messages() {
    let mut p = SseParser::default();
    let mut out = p.feed(b": comment\ndata: one\n\nevent: tick\nid: 7\ndata: a\ndata: b\n\n");
    out.extend(p.feed(b"data: split"));
    out.extend(p.feed(b" late\r\n\r\n"));
    assert_eq!(out, vec![
        UpstreamEvent::Message { data: "one".into(), event: None, id: None },
        UpstreamEvent::Message { data: "a\nb".into(), event: Some("tick".into()), id: Some("7".into()) },
        UpstreamEvent::Message { data: "split late".into(), event: None, id: Some("7".into()) },
    ]);
}

#[test]
fn a_byte_order_mark_cr_line_ends_and_split_characters_are_read_as_the_format_says() {
    let mut p = SseParser::default();
    let text = "data: 안녕\r\rdata:\n\ndata: x\r\n\r\n";
    let mut bytes = b"\xEF\xBB\xBF".to_vec();
    bytes.extend_from_slice(text.as_bytes());
    // One byte at a time: the mark, the CR LF pairs and the Korean characters all split across reads.
    let out: Vec<UpstreamEvent> = bytes.iter().flat_map(|b| p.feed(&[*b])).collect();
    assert_eq!(out, vec![
        UpstreamEvent::Message { data: "안녕".into(), event: None, id: None },
        UpstreamEvent::Message { data: "".into(), event: None, id: None },
        UpstreamEvent::Message { data: "x".into(), event: None, id: None },
    ]);
}

/// A blank line ended by CR ends the event when it arrives, without waiting for a byte that may never come;
/// an LF after a CR, in the same read or the next, ends no second line.
#[test]
fn an_event_ended_by_cr_is_dispatched_at_once() {
    let mut p = SseParser::default();
    assert_eq!(p.feed(b"data: hello\r\r"), vec![UpstreamEvent::Message { data: "hello".into(), event: None, id: None }]);
    let mut out = p.feed(b"data: a\r");
    out.extend(p.feed(b"\n"));
    out.extend(p.feed(b"\r"));
    assert_eq!(out, vec![UpstreamEvent::Message { data: "a".into(), event: None, id: None }], "CR, then LF in the next read, is one line ending");
    let mut out = p.feed(b"\ndata: b\n\ndata: c\r\n\r\n");
    out.extend(p.feed(b"data: d\n\n"));
    let data: Vec<String> = out.into_iter().map(|e| match e { UpstreamEvent::Message { data, .. } => data, other => panic!("{other:?}") }).collect();
    assert_eq!(data, vec!["b", "c", "d"], "a stray LF after the CR blank line ends nothing");
}
