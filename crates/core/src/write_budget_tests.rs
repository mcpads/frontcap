use super::*;

#[test]
fn competing_writers_share_spent_capacity_and_leave_retirement_available() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("budget");
    let owner = LaunchId::generate();
    let limits = Limits {
        ordinary: 12,
        retirement: 5,
    };
    let budget = WriteBudget::create(&directory, owner.clone(), limits).unwrap();
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let budget = budget.clone();
            std::thread::spawn(move || budget.charge(Class::Ordinary, 3).is_ok())
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .filter_map(|t| t.join().unwrap().then_some(()))
            .count(),
        4
    );
    drop(budget);
    let reopened = WriteBudget::open(&directory, owner.clone(), limits).unwrap();
    assert!(reopened.charge(Class::Ordinary, 1).is_err());
    reopened.charge(Class::Retirement, 5).unwrap();
    assert!(reopened.charge(Class::Retirement, 1).is_err());
    assert_eq!(reopened.charged().unwrap(), limits);
    assert!(WriteBudget::create(&directory, owner.clone(), limits).is_err());
    assert!(WriteBudget::open(&directory, LaunchId::generate(), limits).is_err());
    assert!(
        WriteBudget::open(
            &directory,
            owner,
            Limits {
                ordinary: 24,
                ..limits
            }
        )
        .is_err()
    );
}

#[test]
fn abandoned_charge_and_missing_ledger_do_not_replenish_capacity() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("budget");
    let owner = LaunchId::generate();
    let limits = Limits {
        ordinary: 9,
        retirement: 4,
    };
    let budget = WriteBudget::create(&directory, owner.clone(), limits).unwrap();
    // Serialization of this string is six bytes; missing parent makes publication fail.
    assert!(
        budget
            .save_atomic(Class::Ordinary, &root.path().join("absent/result"), &"test")
            .is_err()
    );
    drop(budget);
    let budget = WriteBudget::open(&directory, owner.clone(), limits).unwrap();
    assert!(budget.charge(Class::Ordinary, 4).is_err());
    assert!(budget.charge(Class::Ordinary, u64::MAX).is_err());
    budget.charge(Class::Ordinary, 3).unwrap();
    fs::remove_file(directory.join("ledger.json")).unwrap();
    assert!(budget.charge(Class::Retirement, 1).is_err());
    assert!(WriteBudget::open(&directory, owner, limits).is_err());
}
