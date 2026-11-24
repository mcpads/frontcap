use super::*;

#[test]
fn recovery_retains_unbound_owners_and_rejects_conflicting_or_invalid_evidence() {
    let lineage = WorkerLineage { creator_document: "document".into(), constructors: vec![2, 1] };
    let place = DocumentPlace::Current { window: 0, frames: vec![1] };
    let entry = BindingEntry::WorkerRequestOwner { network_id: "request".into(), issued_us: 1000, lineage: lineage.clone(), document: Some(place.clone()) };
    assert_eq!(recovered_owners(&[entry.clone(), entry.clone()]).unwrap().get("request"), Some(&lineage));
    assert_eq!(recovered_issues(std::slice::from_ref(&entry)).unwrap().get("request"), Some(&VirtualTime(1000)));
    assert_eq!(recovered_places(&[entry.clone(), entry.clone()]).unwrap().get("request"), Some(&place));
    let changed_time = BindingEntry::WorkerRequestOwner { network_id: "request".into(), issued_us: 2000, lineage: lineage.clone(), document: Some(place.clone()) };
    assert!(recovered_issues(&[entry.clone(), changed_time]).is_err());
    let moved = BindingEntry::WorkerRequestOwner { network_id: "request".into(), issued_us: 1000, lineage: lineage.clone(), document: Some(DocumentPlace::Retired) };
    assert!(recovered_places(&[entry.clone(), moved]).is_err());
    let conflict = BindingEntry::WorkerRequestOwner { network_id: "request".into(), issued_us: 1000, lineage: WorkerLineage {
        constructors: vec![2, 2], ..lineage.clone()
    }, document: Some(place.clone()) };
    assert!(recovered_owners(&[entry, conflict]).is_err());
    let invalid = BindingEntry::WorkerRequestOwner { network_id: "request".into(), issued_us: 1000, lineage: WorkerLineage {
        constructors: vec![0], ..lineage
    }, document: None };
    assert!(recovered_owners(&[invalid]).is_err());
}

fn info(target: &str, parent: &str, document: Option<&str>, sequence: &str) -> target::TargetInfo {
    serde_json::from_value(serde_json::json!({
        "targetId":target,"parentId":parent,"type":"worker","title":"same.js","url":"https://example.test/same.js",
        "attached":true,"canAccessOpener":false,"frontcapWorkerSequence":sequence,"frontcapCreatorDocumentId":document
    })).unwrap()
}

#[test]
fn ancestry_uses_constructor_sequences_even_when_child_metadata_arrives_first() {
    let mut book = WorkerLineages::default();
    book.remember("child-session", &info("child", "parent", None, "3")).unwrap();
    assert_eq!(book.for_session("child-session").unwrap(), None);
    book.remember("parent-session", &info("parent", "page", Some("document"), "2")).unwrap();
    let expected = Some(WorkerLineage { creator_document: "document".into(), constructors: vec![2, 3] });
    assert_eq!(book.for_session("child-session").unwrap(), expected);
    assert_eq!(book.for_target("child").unwrap(), expected);
    book.remember("replacement", &info("child", "parent", None, "3")).unwrap();
    assert_eq!(book.for_session("replacement").unwrap(), expected);
    assert_eq!(book.for_session("child-session").unwrap(), expected);
}

#[test]
fn retiring_live_routes_keeps_the_issued_requests_constructor_evidence() {
    let mut book = WorkerSessions::default();
    book.lineages.remember("parent-session", &info("parent", "page", Some("document"), "2")).unwrap();
    book.lineages.remember("child-session", &info("child", "parent", None, "1")).unwrap();
    book.parents.insert("child-session".into(), "parent-session".into());
    book.requests.insert("network-request".into(), "child-session".into());
    assert_eq!(book.forget("parent-session"), vec!["parent-session", "child-session"]);
    assert!(book.requests.is_empty());
    assert_eq!(book.lineages.for_session("child-session").unwrap(), Some(WorkerLineage {
        creator_document: "document".into(), constructors: vec![2, 1],
    }));
}

#[test]
fn issuers_order_the_main_thread_then_workers_by_document_then_constructor_path() {
    let worker = |window, frames: &[u32], constructors: &[u64]| Issuer::Worker {
        document: DocumentPlace::Current { window, frames: frames.to_vec() }, constructors: constructors.to_vec(),
    };
    let mut issuers = vec![
        Issuer::UnresolvedWorker,
        Issuer::Worker { document: DocumentPlace::Retired, constructors: vec![1] },
        worker(1, &[], &[1]),
        worker(0, &[0], &[1]),
        worker(0, &[], &[2]),
        worker(0, &[], &[1, 1]),
        worker(0, &[], &[1]),
        Issuer::MainThread,
    ];
    issuers.sort();
    assert_eq!(issuers, vec![
        Issuer::MainThread,
        worker(0, &[], &[1]),
        worker(0, &[], &[1, 1]),
        worker(0, &[], &[2]),
        worker(0, &[0], &[1]),
        worker(1, &[], &[1]),
        Issuer::Worker { document: DocumentPlace::Retired, constructors: vec![1] },
        Issuer::UnresolvedWorker,
    ]);
}

#[test]
fn conflicting_or_malformed_native_identity_never_becomes_an_ordering_key() {
    let mut book = WorkerLineages::default();
    book.remember("a", &info("a", "b", None, "1")).unwrap();
    book.remember("b", &info("b", "a", None, "1")).unwrap();
    assert!(book.for_target("a").is_err());
    assert!(book.remember("a", &info("a", "b", None, "2")).is_err());
    book.remember("bad", &info("bad", "page", Some("document"), "0")).unwrap();
    assert!(book.for_target("bad").is_err());
    book.remember("overflow", &info("overflow", "page", Some("document"), "18446744073709551616")).unwrap();
    assert!(book.for_target("overflow").is_err());
}
