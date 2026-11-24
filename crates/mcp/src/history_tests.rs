use super::*;
use serde_json::json;

fn event(seq:u64,t:u64)->serde_json::Value {
    json!({"seq":seq,"t_us":t,"kind":"console","payload":{"level":"log","text":"event"}})
}

#[test]
fn pages_bind_published_prefix_and_report_torn_tail_without_repair() {
    let root=tempfile::tempdir().unwrap();
    let mut bytes=Vec::new();
    for (seq,t) in [(0,10),(1,10),(2,20)] {
        bytes.extend_from_slice(format!("{}\n",event(seq,t)).as_bytes());
    }
    let hash=sha256_bytes_hex(&bytes);
    bytes.extend_from_slice(b"{\"seq\":3,\"text\":\"\xe2");
    std::fs::write(root.path().join("trace.jsonl"),&bytes).unwrap();
    let first=page(root.path(),None,Some(2)).unwrap();
    assert_eq!(first.events.iter().map(|e|e.seq).collect::<Vec<_>>(),vec![0,1]);
    assert_eq!(first.next_after,Some(1));assert_eq!(first.last_seq,Some(2));
    assert_eq!(first.complete_prefix_sha256,hash);assert!(first.incomplete_tail);
    let last=page(root.path(),first.next_after,Some(2)).unwrap();
    assert_eq!(last.events[0].seq,2);assert_eq!(last.next_after,None);
    assert_eq!(std::fs::read(root.path().join("trace.jsonl")).unwrap(),bytes);
    assert!(page(root.path(),None,Some(0)).is_err());
}

#[test]
fn complete_corruption_and_sequence_gaps_refuse() {
    let root=tempfile::tempdir().unwrap();
    for text in ["corrupt\n".to_string(),format!("{}\n{}\n",event(0,10),event(2,20)),format!("{}\n{}\n",event(0,10),event(1,9))] {
        std::fs::write(root.path().join("trace.jsonl"),text).unwrap();
        assert!(read(root.path()).is_err());
    }
}
