use super::*;
use frontcap_core::{ids::LaunchId, write_budget::{Class, Limits, WriteBudget}};

#[test]
fn refused_capture_creates_no_file_and_failed_publication_never_refunds() {
    let root = tempfile::tempdir().unwrap();
    let budget = WriteBudget::create(&root.path().join("budget"), LaunchId::new("g-capture").unwrap(),
        Limits { ordinary: 8, retirement: 4 }).unwrap();
    let original = root.path().join("original.png");
    save_capture(&original, b"1234", Some(&budget)).unwrap();
    // Existing evidence is never overwritten; even failed publication consumes admitted bytes.
    assert!(save_capture(&original, b"5678", Some(&budget)).is_err());
    assert_eq!(std::fs::read(&original).unwrap(), b"1234");
    let refused = root.path().join("refused.png");
    assert!(save_capture(&refused, b"x", Some(&budget)).is_err());
    assert!(!refused.exists());
    assert_eq!(budget.charged().unwrap().ordinary, 8);
    budget.charge(Class::Retirement, 4).unwrap();
}
