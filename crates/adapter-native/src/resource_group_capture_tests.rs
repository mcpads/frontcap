use super::*;
#[test]
fn binary_and_diagnostic_budgets_are_independent_and_bounded() {
    let limits = remaining_artifact_limits(4096, 64*1024*1024-3, 8, 10).unwrap();
    assert_eq!(limits.stdout, 2); assert_eq!(limits.stderr, 3);
    assert!(remaining_artifact_limits(4096, 0, 10, 10).is_err());
    assert!(remaining_artifact_limits(4096, 64*1024*1024, 0, 10).is_err());
    assert!(remaining_output_limit(4096, 0).is_ok());
    assert_eq!(artifact_budget(None).unwrap(), DEFAULT_ARTIFACT_BUDGET);
    assert_eq!(artifact_budget(Some(123)).unwrap(), 123);
    assert!(artifact_budget(Some(0)).is_err());
    assert!(artifact_budget(Some(u64::MAX)).is_err());
}
#[test]
fn remaining_budget_admits_small_helpers_and_bounds_excess_output() {
    let root = tempfile::tempdir().unwrap();
    let retained = 64 * 1024 * 1024 - 20;
    let limit = remaining_output_limit(16 * 1024 * 1024, retained).unwrap();
    let mut small = Command::new("/bin/sh");
    small.args(["-c", "printf small; printf error >&2"]);
    let small_path = root.path().join("small");
    let result = run(&mut small,None,Instant::now()+Duration::from_secs(5),&AtomicBool::new(false),
        limit,&small_path,&mut BTreeMap::new()).unwrap();
    let exchange: crate::process::Exchange = crate::process::load_exchange(&small_path.join("exchange.json")).unwrap().unwrap();
    assert!(exchange.failure.is_none(),"{exchange:?}");
    assert_eq!(exchange.stdout,b"small");assert_eq!(exchange.stderr,b"error");
    assert_eq!(result.output_bytes,10);
    let mut excessive = Command::new("/bin/sh");
    excessive.args(["-c", "printf 123456789012345678901234567890"]);
    let excess_path = root.path().join("excessive");
    let result = run(&mut excessive,None,Instant::now()+Duration::from_secs(5),&AtomicBool::new(false),
        remaining_output_limit(16 * 1024 * 1024,retained+10).unwrap(),&excess_path,&mut BTreeMap::new()).unwrap();
    let exchange: crate::process::Exchange = crate::process::load_exchange(&excess_path.join("exchange.json")).unwrap().unwrap();
    assert!(exchange.failure.is_some());
    assert!(result.output_bytes<=10);
    assert!(exchange.cleanup_failure.is_none());
    assert!(remaining_output_limit(16 * 1024 * 1024,64 * 1024 * 1024).is_err());
    assert!(remaining_output_limit(0,0).is_err());
    assert!(remaining_output_limit(17 * 1024 * 1024,0).is_err());
}
